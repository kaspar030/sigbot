use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use camino::Utf8PathBuf;
use futures::{pin_mut, StreamExt};
use log::debug;
use presage::prelude::content::Reaction;
use presage::prelude::proto::data_message::Quote;
use presage::prelude::proto::AttachmentPointer;
use presage::prelude::{Content, ContentBody, DataMessage, SignalServers, Uuid};
use presage::{Manager, Registered, Thread};
use presage_store_sled::{MigrationConflictStrategy, SledStore};
use tokio::sync::Mutex;

#[async_trait]
pub trait SignalMsgHandler {
    async fn handle(&self, content: &Content, signal: &mut SignalHandle);
}

#[derive(Debug, Clone)]
pub enum SignalMsg {
    Received(Content),
    Send(Uuid, ContentBody),
    ManagerRequest(ManagerRequest, flume::Sender<ManagerReply>),
    ManagerReply(ManagerReply),
}

#[derive(Debug, Clone)]
pub enum ManagerRequest {
    GetAttachment(AttachmentPointer),
}

#[derive(Debug, Clone)]
pub enum ManagerReply {
    Attachment(Vec<u8>),
}

pub struct Signal {
    out_chan_tx: flume::Sender<SignalMsg>,
    in_chan_rx: flume::Receiver<SignalMsg>,
    handlers: Vec<Arc<Mutex<Box<dyn SignalMsgHandler + Send>>>>,
}

pub struct SignalHandle {
    out_chan_tx: flume::Sender<SignalMsg>,
}

impl SignalHandle {
    fn new(out_chan_tx: flume::Sender<SignalMsg>) -> Self {
        Self { out_chan_tx }
    }

    pub(crate) async fn send(&self, uuid: &Uuid, body: &str) -> anyhow::Result<()> {
        let message = ContentBody::DataMessage(DataMessage {
            body: Some(String::from(body)),
            timestamp: Some(timestamp()),
            ..Default::default()
        });
        self.out_chan_tx
            .send_async(SignalMsg::Send(*uuid, message))
            .await?;
        Ok(())
    }

    pub(crate) async fn react(&self, content: &Content, emoji: &str) -> anyhow::Result<()> {
        let thread = Thread::try_from(content).unwrap();

        let message = ContentBody::DataMessage(DataMessage {
            body: None,
            timestamp: Some(timestamp()),
            reaction: Some(Reaction {
                emoji: Some(String::from(emoji)),
                target_author_uuid: Some(content.metadata.sender.uuid.to_string()),
                target_sent_timestamp: Some(content.metadata.timestamp),
                ..Default::default()
            }),
            ..Default::default()
        });

        if let Thread::Contact(uuid) = thread {
            self.out_chan_tx
                .send_async(SignalMsg::Send(uuid, message))
                .await?;
        }

        Ok(())
    }

    pub(crate) async fn quote(&self, content: &Content, body: &str) -> anyhow::Result<()> {
        let thread = Thread::try_from(content).unwrap();

        let message = ContentBody::DataMessage(DataMessage {
            body: Some(body.into()),
            timestamp: Some(timestamp()),
            quote: Some(Quote {
                author_uuid: Some(content.metadata.sender.uuid.to_string()),
                id: Some(content.metadata.timestamp),
                ..Default::default()
            }),
            ..Default::default()
        });

        if let Thread::Contact(uuid) = thread {
            self.out_chan_tx
                .send_async(SignalMsg::Send(uuid, message))
                .await?;
        }

        Ok(())
    }

    pub async fn reply(&self, thread: &Thread, msg: &str) -> anyhow::Result<()> {
        match thread {
            Thread::Contact(uuid) => {
                self.send(uuid, msg).await?;
            }
            Thread::Group(_bytes) => {
                debug!("reply to group not implemented");
            }
        }
        Ok(())
    }

    pub async fn get_attachment(
        &self,
        attachment_pointer: &AttachmentPointer,
    ) -> anyhow::Result<Vec<u8>> {
        let data = self
            .manager_request(ManagerRequest::GetAttachment(attachment_pointer.clone()))
            .await?;
        if let ManagerReply::Attachment(data) = data {
            Ok(data)
        } else {
            Err(anyhow!("get_attachment() invalid reply"))
        }
    }

    pub async fn manager_request(&self, req: ManagerRequest) -> anyhow::Result<ManagerReply> {
        let (tx, rx) = flume::unbounded();
        self.out_chan_tx
            .send_async(SignalMsg::ManagerRequest(req, tx))
            .await?;
        Ok(rx.recv_async().await?)
    }
}

pub struct SignalConfig {
    device_name: String,
    db_path: Utf8PathBuf,
    servers: SignalServers,
    //manager: Option<Manager<SledStore, Registered>>,
    handlers: Vec<Arc<Mutex<Box<dyn SignalMsgHandler + Send>>>>,
}

impl SignalConfig {
    pub fn new(device_name: &str) -> anyhow::Result<Self> {
        let path_base = shellexpand::tilde("~/.config/sigbot").into_owned();
        let mut db_path = Utf8PathBuf::from(path_base);
        std::fs::create_dir_all(&db_path).with_context(|| format!("creating {db_path}"))?;
        db_path.push(device_name);

        let device_name = String::from(device_name);

        Ok(Self {
            device_name,
            db_path,
            servers: SignalServers::Production,
            handlers: Vec::new(),
        })
    }

