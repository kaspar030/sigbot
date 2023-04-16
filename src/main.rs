use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::anyhow;
use env_logger::Env;

mod signal;
use async_trait::async_trait;
use log::debug;
use presage::{
    prelude::{proto::AttachmentPointer, Content},
    Manager, Registered, Thread,
};
use signal::{Signal, SignalMsgHandler};

mod resample;

struct DebugHandler {}

#[async_trait(?Send)]
impl SignalMsgHandler for DebugHandler {
    async fn handle(&mut self, _signal: &Signal, _content: &Content) {
        debug!("handling message");
    }
}

struct PrintMsgHandler {}
#[async_trait(?Send)]
impl SignalMsgHandler for PrintMsgHandler {
    async fn handle(&mut self, signal: &Signal, content: &Content) {
        let manager = signal.manager();
        print_message(manager, content);
    }
}

#[derive(Default, Copy, Clone, Debug, Hash, PartialEq)]
struct BotThreadState {
    enabled: bool,
}

struct BotState {
    threads: HashMap<Thread, BotThreadState>,
}

impl BotState {
    pub fn new() -> Self {
        Self {
            threads: HashMap::new(),
        }
    }
}

struct Bot {
    state: BotState,
}

impl Bot {
    pub fn new() -> Self {
        Bot {
            state: BotState::new(),
        }
    }

    async fn process_audio_attachment(
        &mut self,
        signal: &Signal,
        attachment_pointer: &AttachmentPointer,
    ) -> anyhow::Result<String> {
        use redlux::Decoder;
        let Ok(attachment_data) = signal.manager().get_attachment(attachment_pointer).await else {
                log::warn!("failed to fetch attachment");
                return Err(anyhow!("failed to fetch attachment"));
            };

        debug!(
            "processing AAC attachment with size {}",
            attachment_data.len()
        );

        let decoder = Decoder::new_aac(std::io::Cursor::new(attachment_data));

        //        let decoded: Vec<f32> = decoder.map(|sample| sample as f32 / 32768.0).collect();
        let decoded: Vec<i16> = decoder.collect();

        use whisper_rs::*;

        let decoded = resample::resample(&decoded[..], 16000);
        debug!("number of samples {}", decoded.len());

        debug!("loading model...");
        let mut ctx = WhisperContext::new("models/ggml-medium.bin").expect("opening model file");
        debug!("loading model done");
        let params = FullParams::new(SamplingStrategy::default());
        ctx.full(params, &decoded[..]).expect("failed to run model");
        // fetch the results
        let num_segments = ctx.full_n_segments();
        let mut out = Vec::new();
        for i in 0..num_segments {
            let segment = ctx.full_get_segment_text(i).expect("failed to get segment");
            let start_timestamp = ctx.full_get_segment_t0(i);
            let end_timestamp = ctx.full_get_segment_t1(i);
            debug!("[{} - {}]: {}", start_timestamp, end_timestamp, segment);
            out.push(segment);
        }
        Ok(out.join("\n"))
    }

