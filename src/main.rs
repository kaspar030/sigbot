use anyhow::Result;
use clap::{crate_version, Arg, Command};
use env_logger::Env;
use log::{debug, info};

mod bot;
mod debug;
mod quick_hash;
mod signal;
use signal::SignalConfig;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    fn profile() -> Arg {
        Arg::new("profile")
            .help("profile to use")
            .default_value("default")
            .env("SIGBOT_PROFILE")
            .value_parser(clap::value_parser!(String))
    }

    let matches = Command::new("SigBot")
        .version(crate_version!())
        .author("Kaspar Schleiser <kaspar@schleiser.de>")
        .about("A signal bot")
        .infer_subcommands(true)
        .subcommand(Command::new("run").about("Runs the program").arg(profile()))
        .subcommand(
            Command::new("link")
                .about("Create new device link")
                .arg(profile()),
        )
        .get_matches();

    match matches.subcommand() {
        Some(("run", matches)) => {
            let profile: &String = matches.get_one("profile").unwrap();
            run_command(profile).await
        }
        Some(("link", matches)) => {
            let profile: &String = matches.get_one("profile").unwrap();
            link_command(profile).await
        }
        _ => Err(anyhow::anyhow!("Invalid command")),
    }
}

async fn run_command(profile: &str) -> anyhow::Result<()> {
    env_logger::Builder::from_env(
        Env::default().default_filter_or(format!("{}=info", env!("CARGO_PKG_NAME"))),
    )
    .init();

    info!("starting");

    let signal = SignalConfig::new(profile)?
        .register_handler(Box::new(bot::Bot::new()))
        .run(signal::timestamp())?;

    signal.run().await?;

    Ok(())
}

async fn link_command(profile: &str) -> anyhow::Result<()> {
    SignalConfig::new(profile)?.link().await
}
