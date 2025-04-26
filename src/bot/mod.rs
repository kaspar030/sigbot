use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use log::{debug, trace};
use once_cell::sync::Lazy;
use presage::{libsignal_service::content::Content, proto::AttachmentPointer, store::Thread};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::quick_hash::QuickHash;
use crate::signal::{SignalHandle, SignalMsgHandler};

use whisper_simple::{transcribe, WhisperAudio, WhisperModel};

trait ThreadConfig {
    fn get(&self) -> anyhow::Result<BotThreadState>;
    fn put(&self, config: BotThreadState) -> anyhow::Result<()>;
}

impl ThreadConfig for Thread {
    fn get(&self) -> anyhow::Result<BotThreadState> {
        fn default(thread: &Thread) -> BotThreadState {
            let mut config = BotThreadState::default();
            if let Thread::Group(_) = thread {
                config.enabled = false;
            }
            debug!("default config for {:?}: {:#?}", thread, &config);
            config
        }

        let tx = DB.tx(true)?;
        let bucket = tx.get_or_create_bucket("threads")?;
        let res = bucket
            .get(&bincode::serialize(&self)?)
            .map_or_else(
                || Ok(default(self)),
                |kv| {
                    if let jammdb::Data::KeyValue(kv) = kv {
                        let config = bincode::deserialize(kv.value())?;
                        debug!("loaded config for {:?}: {:#?}", &self, &config);
                        return Ok(config);
                    }
                    unreachable!();
                },
            )
            .unwrap_or_else(|e: anyhow::Error| {
                debug!(
                    "error loading config for {:?}: {:#?} (using default)",
                    self, e
                );
                default(self)
            });

        tx.commit()?;
        Ok(res)
    }

    fn put(&self, config: BotThreadState) -> anyhow::Result<()> {
        debug!("storing config for {:?}: {:#?}", &self, &config);
        let tx = DB.tx(true)?;
        let bucket = tx.get_or_create_bucket("threads")?;
        bucket.put(bincode::serialize(&self)?, bincode::serialize(&config)?)?;
        tx.commit()?;
        Ok(())
    }
}

static DB: Lazy<jammdb::DB> = Lazy::new(|| {
    let path = shellexpand::tilde("~/.config/sigbot/tester/threads.db").into_owned();
    jammdb::DB::open(path).expect("opening thread config db")
});

#[allow(clippy::upper_case_acronyms)]
enum AudioType {
    AAC,
    M4A,
}

#[derive(Default, Copy, Clone, Debug, Hash, PartialEq, Serialize, Deserialize)]
struct BotThreadState {
    enabled: bool,
    whisper_model: WhisperModel,
}

struct BotState {}

impl BotState {
    pub fn new() -> Self {
        Self {}
    }
}

pub struct Bot {
    //state: Arc<Mutex<BotState>>,
}

impl Bot {
    pub fn new() -> Self {
        Bot {
            //state: Arc::new(Mutex::new(BotState::new())),
        }
    }

    async fn process_audio_attachment(
        &self,
        signal: &SignalHandle,
        thread_state: &BotThreadState,
        _content: &Content,
        attachment_pointer: &AttachmentPointer,
    ) -> anyhow::Result<String> {
        let Ok(attachment_data) = signal.get_attachment(attachment_pointer).await else {
            return Err(anyhow!("failed to fetch attachment"));
        };

        debug!(
            "processing audio attachment with size {}",
            attachment_data.len()
        );

        let model = thread_state.whisper_model;

        let audio_type = {
            if let Some(file_name) = &attachment_pointer.file_name {
                if file_name.ends_with(".m4a") {
                    AudioType::M4A
                } else {
                    AudioType::AAC
                }
            } else {
                AudioType::AAC
            }
        };

        let audio_data = tokio::task::spawn_blocking(move || match audio_type {
            AudioType::AAC => WhisperAudio::from_aac(&attachment_data),
            AudioType::M4A => WhisperAudio::from_m4a(&attachment_data),
        })
        .await??;

        transcribe(model, audio_data, None).await
    }

