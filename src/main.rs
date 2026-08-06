// src/main.rs
mod common;
mod completer;
mod display;
mod helper;
mod link;
mod protocol;
mod victim;

use std::{borrow::Cow, str::FromStr};

use clap::{Args, Parser, Subcommand};
use magic_wormhole::{
    AppID, Code, MailboxConnection, Wormhole,
    transfer::APP_CONFIG,
    transit::{self, RelayHint, TransitRole},
};

use crate::{common::establish_transit, helper::Helper, victim::Victim};

#[derive(Parser)]
#[command(about = "Remote rescue shell over magic-wormhole")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Serve {
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

fn parse_transit_args(args: &CommonArgs) -> transit::Abilities {
    match (args.force_direct, args.force_relay) {
        (false, false) => transit::Abilities::ALL,
        (true, false) => transit::Abilities::FORCE_DIRECT,
        (false, true) => transit::Abilities::FORCE_RELAY,
        (true, true) => unreachable!("These flags are mutually exclusive"),
    }
}

fn parse_relay_hints(relay_servers: &[url::Url]) -> anyhow::Result<Vec<RelayHint>> {
    relay_servers
        .iter()
        .map(|url| {
            RelayHint::from_urls(
                url.host_str().map(str::to_owned), // human-readable name
                std::iter::once(url.clone()),
            )
            .map_err(Into::into)
        })
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Serve { common } => {
            let config = app_config(&common);
            let relay_hints = parse_relay_hints(&common.relay_server)?;
            let abilities = parse_transit_args(&common);

            let mailbox = MailboxConnection::create(config, 2).await?;
            let code = mailbox.code().clone();
            println!("════════════════════════════════════════");
            println!("  Give this code to your helper:");
            println!("      {code}");
            println!("════════════════════════════════════════");
            println!("Waiting for them to connect...");

            let mut wormhole = Wormhole::connect(mailbox).await?;
            let transit =
                establish_transit(&mut wormhole, relay_hints, abilities, TransitRole::Leader)
                    .await?;
            Victim::run(transit).await
        }
        Cmd::Connect { common } => {
            let config = app_config(&common);
            let relay_hints = parse_relay_hints(&common.relay_server)?;
            let abilities = parse_transit_args(&common);

            let code = Code::from_str(&completer::enter_code()?)?;
            let mailbox = MailboxConnection::connect(config, code, true).await?;
            let mut wormhole = Wormhole::connect(mailbox).await?;
            let transit =
                establish_transit(&mut wormhole, relay_hints, abilities, TransitRole::Follower)
                    .await?;
            Helper::run(transit).await
        }
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
