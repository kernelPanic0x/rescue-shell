use crate::common::{ALPN, ConnectionStateWatcher};
use crate::console::{
    LocalConsole, Osc52Extractor, Role, StatusBarHandle, TermGuard, is_detach_key,
    process_pty_output, render_local_screen,
};
use crate::protocol::Msg;
use crate::protocol::{decode, encode};
use crate::{ServeArgs, app_config};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use crossterm::terminal::{enable_raw_mode, size};
use futures::{SinkExt, StreamExt};
use iroh::PublicKey;
use iroh::{
    Endpoint, SecretKey,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use magic_wormhole::{Code, MailboxConnection, Wormhole};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tokio::io::BufReader;
use tokio::sync::{broadcast, mpsc};
use tokio_util::codec::LengthDelimitedCodec;

pub struct HelperHub {
    to_helpers: broadcast::Sender<Msg>,
    from_helpers_rx: tokio::sync::Mutex<mpsc::Receiver<Msg>>,
    #[allow(unused)]
    link: Link,
}

impl HelperHub {
    pub async fn start(
        args: ServeArgs,
        statusbar_handle: StatusBarHandle,
        vt_parser: Arc<Mutex<vt100::Parser>>,
    ) -> Result<Self> {
        let (to_helpers, _) = broadcast::channel(64);
        let (from_helpers_tx, from_helpers_rx) = mpsc::channel(64);

        let protocol = Protocol::new(
            &args,
            to_helpers.clone(),
            from_helpers_tx,
            statusbar_handle.clone(),
            vt_parser,
        )?;

        let link = Link::new(args, protocol, statusbar_handle)
            .await
            .context("Link creation")?;

        Ok(Self {
            to_helpers,
            from_helpers_rx: tokio::sync::Mutex::new(from_helpers_rx),
            link,
        })
    }

    pub fn broadcast(&self, msg: Msg) {
        let _ = self.to_helpers.send(msg);
    }

    pub async fn recv(&self) -> Option<Msg> {
        self.from_helpers_rx.lock().await.recv().await
    }

    pub async fn shutdown(&self) {
        self.link.shutdown().await
    }
}

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    to_pty_tx: mpsc::Sender<Bytes>,
    pty_out_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    child_exit: tokio::sync::Mutex<
        Option<tokio::task::JoinHandle<std::io::Result<portable_pty::ExitStatus>>>,
    >,
}

impl PtySession {
    pub fn spawn(cols: u16, rows: u16) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell = find_shell();
        let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.env("TERM", term);
        cmd.env("RESCUE_SHELL", std::env::current_exe()?);
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn {shell}"))?;
        drop(pair.slave);

        let master = pair.master;
        let mut pty_reader = master.try_clone_reader()?;
        let mut pty_writer = master.take_writer()?;

        let (pty_out_tx, pty_out_rx) = mpsc::channel::<Vec<u8>>(64);
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 8192];
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if pty_out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let (to_pty_tx, mut to_pty_rx) = mpsc::channel::<Bytes>(64);
        tokio::task::spawn_blocking(move || {
            while let Some(bytes) = to_pty_rx.blocking_recv() {
                if pty_writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });

        let child_exit = tokio::task::spawn_blocking(move || child.wait());

        Ok(Self {
            master,
            to_pty_tx,
            pty_out_rx: tokio::sync::Mutex::new(pty_out_rx),
            child_exit: tokio::sync::Mutex::new(Some(child_exit)),
        })
    }

    pub async fn read_output(&self) -> Option<Vec<u8>> {
        self.pty_out_rx.lock().await.recv().await
    }

    pub async fn write_input(&self, bytes: Bytes) -> Result<()> {
        self.to_pty_tx.send(bytes).await.map_err(|e| anyhow!(e))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    pub async fn wait_exit(&self) -> Result<()> {
        let mut handle = self.child_exit.lock().await;

        if let Some(handle) = handle.as_mut() {
            match handle.await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(anyhow!("wait failed: {e}")),
                Err(e) => Err(anyhow!("join failed: {e}")),
            }
        } else {
            unreachable!("handle already taken")
        }
    }
}

#[derive(Debug)]
pub struct Victim {
    #[allow(dead_code)]
    code: Code,
}

