mod common;
mod console;
mod helper;
mod io;
mod protocol;
mod victim;

use std::{borrow::Cow, path::PathBuf};

use clap::{Args, Parser, Subcommand};
use color_eyre::eyre::Context;
use iroh::{PublicKey, SecretKey};
use magic_wormhole::{AppID, Code, transfer::APP_CONFIG};

use crate::{
    helper::Helper,
    io::{copy_to_osc52, gen_public_key, gen_secret_key, read_public_keys_file},
    victim::Victim,
};

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
    /// Generate an iroh secret key and print it to stdout.
    GenKey,
    /// Generate an iroh public key based a secret key read from stdin.
    GenPub,
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

    /// Helper is only allowed to read but not to write
    #[arg(long)]
    read_only_helper: bool,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Serve(mut args) => {
            if let Some(path) = &args.allowed_public_keys_file {
                let list = read_public_keys_file(path).context("Read public keys from file")?;
                args.allowed_public_keys
                    .get_or_insert_default()
                    .extend(list);
            }

            Victim::run(args).await?
        }
        Cmd::Connect(mut args) => {
            if args.common.code.is_none() {
                args.common.code = Some(wormhole_cli::completer::enter_code()?.parse()?);
            }

            Helper::run(args).await?
        }
        Cmd::Copy => copy_to_osc52()?,
        Cmd::Wormhole { args } => {
            let code = wormhole_cli::run_from(args).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cmd::GenKey => gen_secret_key(),
        Cmd::GenPub => gen_public_key()?,
    }

    Ok(())
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
