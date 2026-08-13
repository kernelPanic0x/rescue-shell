// src/main.rs
mod common;
mod completer;
mod console;
mod helper;
mod osc52;
mod osc_extractor;
mod protocol;
mod victim;

use std::{borrow::Cow, path::PathBuf};

use clap::{Args, Parser, Subcommand};
use iroh::{PublicKey, SecretKey};
use magic_wormhole::{AppID, Code, transfer::APP_CONFIG};

use crate::{helper::Helper, osc52::copy_to_osc52, victim::Victim};

#[derive(Parser)]
#[command(
    name = "rescue-shell",
    author,
    about,
    after_help = "\
Environment Variables:
    SHELL         Set the shell for the session.
                  You can use `echo \"example text\" | $SHELL copy` to copy to helper clipboard.
    RESCUE_SHELL  Is set by rescue-shell to make it more easy to find the executable.
",
    help_template = "\
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}
Authors: {author}
"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a session
    Serve(ServeArgs),
    /// Connect to a session
    Connect(ConnectArgs),
    /// Copys stdin to OSC52 for remote clipboard
    Copy,
    /// Send/receive files or forward ports (embedded wormhole-rs)
    Wormhole {
        /// Passed through to wormhole-rs verbatim, e.g. `rescue-shell wormhole send -c 4 file.txt`
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            hide = true,
            value_name = "ARGS"
        )]
        args: Vec<std::ffi::OsString>,
    },
}

#[derive(Debug, Clone, Args)]
struct CommonArgs {
    /// Use a custom rendezvous server. Both sides need to use the same value in order to find each other.
    #[arg(long, value_name = "ws://example.org", value_hint = clap::ValueHint::Url, env = "WORMHOLE_MAILBOX_URL")]
    rendezvous_server: Option<url::Url>,

    /// Manually set a fixed private key.
    #[arg(long, env = "RESCUE_SHELL_PRIVATE_KEY")]
    private_key: Option<SecretKey>,

    /// The wormhole code to establish a connection.
    #[arg(long, short, env = "RESCUE_SHELL_CODE")]
    code: Option<Code>,
}

#[derive(Debug, Clone, Args)]
struct ConnectArgs {
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, Clone, Args)]
struct ServeArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Generate new codes to allow multiple connected helpers at once.
    #[arg(long, env = "RESCUE_SHELL_MULTIPLE_HELPERS")]
    multiple_helpers: bool,

    /// Number of words to use when creating the wormhole code.
    #[arg(
        long,
        short = 'l',
        conflicts_with = "code",
        env = "RESCUE_SHELL_CODE_LENGTH"
    )]
    code_length: Option<usize>,

    /// Only allow these public keys to connect.
    #[arg(long, env = "RESCUE_SHELL_ALLOWED_PUBLIC_KEYS")]
    allowed_public_keys: Option<Vec<PublicKey>>,

    /// Read allowed peers from file seperated by any whitespace.
    #[arg(long, env = "RESCUE_SHELL_ALLOWED_PUBLIC_KEYS_FILE")]
    allowed_public_keys_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Serve(args) => Victim::run(args).await,
        Cmd::Connect(mut args) => {
            if args.common.code.is_none() {
                args.common.code = Some(completer::enter_code()?.parse()?);
            }

            Helper::run(args).await
        }
        Cmd::Copy => copy_to_osc52(),
        Cmd::Wormhole { args } => {
            let code = wormhole_cli::run_from(args).await;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
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