impl Victim {
    pub async fn run(args: ServeArgs) -> Result<()> {
        enable_raw_mode()?;
        #[cfg(windows)]
        crate::console::enable_vt_input()?;
        let _guard = TermGuard;

        let (mut cols, mut rows) = size()?;
        let mut pty_rows = rows.saturating_sub(1).max(1);
        let vt_parser = Arc::new(Mutex::new(vt100::Parser::new(pty_rows, cols, 1000)));
        let statusbar_handle = StatusBarHandle::new(Role::Victim);
        let mut statusbar_rx = statusbar_handle.subscribe();

        let pty = PtySession::spawn(cols, pty_rows)?;
        let console = LocalConsole::new();
        #[cfg(unix)]
        let mut vte_parser = vte::Parser::new();
        let mut osc52_extractor = Osc52Extractor::default();
        let hub = HelperHub::start(args, statusbar_handle.clone(), vt_parser.clone()).await?;

        #[cfg(unix)]
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

        let res: Result<()> = loop {
            #[cfg(unix)]
            let sigwinch_recv = sigwinch.recv();
            #[cfg(not(unix))]
            let sigwinch_recv = std::future::pending::<Option<()>>();

            tokio::select! {
                // Statusbar update
                _ = statusbar_rx.changed() => {
                    let frame = render_local_screen(vt_parser.clone(), &statusbar_rx.borrow(), cols, rows);
                    console.write_stdout(frame).await?;

                    hub.broadcast(Msg::ConnectedHelpers(statusbar_handle.get_connected()));
                }

                // Shell output -> Mirror locally AND send to Helpers
                Some(bytes) = pty.read_output() => {
                    // Model the screen FIRST so the CPR reply uses the post-chunk cursor.
                    vt_parser.lock().unwrap().process(&bytes);

                    // Answer device queries (DA1/DA2/DSR/CPR/XTVERSION) back to the shell.
                    //
                    // Unix only: there the shell talks to us over a raw byte pipe.
                    // On Windows, ConPTY already answers the shell's queries itself, and
                    // injecting our own ESC-prefixed replies leaks a lone ESC keystroke
                    // into PSReadLine (bound to RevertLine = clear the command line).
                    #[cfg(unix)]
                    if let Some(reply) = process_pty_output(&bytes, vt_parser.clone(), &mut vte_parser)? {
                        pty.write_input(reply).await?;
                    }

                    if let Some(output) = osc52_extractor.extract(&bytes) {
                        console.write_stdout(output).await?;
                    }

                    let frame = render_local_screen(vt_parser.clone(), &statusbar_rx.borrow(), cols, rows);
                    console.write_stdout(frame).await?;

                    hub.broadcast(Msg::Data(Bytes::from(bytes)));
                }

                // Victim local typing -> Send to PTY
                Some(bytes) = console.read_stdin() => {
                    if is_detach_key(&bytes) {
                        break Ok(());
                    }
                    pty.write_input(bytes).await?;
                }

                // Remote messages from Helper -> Send to PTY / Resize
                Some(msg) = hub.recv() => {
                    match msg {
                        Msg::Data(bytes) => {
                            pty.write_input(bytes).await?;
                        }
                        Msg::Resize { cols, rows } => {
                            pty.resize(cols, rows)?;
                        }
                        Msg::Bye => {},
                        Msg::ConnectedHelpers(_) => {},
                    }
                }

                // Window resized locally -> update PTY & VT Parser
                _ = sigwinch_recv => {
                    if let Ok((new_cols, new_rows)) = size() {
                        cols = new_cols;
                        rows = new_rows;
                        pty_rows = rows.saturating_sub(1).max(1);

                        let _ = pty.resize(cols, pty_rows);
                        vt_parser.lock().unwrap().screen_mut().set_size(pty_rows, cols);

                        let frame = render_local_screen(vt_parser.clone(), &statusbar_rx.borrow(), cols, rows);
                        console.write_stdout(frame).await?;
                    }
                }

                // Child Shell exited
                res = pty.wait_exit() => {
                    if let Err(e) = res {
                        eprintln!("PTY wait error: {e}");
                    }
                    break Ok(());
                }
            }
        };

        hub.broadcast(Msg::Bye);
        hub.shutdown().await;
        console.flush_and_close().await;

        res
    }
}

struct StatusBarClientsGuard {
    statusbar_handle: StatusBarHandle,
}

impl StatusBarClientsGuard {
    fn new(statusbar_handle: StatusBarHandle) -> Self {
        statusbar_handle.inc_connected();
        Self { statusbar_handle }
    }
}

impl Drop for StatusBarClientsGuard {
    fn drop(&mut self) {
        self.statusbar_handle.dec_connected();
    }
}

struct Protocol {
    allowed_peers: Option<Vec<PublicKey>>,
    from_helper: mpsc::Sender<Msg>,
    to_helpers: broadcast::Sender<Msg>,
    statusbar_handle: StatusBarHandle,
    vt_parser: Arc<Mutex<vt100::Parser>>,
}

impl Protocol {
    fn new(
        args: &ServeArgs,
        to_helpers: broadcast::Sender<Msg>,
        from_helper: mpsc::Sender<Msg>,
        statusbar_handle: StatusBarHandle,
        vt_parser: Arc<Mutex<vt100::Parser>>,
    ) -> anyhow::Result<Self> {
        let mut allowed_peers: Option<Vec<PublicKey>> = None;

        if let Some(ref path) = args.allowed_public_keys_file {
            let contents = std::fs::read_to_string(path)?;
            let peers: Vec<PublicKey> = contents
                .split_whitespace()
                .map(|s| s.parse::<PublicKey>())
                .collect::<Result<_, _>>()?;

            allowed_peers.get_or_insert_default().extend(peers);
        }

        if let Some(ref peers) = args.allowed_public_keys {
            allowed_peers.get_or_insert_default().extend(peers);
        }

        Ok(Self {
            allowed_peers,
            from_helper,
            to_helpers,
            statusbar_handle,
            vt_parser,
        })
    }
}

