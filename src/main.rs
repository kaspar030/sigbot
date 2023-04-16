use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use env_logger::Env;

mod signal;
use futures::future::{BoxFuture, FutureExt};
use log::debug;
use presage::{
    prelude::{Content, Uuid},
    Manager, Registered, Thread,
};
use presage_store_sled::SledStore;
use signal::Signal;

fn handle_message(manager: &Manager<SledStore, Registered>, content: &Content) {
    debug!("handling message");
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

    let state = Arc::new(Mutex::new(BotState::new()));

    signal.register_handler(print_message, state).await.unwrap();
}

#[derive(Default, Copy, Clone, Debug, Hash, PartialEq)]
struct BotThreadState {
    enabled: bool,
}

#[derive(Clone, Debug)]
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

fn print_message(
    manager: &Manager<SledStore, Registered>,
    content: &Content,
    state: &Arc<Mutex<BotState>>,
) {
    use presage::libsignal_service::content::Reaction;
    use presage::libsignal_service::proto::data_message::Quote;
    use presage::libsignal_service::proto::sync_message::Sent;
    use presage::prelude::{ContentBody, DataMessage, SyncMessage};
    use presage::Thread;

    let Ok(thread) = Thread::try_from(content) else {
        log::warn!("failed to derive thread from content");
        return;
    };

    {
        let mut state = state.lock().unwrap();
        let thread_state = state.threads.entry(thread.clone()).or_default();
        debug!("Sigbot: {:?} {:?}", &thread, thread_state);
        match &content.body {
            ContentBody::SynchronizeMessage(SyncMessage {
                sent:
                    Some(Sent {
                        message:
                            Some(DataMessage {
                                body: Some(body), ..
                            }),
                        destination_uuid: Some(destination_uuid),
                        ..
                    }),
                ..
            }) => {
                debug!("Sigbot: body=\"{body}\"");
                if body == "/bot ping" {
                    debug!("pong");
                } else if body == "/bot enable" {
                    debug!("enable");
                    thread_state.enabled = true;
                } else if body == "/bot disable" {
                    debug!("disable");

                    let uuid = content.metadata.sender.uuid;
                    //                    manager.send_message(uuid, "foo".to_string(), 0);
                    thread_state.enabled = false;
                }
            }
            _ => {
                debug!("Sigbot: ignored");
                return;
            }
        }
    }

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
        DataMessage { attachments, .. } => Some(format!("attachments: {}", attachments.len())),
        //_ => Some("Empty data message".to_string()),
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

        // if false {
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
