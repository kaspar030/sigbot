use anyhow::Result;
use clap::{crate_version, Command};
use env_logger::Env;
use log::debug;

mod bot;
mod debug;
mod signal;
use signal::SignalConfig;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let matches = Command::new("SigBot")
        .version(crate_version!())
        .author("Kaspar Schleiser <kaspar@schleiser.de>")
        .about("A signal bot")
        .infer_subcommands(true)
        .subcommand(Command::new("run").about("Runs the program"))
        .subcommand(Command::new("link").about("Create new device link"))
        .get_matches();

    match matches.subcommand() {
        Some(("run", matches)) => run_command().await,
        Some(("link", matches)) => link_command().await,
        _ => Err(anyhow::anyhow!("Invalid command")),
    }
}

async fn run_command() -> anyhow::Result<()> {
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

async fn link_command() -> anyhow::Result<()> {
    SignalConfig::new("tester")?.link().await
}