impl std::fmt::Debug for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Protocol")
            .field("from_helper", &self.from_helper)
            .field("to_helpers", &self.to_helpers)
            .field("statusbar_handle", &self.statusbar_handle)
            .field("vt_parser", &"<vt100::Parser>")
            .finish()
    }
}

impl ProtocolHandler for Protocol {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        if c.alpn() != ALPN {
            return Err(AcceptError::NotAllowed {
                meta: n0_error::Meta::default(),
            });
        }

        if let Some(ref allowed) = self.allowed_peers
            && !allowed.contains(&c.remote_id())
        {
            return Err(AcceptError::NotAllowed {
                meta: n0_error::Meta::default(),
            });
        }

        let _sb_clients_guard = StatusBarClientsGuard::new(self.statusbar_handle.clone());

        let mut to_helpers = self.to_helpers.subscribe();
        let from_helper = self.from_helper.clone();

        let (tx, rx) = c.accept_bi().await?;
        let encoder = async_compression::tokio::write::Lz4Encoder::new(tx);
        let decoder = async_compression::tokio::bufread::Lz4Decoder::new(BufReader::new(rx));
        let mut raw_writer =
            tokio_util::codec::FramedWrite::new(encoder, LengthDelimitedCodec::new());
        let mut raw_reader =
            tokio_util::codec::FramedRead::new(decoder, LengthDelimitedCodec::new());

        let initial_state = {
            let parser = self.vt_parser.lock().unwrap();
            Bytes::from(parser.screen().state_formatted())
        };

        if let Ok(encoded) = encode(&Msg::Data(initial_state)) {
            let _ = raw_writer.send(encoded).await;
        }

        let helper_victim = tokio::spawn(async move {
            loop {
                let bytes = raw_reader.next().await.ok_or(anyhow!("reader is none"))??;
                let msg = decode(&bytes)?;
                from_helper.send(msg).await?;
            }

            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        let victim_helper = tokio::spawn(async move {
            loop {
                let msg = to_helpers.recv().await?;
                raw_writer.send(encode(&msg)?).await?;
            }

            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        let (res, _, _) = futures::future::select_all(vec![victim_helper, helper_victim]).await;
        if let Err(e) = res {
            return Err(AcceptError::User {
                source: n0_error::AnyError::from_std(e),
                meta: n0_error::Meta::default(),
            });
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Link {
    #[allow(dead_code)]
    secret_key: SecretKey,
    #[allow(dead_code)]
    router: Router,
}

impl Link {
    async fn new(
        args: ServeArgs,
        protocol: Protocol,
        statusbar_handle: StatusBarHandle,
    ) -> anyhow::Result<Self> {
        let secret_key = args
            .common
            .private_key
            .clone()
            .unwrap_or_else(SecretKey::generate);

        let public_key = secret_key.public();

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key.clone())
            .bind()
            .await?;

        let router = Router::builder(endpoint.clone())
            .accept(ALPN, protocol)
            .spawn();

        let connection_watcher = ConnectionStateWatcher::new(endpoint, statusbar_handle.clone());
        tokio::task::spawn(async move { connection_watcher.watch().await });

        tokio::task::spawn(async move {
            loop {
                let wormhole = async {
                    let mailbox = match &args.common.code {
                        Some(code) => {
                            MailboxConnection::connect(
                                app_config(&args.common),
                                code.to_owned(),
                                true,
                            )
                            .await?
                        }
                        None => {
                            MailboxConnection::create(
                                app_config(&args.common),
                                args.code_length.unwrap_or(2),
                            )
                            .await?
                        }
                    };

                    let code = mailbox.code();
                    statusbar_handle.set_code(Some(code.clone()));

                    let mut wormhole = Wormhole::connect(mailbox).await?;
                    wormhole.send(public_key.as_bytes().to_vec()).await?;

                    Ok::<(), anyhow::Error>(())
                };

                let not_alone = async {
                    if args.multiple_helpers {
                        // continue generating codes
                        let _ = std::future::pending::<Result<()>>().await;
                    } else {
                        let _ = statusbar_handle
                            .subscribe()
                            .wait_for(|s| s.connected_helpers > 0)
                            .await;
                    }

                    Ok::<(), anyhow::Error>(())
                };

                tokio::select! {
                    _ = wormhole => {}
                    _ = not_alone => {
                        statusbar_handle.set_code(None);

                        // wait until alone again
                        let _ = statusbar_handle.subscribe().wait_for(|s| s.connected_helpers == 0).await;
                    }
                }
            }
        });

        Ok(Self { secret_key, router })
    }

    pub async fn shutdown(&self) {
        let _ = self.router.shutdown().await;
    }
}

fn find_shell() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into())
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(s) = std::env::var("SHELL")
            && std::path::Path::new(&s).exists()
        {
            return s;
        }

        ["/bin/sh", "/bin/bash", "/bin/ash", "/bin/dash"]
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "sh".into())
    }
}
