use std::collections::HashMap;

use anyhow::anyhow;
use async_trait::async_trait;
use env_logger::Env;
use log::{debug, trace};

mod bot;
mod debug;
mod signal;
use signal::{SignalConfig, SignalHandle, SignalMsgHandler};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(
        Env::default().default_filter_or(format!("{}=warn", env!("CARGO_PKG_NAME"))),
    )
    .init();

    let signal = SignalConfig::new("tester")?
        //.register_handler(Box::new(debug::DebugHandler {}))
        .register_handler(Box::new(bot::Bot::new()))
        .run(signal::timestamp())?;

    signal.run().await?;

    Ok(())
}
