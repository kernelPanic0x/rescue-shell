// src/main.rs
mod common;
mod completer;
mod console;
mod helper;
mod osc52;
mod osc_extractor;
mod protocol;
mod victim;

use std::{borrow::Cow, str::FromStr};

use clap::{Args, Parser, Subcommand};
use magic_wormhole::{AppID, Code, transfer::APP_CONFIG};

use crate::{helper::Helper, osc52::copy_to_osc52, victim::Victim};

#[derive(Parser)]
#[command(about = "Remote rescue shell over magic-wormhole")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a session
    Serve {
        #[command(flatten)]
        common: CommonArgs,
    },
    /// Connect to a session
    Connect {
        #[command(flatten)]
        common: CommonArgs,
    },
    /// Copys stdin to OSC52 for remote clipboard
    Copy,
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
        Cmd::Serve { common } => {
            let config = app_config(&common);
            Victim::run(config).await
        }
        Cmd::Connect { common } => {
            let config = app_config(&common);
            let code = Code::from_str(&completer::enter_code()?)?;
            Helper::run(config, code).await
        }
        Cmd::Copy => copy_to_osc52(),
    }
}

fn app_config(
    common: &CommonArgs,
) -> magic_wormhole::AppConfig<magic_wormhole::transfer::AppVersion> {
    let mut config = APP_CONFIG.id(AppID::new("rescue-shell-v1"));
    if let Some(url) = &common.rendezvous_server {
        config = config.rendezvous_url(Cow::Owned(url.to_string()));
    }
    config
}
