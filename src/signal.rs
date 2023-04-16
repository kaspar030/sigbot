use anyhow::Context;
use camino::Utf8PathBuf;
use presage::{
    prelude::{Contact, Content, ContentBody, DataMessage, SignalServers, Uuid},
    Manager, Registered, Thread,
};
use presage_store_sled::{MigrationConflictStrategy, SledStore};

use async_trait::async_trait;

use futures::{channel::oneshot, future, pin_mut, StreamExt};
use log::{debug, error, info};
use std::time::UNIX_EPOCH;

use std::sync::{Arc, Mutex};

#[async_trait(?Send)]
pub trait SignalMsgHandler {
    async fn handle(&mut self, signal: &Signal, content: &Content);
}

pub struct Signal {
    device_name: String,
    db_path: Utf8PathBuf,
    servers: SignalServers,
    manager: Option<Manager<SledStore, Registered>>,
    handlers: Vec<Arc<Mutex<Box<dyn SignalMsgHandler>>>>,
}

impl Signal {
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
            manager: None,
            handlers: Vec::new(),
        })
    }

    fn config_store(&self) -> anyhow::Result<SledStore> {
        debug!("opening config database from {}", self.db_path);
        SledStore::open_with_passphrase(&self.db_path, Some(""), MigrationConflictStrategy::Raise)
            .map_err(|err| err.into())
    }

    pub async fn link(&mut self) -> anyhow::Result<()> {
        let config_store = self.config_store()?;

        let (provisioning_link_tx, provisioning_link_rx) = oneshot::channel();
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
                let uuid = manager.whoami().await.unwrap().uuid;
                println!("{uuid:?}");
                self.manager = Some(manager);
                Ok(())
            }
            (Err(err), _) => Err(err.into()),
        }
    }

    pub async fn open(&mut self) -> anyhow::Result<()> {
        let config_store = self.config_store()?;
        let manager = Manager::load_registered(config_store)?;
        self.manager = Some(manager);
        Ok(())
    }

    pub fn register_handler(&mut self, handler: Box<dyn SignalMsgHandler>) {
        self.handlers.push(Arc::new(Mutex::new(handler)));
    }

    pub async fn process_messages(&mut self) -> anyhow::Result<()> {
        let mut manager = self.manager.as_ref().unwrap().clone();
        let messages = manager
            .receive_messages()
            .await
            .context("failed to initialize messages stream")?;

        pin_mut!(messages);

        while let Some(content) = messages.next().await {
            for handler in self.handlers.iter() {
                let mut handler = handler.lock().unwrap();
                handler.handle(self, &content).await;
            }
        }

        Ok(())
    }

    pub async fn send(&self, msg: &str, uuid: &Uuid) -> anyhow::Result<()> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        let message = ContentBody::DataMessage(DataMessage {
            body: Some(msg.to_string()),
            timestamp: Some(timestamp),
            ..Default::default()
        });

        let m = self.manager.as_ref().unwrap();
        let mut m = m.clone();
        m.send_message(*uuid, message, timestamp).await?;

        Ok(())
    }

    pub async fn reply(&self, thread: &Thread, msg: &str) -> anyhow::Result<()> {
        match thread {
            Thread::Contact(uuid) => {
                self.send(msg, uuid).await?;
            }
            Thread::Group(_bytes) => {
                debug!("reply to group not implemented");
            }
        }
        Ok(())
    }

    pub fn manager(&self) -> &Manager<SledStore, Registered> {
        self.manager.as_ref().unwrap()
    }
}
