use std::collections::HashMap;

use anyhow::anyhow;
use async_trait::async_trait;
use env_logger::Env;
use log::debug;
use presage::{
    prelude::{proto::AttachmentPointer, Content},
    Manager, Registered, Thread,
};

use std::sync::Arc;
use tokio::sync::Mutex;

mod signal;
use signal::{SignalConfig, SignalHandle, SignalMsgHandler};

mod resample;

struct DebugHandler {}

#[async_trait]
impl SignalMsgHandler for DebugHandler {
    async fn handle(&self, content: &Content, signal: &mut SignalHandle) {
        debug!("DebugHandler: {:#?}", content);
    }
}

#[derive(Default, Copy, Clone, Debug, Hash, PartialEq)]
struct BotThreadState {
    enabled: bool,
}

struct BotState {
    threads: HashMap<Thread, BotThreadState>,
    whisper_sema: tokio::sync::Semaphore,
}

impl BotState {
    pub fn new() -> Self {
        Self {
            threads: HashMap::new(),
            whisper_sema: tokio::sync::Semaphore::new(1),
        }
    }
}

struct Bot {
    state: Arc<Mutex<BotState>>,
}

impl Bot {
    pub fn new() -> Self {
        Bot {
            state: Arc::new(Mutex::new(BotState::new())),
        }
    }

    async fn process_audio_attachment(
        &self,
        signal: &SignalHandle,
        attachment_pointer: &AttachmentPointer,
    ) -> anyhow::Result<String> {
        use redlux::Decoder;
        let Ok(attachment_data) = signal.get_attachment(attachment_pointer).await else {
                log::warn!("failed to fetch attachment");
                return Err(anyhow!("failed to fetch attachment"));
            };

        debug!(
            "processing AAC attachment with size {}",
            attachment_data.len()
        );

        // limit calls to whisper-rs
        // let _sema = {
        //     let state = self.state.lock().await;
        //     state.whisper_sema.acquire().await.unwrap()
        // };

        let (tx, rx) = flume::unbounded();

        std::thread::spawn(move || {
            let decoder = Decoder::new_aac(std::io::Cursor::new(attachment_data));

            //        let decoded: Vec<f32> = decoder.map(|sample| sample as f32 / 32768.0).collect();
            let decoded: Vec<i16> = decoder.collect();

            use whisper_rs::*;

            let decoded = resample::resample(&decoded[..], 16000);
            debug!("number of samples {}", decoded.len());

            debug!("loading model...");
            let mut ctx =
                WhisperContext::new("models/ggml-medium.bin").expect("opening model file");
            debug!("loading model done, starting inference");
            let mut params = FullParams::new(SamplingStrategy::default());
            params.set_translate(true);

            ctx.full(params, &decoded[..]).expect("failed to run model");
            // fetch the results
            let num_segments = ctx.full_n_segments();
            if num_segments == 0 {
                tx.send(Err(anyhow!("no whisper segments"))).unwrap();
            } else {
                let mut out = Vec::new();
                for i in 0..num_segments {
                    let segment = ctx.full_get_segment_text(i).expect("failed to get segment");
                    let start_timestamp = ctx.full_get_segment_t0(i);
                    let end_timestamp = ctx.full_get_segment_t1(i);
                    debug!("[{} - {}]: {}", start_timestamp, end_timestamp, segment);
                    out.push(segment);
                }
                debug!("inference done");
                tx.send(Ok(out.join("\n"))).unwrap();
            }
        });

        rx.recv_async().await.unwrap()
    }

    async fn handle_attachments(
        &self,
        signal: &SignalHandle,
        content: &Content,
        attachments: &Vec<AttachmentPointer>,
    ) -> anyhow::Result<()> {
        debug!("Sigbot: datamessage, attachments:{}", attachments.len());
        let thread = Thread::try_from(content).unwrap();

        for attachment in attachments {
            match attachment.content_type() {
                "audio/aac" => {
                    debug!("Content-type: audio/aac");
                    signal.react(content, "🦻").await?;
                    let res = self.process_audio_attachment(signal, attachment).await;
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
                _ => debug!("unhandled content type {}", attachment.content_type()),
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SignalMsgHandler for Bot {
    async fn handle(&self, content: &Content, signal: &mut SignalHandle) {
        use presage::libsignal_service::proto::sync_message::Sent;
        use presage::prelude::{ContentBody, DataMessage, SyncMessage};
        use presage::Thread;

        let Ok(thread) = Thread::try_from(content) else {
            log::warn!("failed to derive thread from content");
            return;
        };

        async fn thread_get_state(state: &Arc<Mutex<BotState>>, thread: &Thread) -> bool {
            let mut state = state.lock().await;
            state.threads.entry(thread.clone()).or_default().enabled
        }

        async fn thread_set_state(state: &Arc<Mutex<BotState>>, thread: &Thread, enabled: bool) {
            let mut state = state.lock().await;
            state.threads.entry(thread.clone()).or_default().enabled = enabled;
        }

        let thread_enabled = thread_get_state(&self.state, &thread).await;

        debug!(
            "Sigbot: {:?} {:?}",
            &thread,
            thread_get_state(&self.state, &thread).await
        );

        match &content.body {
            ContentBody::SynchronizeMessage(SyncMessage {
                sent:
                    Some(Sent {
                        message:
                            Some(DataMessage {
                                body: Some(body),
                                quote,
                                ..
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
                        signal.quote(&content, "pong").await.unwrap();
                    }
                    "/bot enable" => {
                        debug!("enable");
                        signal.react(content, "👍").await.unwrap();
                        if !thread_enabled {
                            thread_set_state(&self.state, &thread, true).await;
                        }
                    }
                    "/bot disable" => {
                        debug!("disable");
                        signal.react(content, "👍").await.unwrap();
                        if thread_enabled {
                            thread_set_state(&self.state, &thread, false).await;
                        }
                    }
                    _ => {
                        handled = false;
                    }
                };

                // if let Some(quote) = quote {
                //     debug!("quote: {:#?}", quote);
                // }

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
                self.handle_attachments(signal, content, attachments)
                    .await
                    .unwrap();
            }
            ContentBody::DataMessage(DataMessage {
                body: None,
                attachments,
                ..
            }) => {
                self.handle_attachments(signal, content, attachments)
                    .await
                    .unwrap();
            }
            _ => {
                debug!("Sigbot: unhandled {:#?}", content);
            }
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(
        Env::default().default_filter_or(format!("{}=warn", env!("CARGO_PKG_NAME"))),
    )
    .init();

    let signal = SignalConfig::new("tester")?
        //        .register_handler(Box::new(DebugHandler {}))
        .register_handler(Box::new(Bot::new()))
        .run(signal::timestamp())?;

    signal.run().await?;

    Ok(())
}
