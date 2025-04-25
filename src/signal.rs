use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use camino::Utf8PathBuf;
use futures::{future, pin_mut, StreamExt};
use log::{debug, info, warn};
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::{
    content::{Content, ContentBody, DataMessage, GroupContextV2, Reaction, SyncMessage},
    proto::{data_message::Quote, sync_message::Sent, AttachmentPointer},
};
use presage::{manager::Registered, store::Thread, Manager};
use presage_store_sqlite::SqliteStore;
use tokio::sync::Mutex;

#[async_trait]
pub trait SignalMsgHandler {
    async fn handle(&self, content: &Content, signal: &mut SignalHandle);
}

#[derive(Debug, Clone)]
pub enum SignalMsg {
    Received(Content),
    Send(Thread, ContentBody),
    ManagerRequest(ManagerRequest, flume::Sender<ManagerReply>),
    //    ManagerReply(ManagerReply),
    Err(String),
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

    async fn send_message(&self, thread: Thread, message: ContentBody) -> anyhow::Result<()> {
        self.out_chan_tx
            .send_async(SignalMsg::Send(thread.clone(), message))
            .await?;
        Ok(())
    }

    pub(crate) async fn send(&self, thread: &Thread, body: &str) -> anyhow::Result<()> {
        let message = ContentBody::DataMessage(DataMessage {
            body: Some(String::from(body)),
            timestamp: Some(timestamp()),
            ..Default::default()
        });
        self.send_message(thread.clone(), message).await
    }

    pub(crate) async fn react(&self, content: &Content, emoji: &str) -> anyhow::Result<()> {
        let thread = Thread::try_from(content).unwrap();

        let datamessage = content
            .datamessage()
            .context(anyhow!("extracting datamessage for react()"))?;

        let message = ContentBody::DataMessage(DataMessage {
            body: None,
            timestamp: Some(timestamp()),
            reaction: Some(Reaction {
                emoji: Some(String::from(emoji)),
                target_author_aci: Some(content.metadata.sender.service_id_string()),
                target_sent_timestamp: Some(datamessage.timestamp()),
                ..Default::default()
            }),
            ..Default::default()
        });

        self.send_message(thread, message).await
    }

    pub(crate) async fn quote(&self, content: &Content, body: &str) -> anyhow::Result<()> {
        let thread = Thread::try_from(content).unwrap();

        let message = ContentBody::DataMessage(DataMessage {
            body: Some(body.into()),
            timestamp: Some(timestamp()),
            quote: Some(Quote {
                author_aci: Some(content.metadata.sender.service_id_string()),
                id: Some(content.metadata.timestamp),
                ..Default::default()
            }),
            ..Default::default()
        });

        self.send_message(thread, message).await
    }

