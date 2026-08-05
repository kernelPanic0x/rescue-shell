// src/main.rs
mod common;
mod completer;
mod helper;
mod protocol;
mod victim;

use std::str::FromStr;

use clap::{Args, Parser, Subcommand};
use magic_wormhole::Code;

#[derive(Parser)]
#[command(about = "Remote rescue shell over magic-wormhole")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Serve {
        #[arg(long)]
        mirror: bool,

        #[command(flatten)]
        common: CommonArgs,
    },
    Connect {
        #[command(flatten)]
        common: CommonArgs,
    },
}

#[derive(Debug, Clone, Args)]
struct CommonArgs {
    /// Use a custom relay server (specify multiple times for multiple relays)
    #[arg(
        long,
        visible_alias = "relay",
        action = clap::ArgAction::Append,
        value_name = "tcp://HOSTNAME:PORT",
        value_hint = clap::ValueHint::Url,
        env = "WORMHOLE_RELAY_URL",
    )]
    relay_server: Vec<url::Url>,
    /// Use a custom rendezvous server. Both sides need to use the same value in order to find each other.
    #[arg(long, value_name = "ws://example.org", value_hint = clap::ValueHint::Url, env = "WORMHOLE_MAILBOX_URL")]
    rendezvous_server: Option<url::Url>,
    /// Disable the relay server support and force a direct connection.
    #[arg(long)]
    force_direct: bool,
    /// Always route traffic over a relay server. This hides your IP address from the peer (but not from the server operators. Use Tor for that).
    #[arg(long, conflicts_with = "force_direct")]
    force_relay: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { mirror, common } => victim::run(mirror).await,
        Cmd::Connect { common } => {
            let code = Code::from_str(&completer::enter_code()?)?;
            helper::run(code).await
        }
    }
}