    async fn handle_attachments(
        &self,
        signal: &SignalHandle,
        thread_state: &BotThreadState,
        content: &Content,
        attachments: &Vec<AttachmentPointer>,
    ) -> anyhow::Result<()> {
        debug!("datamessage, attachments:{}", attachments.len());
        for attachment in attachments {
            match attachment.content_type() {
                "audio/aac" => {
                    debug!("Content-type: audio/aac");
                    signal.react(content, "🦻").await?;
                    let res = self
                        .process_audio_attachment(signal, thread_state, content, attachment)
                        .await;

                    debug!("audio attachment processed");

                    match res {
                        Ok(string) => {
                            signal
                                .quote(content, &format!("TRANSCRIPT:\n{string}"))
                                .await?
                        }
                        Err(err) => debug!("{}", err),
                    }
                    debug!("replied");
                }
                _ => trace!("unhandled content type {}", attachment.content_type()),
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SignalMsgHandler for Bot {
    async fn handle(&self, content: &Content, signal: &mut SignalHandle) {
        use presage::libsignal_service::content::{ContentBody, DataMessage, SyncMessage};
        use presage::libsignal_service::proto::sync_message::Sent;
        use presage::store::Thread;

        let Ok(thread) = Thread::try_from(content) else {
            log::warn!("failed to derive thread from content");
            return;
        };

        let mut thread_config = thread.get().unwrap();
        let thread_config_hash = thread_config.quick_hash();

        match &content.body {
            ContentBody::SynchronizeMessage(SyncMessage {
                sent:
                    Some(Sent {
                        message:
                            Some(DataMessage {
                                body: Some(body),
                                quote: _,
                                ..
                            }),
                        ..
                    }),
                ..
            }) => {
                let mut handled = true;
                debug!("body=\"{body}\"");
                match body.as_str() {
                    "/bot ping" => {
                        debug!("ping");
                        let msg_text = {
                            format!(
                                "pong{}",
                                if !thread_config.enabled {
                                    " (disabled)"
                                } else {
                                    ""
                                }
                            )
                        };
                        signal.quote(content, &msg_text).await.unwrap();
                    }
                    "/bot enable" => {
                        debug!("enable");
                        signal.react(content, "👍").await.unwrap();
                        thread_config.enabled = true;
                    }
                    // TODO: this needs deduplication
                    "/bot model tiny" => {
                        debug!("set model tiny");
                        signal.react(content, "👍").await.unwrap();
                        //thread_config.whisper_model = WhisperModel::Tiny;
                    }
                    "/bot model base" => {
                        debug!("set model base");
                        signal.react(content, "👍").await.unwrap();
                        //thread_config.whisper_model = WhisperModel::Base;
                    }
                    "/bot model small" => {
                        debug!("set model small");
                        signal.react(content, "👍").await.unwrap();
                        //thread_config.whisper_model = WhisperModel::Small;
                    }
                    "/bot model medium" => {
                        debug!("set model medium");
                        //thread_config.whisper_model = WhisperModel::Medium;
                    }
                    "/bot model large" => {
                        debug!("set model large");
                        signal.react(content, "👍").await.unwrap();
                        //thread_config.whisper_model = WhisperModel::Large;
                    }
                    "/bot disable" => {
                        debug!("disable");
                        signal.react(content, "👍").await.unwrap();
                        //thread_config.enabled = false;
                    }
                    _ => {
                        handled = false;
                    }
                };

                if handled {
                    if thread_config.quick_hash() != thread_config_hash {
                        thread.put(thread_config).expect("saving thread config");
                    }
                    return;
                }
            }
            ContentBody::SynchronizeMessage(SyncMessage {
                sent:
                    Some(Sent {
                        message:
                            Some(DataMessage {
                                body: None,
                                attachments,
                                ..
                            }),
                        ..
                    }),
                ..
            })
            | ContentBody::DataMessage(DataMessage {
                body: None,
                attachments,
                ..
            }) => {
                self.handle_attachments(signal, &thread_config, content, attachments)
                    .await
                    .unwrap();
            }
            _ => {
                debug!("skipping message");
            }
        }
    }
}
