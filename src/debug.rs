use async_trait::async_trait;
use presage::prelude::{Content, ContentBody};

use crate::{
    debug,
    signal::{SignalHandle, SignalMsgHandler},
};

pub struct DebugHandler {}

#[async_trait]
impl SignalMsgHandler for DebugHandler {
    async fn handle(&self, content: &Content, _signal: &mut SignalHandle) {
        let mut content = content.clone();
        if let ContentBody::SynchronizeMessage(ref mut msg) = content.body {
            msg.padding = None;
            if let Some(ref mut sent) = msg.sent {
                if let Some(ref mut message) = sent.message {
                    message.profile_key = None;
                }
            }
        }

        debug!("DebugHandler: {:#?}", content);
    }
}