    pub async fn get_attachment(
        &self,
        attachment_pointer: &AttachmentPointer,
    ) -> anyhow::Result<Vec<u8>> {
        let data = self
            .manager_request(ManagerRequest::GetAttachment(attachment_pointer.clone()))
            .await?;
        let ManagerReply::Attachment(data) = data;
        Ok(data)
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

    async fn open_config_store(&self) -> Result<SqliteStore, anyhow::Error> {
        SqliteStore::open_with_passphrase(
            self.db_path.as_str(),
            None,
            presage_store_sqlite::OnNewIdentity::Reject,
        )
        .await
        .with_context(|| format!("failed to open sqlite data storage at: {}", self.db_path))
    }

    pub async fn link(&mut self) -> anyhow::Result<()> {
        let config_store = self.open_config_store().await?;

        let (provisioning_link_tx, provisioning_link_rx) =
            futures::channel::oneshot::channel::<url::Url>();
        let manager = future::join(
            Manager::link_secondary_device(
                config_store,
                self.servers,
                self.device_name.clone(),
                provisioning_link_tx,
            ),
            async move {
                match provisioning_link_rx.await {
                    Ok(url) => qr2term::print_qr(url.to_string()).expect("failed to render qrcode"),
                    Err(e) => log::error!("Error linking device: {e}"),
                }
            },
        )
        .await;

        match manager {
            (Ok(manager), _) => {
                let uuid = manager.whoami().await.unwrap().aci;
                info!("Linking successful. UUID={uuid:?}");
                Ok(())
            }
            (Err(err), _) => Err(err.into()),
        }
    }

    pub fn register_handler(mut self, handler: Box<dyn SignalMsgHandler + Send>) -> Self {
        self.handlers.push(Arc::new(Mutex::new(handler)));
        self
    }

    pub async fn run(self, replay_timestamp: u64) -> anyhow::Result<Signal> {
        let (in_chan_tx, in_chan_rx) = flume::bounded(1024);
        let (out_chan_tx, out_chan_rx) = flume::bounded(1024);

        let config_store = self.open_config_store().await?;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        std::thread::spawn(move || {
            let local = tokio::task::LocalSet::new();
            let incoming_manager = rt
                .block_on(Manager::load_registered(config_store))
                .expect("loaded config store");

            let outgoing_manager = incoming_manager.clone();

            local.spawn_local(SignalConfig::incoming_signal_task(
                incoming_manager,
                in_chan_tx,
                replay_timestamp,
            ));
            local.spawn_local(SignalConfig::outgoing_signal_task(
                outgoing_manager,
                out_chan_rx,
            ));
            rt.block_on(local)
        });

        let signal = Signal {
            in_chan_rx,
            out_chan_tx,
            handlers: self.handlers,
        };

        Ok(signal)
    }

    async fn incoming_signal_task<T: presage::store::Store>(
        mut manager: Manager<T, Registered>,
        in_chan_tx: flume::Sender<SignalMsg>,
        replay_timestamp: u64,
    ) -> anyhow::Result<()> {
        debug!("launching incoming signal message task");
        //        let mut manager = self.manager.unwrap();
        loop {
            let messages = match manager
                .receive_messages()
                .await
                .context("failed to initialize messages stream")
            {
                Ok(x) => x,
                Err(e) => {
                    in_chan_tx.send_async(SignalMsg::Err(e.to_string())).await?;
                    return Err(e);
                }
            };

            pin_mut!(messages);

            use presage::model::messages::Received;

            while let Some(content) = messages.next().await {
                match content {
                    Received::QueueEmpty => {}
                    Received::Contacts => {}
                    Received::Content(content) => {
                        if content.metadata.timestamp < replay_timestamp {
                            continue;
                        }

                        debug!("incoming signal message task: msg received");
                        if in_chan_tx
                            .send_async(SignalMsg::Received(*content))
                            .await
                            .is_err()
                        {
                            debug!("incoming signal message task exiting");
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    async fn outgoing_signal_task<T: presage::store::Store>(
        mut manager: Manager<T, Registered>,
        out_chan_rx: flume::Receiver<SignalMsg>,
    ) {
        async fn handle_manager_request<T: presage::store::Store>(
            manager: &mut Manager<T, Registered>,
            req: ManagerRequest,
            tx: flume::Sender<ManagerReply>,
        ) {
            match req {
                ManagerRequest::GetAttachment(attachment_pointer) => {
                    if let Ok(data) = manager.get_attachment(&attachment_pointer).await {
                        tx.send_async(ManagerReply::Attachment(data))
                            .await
                            .expect("sending ManagerRequest");
                    } else {
                        drop(tx)
                    }
                }
            }
        }

        debug!("outgoing signal message task started");
        while let Ok(msg) = out_chan_rx.recv_async().await {
            match msg {
                SignalMsg::Send(thread, message) => {
                    debug!("send()");

                    if let Err(e) = match thread {
                        Thread::Contact(uuid) => manager
                            .send_message(ServiceId::Aci(uuid.into()), message, timestamp())
                            .await
                            .map_err(|e| e.into()),
                        Thread::Group(master_key_bytes) => {
                            if let ContentBody::DataMessage(mut message) = message {
                                message.group_v2 = Some(GroupContextV2 {
                                    master_key: Some(master_key_bytes.to_vec()),
                                    revision: Some(2),
                                    ..Default::default()
                                });
                                manager
                                    .send_message_to_group(&master_key_bytes, message, timestamp())
                                    .await
                                    .map_err(|e| e.into())
                            } else {
                                Err(anyhow!("unexpected SignalMsg::Send group format"))
                            }
                        }
                    } {
                        warn!("sending message: {e}");
                    }
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
            loop {
                match in_chan_rx.recv_async().await {
                    Ok(msg) => {
                        debug!("incoming msg");
                        match msg {
                            SignalMsg::Received(msg) => {
                                for handler in &handlers {
                                    let handler = handler.clone();
                                    let handler = handler.lock().await;
                                    let f = handler.handle(&msg, &mut signal_handle);
                                    f.await;
                                }
                            }
                            SignalMsg::Err(s) => {
                                warn!("Signal::run() incoming error: {s}");
                                break;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        debug!("Signal::run() RecvError: {e}");
                        break;
                    }
                }
            }
            debug!("Signal::run() end");
        });

        rx_dispatch.await?;

        Ok(())
    }
}

pub fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64
}

trait GetDataMessage {
    fn datamessage(&self) -> Option<&DataMessage>;
}

impl GetDataMessage for Content {
    fn datamessage(&self) -> Option<&DataMessage> {
        match &self.body {
            ContentBody::SynchronizeMessage(SyncMessage {
                sent:
                    Some(Sent {
                        message: Some(datamessage),
                        ..
                    }),
                ..
            })
            | ContentBody::DataMessage(datamessage) => Some(datamessage),
            _ => None,
        }
    }
}