    // pub async fn link(&mut self) -> anyhow::Result<()> {
    //     let config_store = self.config_store()?;

    //     let (provisioning_link_tx, provisioning_link_rx) = oneshot::channel();
    //     let manager = future::join(
    //         Manager::link_secondary_device(
    //             config_store,
    //             self.servers,
    //             self.device_name.clone(),
    //             provisioning_link_tx,
    //         ),
    //         async move {
    //             match provisioning_link_rx.await {
    //                 Ok(url) => qr2term::print_qr(url.to_string()).expect("failed to render qrcode"),
    //                 Err(e) => log::error!("Error linking device: {e}"),
    //             }
    //         },
    //     )
    //     .await;

    //     match manager {
    //         (Ok(manager), _) => {
    //             let uuid = manager.whoami().await.unwrap().uuid;
    //             println!("{uuid:?}");
    //             self.manager = Some(manager);
    //             Ok(())
    //         }
    //         (Err(err), _) => Err(err.into()),
    //     }
    // }

    pub fn register_handler(mut self, handler: Box<dyn SignalMsgHandler + Send>) -> Self {
        self.handlers.push(Arc::new(Mutex::new(handler)));
        self
    }

    pub fn run(self) -> anyhow::Result<Signal> {
        debug!("opening config database from {}", self.db_path);
        let config_store = SledStore::open_with_passphrase(
            &self.db_path,
            Some(""),
            MigrationConflictStrategy::Raise,
        )?;

        let incoming_manager = Manager::load_registered(config_store)?;
        let outgoing_manager = incoming_manager.clone();

        let (in_chan_tx, in_chan_rx) = flume::bounded(1024);
        let (out_chan_tx, out_chan_rx) = flume::bounded(1024);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        std::thread::spawn(move || {
            let local = tokio::task::LocalSet::new();
            local.spawn_local(SignalConfig::incoming_signal_task(
                incoming_manager,
                in_chan_tx,
            ));
            local.spawn_local(SignalConfig::outgoing_signal_task(
                outgoing_manager,
                out_chan_rx,
            ));
            rt.block_on(local);
        });

        let signal = Signal {
            in_chan_rx,
            out_chan_tx,
            handlers: self.handlers,
        };

        Ok(signal)
    }

    async fn incoming_signal_task(
        mut manager: Manager<SledStore, Registered>,
        in_chan_tx: flume::Sender<SignalMsg>,
    ) {
        debug!("launching incoming signal message task");
        //        let mut manager = self.manager.unwrap();
        let messages = manager
            .receive_messages()
            .await
            .context("failed to initialize messages stream")
            .unwrap();

        pin_mut!(messages);

        while let Some(content) = messages.next().await {
            debug!("incoming signal message task: msg received");
            in_chan_tx.send_async(SignalMsg::Received(content)).await;
        }
    }

    async fn outgoing_signal_task(
        mut manager: Manager<SledStore, Registered>,
        out_chan_rx: flume::Receiver<SignalMsg>,
    ) {
        async fn handle_manager_request(
            manager: &mut Manager<SledStore, Registered>,
            req: ManagerRequest,
            tx: flume::Sender<ManagerReply>,
        ) {
            match req {
                ManagerRequest::GetAttachment(attachment_pointer) => {
                    if let Ok(data) = manager.get_attachment(&attachment_pointer).await {
                        tx.send_async(ManagerReply::Attachment(data)).await.unwrap();
                    } else {
                        drop(tx)
                    }
                }
            }
        }

        debug!("outgoing signal message task started");
        while let Ok(msg) = out_chan_rx.recv_async().await {
            match msg {
                SignalMsg::Send(uuid, message) => {
                    debug!("send()");

                    manager
                        .send_message(uuid, message, timestamp())
                        .await
                        .unwrap();
                }
                SignalMsg::ManagerRequest(req, tx) => {
                    handle_manager_request(&mut manager, req, tx).await;
                }
                other => {
                    debug!("outgoing signal message task: unexpected msg {:?}", other);
                }
            }
        }
        debug!("outgoing signal message task ended");
    }
}

impl Signal {
    pub async fn run(&self) -> anyhow::Result<()> {
        let in_chan_rx = self.in_chan_rx.clone();
        let out_chan_tx = self.out_chan_tx.clone();
        let handlers = self.handlers.clone();
        let rx_dispatch = tokio::spawn(async move {
            let mut signal_handle = SignalHandle::new(out_chan_tx);
            while let Ok(msg) = in_chan_rx.recv_async().await {
                debug!("incoming msg");
                if let SignalMsg::Received(msg) = msg {
                    for handler in &handlers {
                        let handler = handler.clone();
                        let handler = handler.lock().await;
                        let f = handler.handle(&msg, &mut signal_handle);
                        f.await;
                    }
                }
            }
        });

        rx_dispatch.await?;

        Ok(())
    }
}

fn timestamp() -> u64 {
    let timestamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64;
    timestamp
}
