use std::{collections::HashMap, sync::Arc};

use anyhow::anyhow;
use async_trait::async_trait;
use log::{debug, trace};
use once_cell::sync::Lazy;
use presage::{
    prelude::{proto::AttachmentPointer, Content},
    Thread,
};
use tokio::sync::Mutex;
use whisper_rs::WhisperContext;

use crate::signal::{SignalHandle, SignalMsgHandler};

mod resample;

static MODEL_TINY: Lazy<WhisperContext> =
    Lazy::new(|| WhisperContext::new("models/ggml-tiny.bin").expect("opening model file"));

static MODEL_BASE: Lazy<WhisperContext> =
    Lazy::new(|| WhisperContext::new("models/ggml-base.bin").expect("opening model file"));

static MODEL_SMALL: Lazy<WhisperContext> =
    Lazy::new(|| WhisperContext::new("models/ggml-small.bin").expect("opening model file"));

static MODEL_MEDIUM: Lazy<WhisperContext> =
    Lazy::new(|| WhisperContext::new("models/ggml-medium.bin").expect("opening model file"));

static MODEL_LARGE: Lazy<WhisperContext> =
    Lazy::new(|| WhisperContext::new("models/ggml-large.bin").expect("opening model file"));

#[allow(clippy::upper_case_acronyms)]
enum AudioType {
    AAC,
    M4A,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Hash)]
enum WhisperModel {
    Tiny,
    Base,
    Small,
    Medium,
    #[default]
    Large,
}

impl WhisperModel {
    fn instance(&self) -> &Lazy<WhisperContext> {
        match self {
            WhisperModel::Tiny => &MODEL_TINY,
            WhisperModel::Base => &MODEL_BASE,
            WhisperModel::Small => &MODEL_SMALL,
            WhisperModel::Medium => &MODEL_MEDIUM,
            WhisperModel::Large => &MODEL_LARGE,
        }
    }
}

#[derive(Default, Copy, Clone, Debug, Hash, PartialEq)]
struct BotThreadState {
    enabled: bool,
    whisper_model: WhisperModel,
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

pub struct Bot {
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
        content: &Content,
        attachment_pointer: &AttachmentPointer,
    ) -> anyhow::Result<String> {
        use redlux::Decoder;
        let thread = Thread::try_from(content).expect("extracting thread");

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

        let model = {
            let mut state = self.state.lock().await;
            state.threads.entry(thread).or_default().whisper_model
        };

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

        let (tx, rx) = flume::unbounded();

        std::thread::spawn(move || {
            let file_size = attachment_data.len() as u64;
            let decoder = {
                match audio_type {
                    AudioType::AAC => Decoder::new_aac(std::io::Cursor::new(attachment_data)),
                    AudioType::M4A => {
                        let dec =
                            Decoder::new_mpeg4(std::io::Cursor::new(attachment_data), file_size);
                        match dec {
                            Err(_) => {
                                tx.send(Ok("[invalid file]".to_string())).unwrap();
                                return;
                            }
                            Ok(dec) => dec,
                        }
                    }
                }
            };

            //        let decoded: Vec<f32> = decoder.map(|sample| sample as f32 / 32768.0).collect();
            let decoded: Vec<i16> = decoder.collect();

            use whisper_rs::*;

            let decoded = resample::resample(&decoded[..], 16000);
            let audio_len = std::time::Duration::from_millis((decoded.len() / 16) as u64);
            debug!("audio length: {:.2?}", audio_len);

            debug!("loading model...");
            let model = model.instance();

            let state = &mut model
                .create_state()
                .expect("failed to create whisper state");

            debug!("loading model done, starting inference");
            let mut params = FullParams::new(SamplingStrategy::default());
            params.set_n_threads(num_cpus::get_physical() as i32);
            params.set_translate(false);
            params.set_language(Some("auto"));

            // let model_context = ctx
            //     .tokenize("Die Sprache ist Deutsch.", 64)
            //     .expect("tokenizing context");
            // params.set_tokens(&model_context);

            let start = std::time::Instant::now();

            state
                .full(params, &decoded[..])
                .expect("failed to run model");
            // fetch the results
            let num_segments = state.full_n_segments().expect("getting segments");
            if num_segments == 0 {
                debug!("no voice found");
                tx.send(Ok("[no voice found]".to_string())).unwrap();
            } else {
                let mut out = Vec::new();
                for i in 0..num_segments {
                    let segment = state
                        .full_get_segment_text(i)
                        .expect("failed to get segment");
                    let start_timestamp = state.full_get_segment_t0(i).expect("getting segment t0");
                    let end_timestamp = state.full_get_segment_t1(i).expect("getting segment t1");
                    debug!("[{} - {}]: {}", start_timestamp, end_timestamp, segment);
                    out.push(segment);
                }
                let transcription = out.join("\n");
                let inference_time = start.elapsed();
                debug!(
                    "inference done. took {:.2?} (ratio: {:.2?})",
                    inference_time,
                    inference_time.as_secs_f64() / audio_len.as_secs_f64()
                );
                tx.send(Ok(transcription)).unwrap();
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
        debug!("datamessage, attachments:{}", attachments.len());
        for attachment in attachments {
            match attachment.content_type() {
                "audio/aac" => {
                    debug!("Content-type: audio/aac");
                    signal.react(content, "🦻").await?;
                    let res = self
                        .process_audio_attachment(signal, content, attachment)
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

        async fn thread_set_model(
            state: &Arc<Mutex<BotState>>,
            thread: &Thread,
            model: WhisperModel,
        ) {
            let mut state = state.lock().await;
            state
                .threads
                .entry(thread.clone())
                .or_default()
                .whisper_model = model;
        }

        let thread_enabled = thread_get_state(&self.state, &thread).await;

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
                        signal.quote(content, "pong").await.unwrap();
                    }
                    "/bot enable" => {
                        debug!("enable");
                        signal.react(content, "👍").await.unwrap();
                        if !thread_enabled {
                            thread_set_state(&self.state, &thread, true).await;
                        }
                    }
                    // TODO: this needs deduplication
                    "/bot model tiny" => {
                        debug!("set model tiny");
                        signal.react(content, "👍").await.unwrap();
                        thread_set_model(&self.state, &thread, WhisperModel::Tiny).await;
                    }
                    "/bot model base" => {
                        debug!("set model base");
                        signal.react(content, "👍").await.unwrap();
                        thread_set_model(&self.state, &thread, WhisperModel::Base).await;
                    }
                    "/bot model small" => {
                        debug!("set model small");
                        signal.react(content, "👍").await.unwrap();
                        thread_set_model(&self.state, &thread, WhisperModel::Small).await;
                    }
                    "/bot model medium" => {
                        debug!("set model medium");
                        signal.react(content, "👍").await.unwrap();
                        thread_set_model(&self.state, &thread, WhisperModel::Medium).await;
                    }
                    "/bot model large" => {
                        debug!("set model large");
                        signal.react(content, "👍").await.unwrap();
                        thread_set_model(&self.state, &thread, WhisperModel::Large).await;
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
                debug!("skipping message");
            }
        }
    }
}