    async fn handle_attachments(
        &mut self,
        signal: &Signal,
        thread: Thread,
        attachments: &Vec<AttachmentPointer>,
    ) -> anyhow::Result<()> {
        debug!("Sigbot: datamessage, attachments:{}", attachments.len());
        for attachment in attachments {
            match attachment.content_type() {
                "audio/aac" => {
                    debug!("Content-type: audio/aac");
                    let res = self.process_audio_attachment(signal, attachment).await;

                    match res {
                        Ok(string) => signal.reply(&thread, &string).await?,
                        Err(err) => debug!("{}", err),
                    }
                }
                _ => debug!("unhandled content type {}", attachment.content_type()),
            }
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl SignalMsgHandler for Bot {
    async fn handle(&mut self, signal: &Signal, content: &Content) {
        use presage::libsignal_service::proto::sync_message::Sent;
        use presage::prelude::{ContentBody, DataMessage, SyncMessage};
        use presage::Thread;

        let Ok(thread) = Thread::try_from(content) else {
            log::warn!("failed to derive thread from content");
            return;
        };

        let thread_state = self.state.threads.entry(thread.clone()).or_default();
        debug!("Sigbot: {:?} {:?}", &thread, thread_state);
        match &content.body {
            ContentBody::SynchronizeMessage(SyncMessage {
                sent:
                    Some(Sent {
                        message:
                            Some(DataMessage {
                                body: Some(body), ..
                            }),
                        ..
                    }),
                ..
            }) => {
                let mut handled = true;
                debug!("Sigbot: body=\"{body}\"");
                match body.as_str() {
                    "/bot ping" => {
                        debug!("ping");
                        signal.reply(&thread, "pong").await.unwrap();
                    }
                    "/bot enable" => {
                        debug!("enable");
                        if thread_state.enabled {
                            signal.reply(&thread, "bot already enabled").await.unwrap();
                        } else {
                            thread_state.enabled = true;
                            signal.reply(&thread, "bot enabled").await.unwrap();
                        }
                    }
                    "/bot disable" => {
                        debug!("disable");
                        if thread_state.enabled {
                            thread_state.enabled = false;

                            signal.reply(&thread, "bot disabled").await.unwrap();
                        }
                    }
                    _ => {
                        handled = false;
                    }
                };

                if handled {
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
            }) => {
                self.handle_attachments(signal, thread, attachments)
                    .await
                    .unwrap();
            }
            ContentBody::DataMessage(DataMessage {
                body: None,
                attachments,
                ..
            }) => {
                self.handle_attachments(signal, thread, attachments)
                    .await
                    .unwrap();
            }
            _ => {
                debug!("Sigbot: unhandled {:#?}", content);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    env_logger::from_env(
        Env::default().default_filter_or(format!("{}=warn", env!("CARGO_PKG_NAME"))),
    )
    .init();

    let mut signal = Signal::new("tester").unwrap();
    //signal.link().await.unwrap();
    signal.open().await.unwrap();

    //    let state = Arc::new(Mutex::new(BotState::new()));
    signal.register_handler(Box::new(PrintMsgHandler {}));
    signal.register_handler(Box::new(Bot::new()));

    signal.process_messages().await.unwrap();
}

fn print_message<C: presage::Store>(manager: &Manager<C, Registered>, content: &Content) {
    use presage::libsignal_service::content::Reaction;
    use presage::libsignal_service::proto::data_message::Quote;
    use presage::libsignal_service::proto::sync_message::Sent;
    use presage::prelude::{ContentBody, DataMessage, SyncMessage};
    use presage::Thread;

    let Ok(thread) = Thread::try_from(content) else {
        log::warn!("failed to derive thread from content");
        return;
    };

    let format_data_message = |thread: &Thread, data_message: &DataMessage| match data_message {
        DataMessage {
            quote:
                Some(Quote {
                    text: Some(quoted_text),
                    ..
                }),
            body: Some(body),
            ..
        } => Some(format!("Answer to message \"{quoted_text}\": {body}")),
        DataMessage {
            reaction:
                Some(Reaction {
                    target_sent_timestamp: Some(timestamp),
                    emoji: Some(emoji),
                    ..
                }),
            ..
        } => {
            let Ok(Some(message)) = manager.message(thread, *timestamp) else {
                log::warn!("no message in {thread} sent at {timestamp}");
                return None;
            };

            let ContentBody::DataMessage(DataMessage { body: Some(body), .. }) = message.body else {
                log::warn!("message reacted to has no body");
                return None;
            };

            Some(format!("Reacted with {emoji} to message: \"{body}\""))
        }
        DataMessage {
            body: Some(body), ..
        } => Some(body.to_string()),
        _ => Some("Empty data message".to_string()),
    };

    let format_contact = |uuid| {
        manager
            .contact_by_id(uuid)
            .ok()
            .flatten()
            .filter(|c| !c.name.is_empty())
            .map(|c| c.name)
            .unwrap_or_else(|| uuid.to_string())
    };

    let format_group = |key| {
        manager
            .group(key)
            .ok()
            .flatten()
            .map(|g| g.title)
            .unwrap_or_else(|| "<missing group>".to_string())
    };

    enum Msg<'a> {
        Received(&'a Thread, String),
        Sent(&'a Thread, String),
    }

    if let Some(msg) = match &content.body {
        ContentBody::NullMessage(_) => Some(Msg::Received(
            &thread,
            "Null message (for example deleted)".to_string(),
        )),
        ContentBody::DataMessage(data_message) => {
            format_data_message(&thread, data_message).map(|body| Msg::Received(&thread, body))
        }
        ContentBody::SynchronizeMessage(SyncMessage {
            sent:
                Some(Sent {
                    message: Some(data_message),
                    ..
                }),
            ..
        }) => format_data_message(&thread, data_message).map(|body| Msg::Sent(&thread, body)),
        ContentBody::CallMessage(_) => Some(Msg::Received(&thread, "is calling!".into())),
        ContentBody::TypingMessage(_) => Some(Msg::Received(&thread, "is typing...".into())),
        c => {
            log::warn!("unsupported message {c:?}");
            None
        }
    } {
        let ts = content.metadata.timestamp;
        let (prefix, body) = match msg {
            Msg::Received(Thread::Contact(sender), body) => {
                let contact = format_contact(sender);
                (format!("From {contact} @ {ts}: "), body)
            }
            Msg::Sent(Thread::Contact(recipient), body) => {
                let contact = format_contact(recipient);
                (format!("To {contact} @ {ts}"), body)
            }
            Msg::Received(Thread::Group(key), body) => {
                let sender = content.metadata.sender.uuid;
                let group = format_group(key);
                (format!("From {sender} to group {group} @ {ts}: "), body)
            }
            Msg::Sent(Thread::Group(key), body) => {
                let group = format_group(key);
                (format!("To group {group} @ {ts}"), body)
            }
        };

        println!("{prefix} / {body}");

        // if notifications {
        //     if let Err(e) = Notification::new()
        //         .summary(&prefix)
        //         .body(&body)
        //         .icon("presage")
        //         .show()
        //     {
        //         log::error!("failed to display desktop notification: {e}");
        //     }
        // }
    }
}
