use crate::common::{ALPN, ConnectionStateWatcher, TermGuard};
use crate::protocol::{decode, encode};
use crate::screen::{Role, StatusBarHandle, StatusBarState, render_local_screen};
use crate::{common::is_detach_key, protocol::Msg};
use anyhow::anyhow;
use anyhow::{Context, Result};
use bytes::Bytes;
use crossterm::cursor::{MoveTo, Show};
use crossterm::execute;
use crossterm::style::{Attribute, SetAttribute};
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size};
use futures::SinkExt;
use futures::StreamExt;
use futures::future::{join, select_all};
use iroh::Watcher;
use iroh::endpoint::RelayStatus;
use iroh::{
    Endpoint, SecretKey,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use libc::{IW_AUTH_WPA_VERSION_WPA2, clearerr};
use magic_wormhole::{AppConfig, Code, MailboxConnection, Wormhole, transfer::AppVersion};
use n0_error::AnyError;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::borrow::Borrow;
use std::io::{Read, Write};
use std::ops::Deref;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use tokio::select;
use tokio::sync::{broadcast, mpsc};
use tokio_serde::{SymmetricallyFramed, formats::SymmetricalBincode};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

struct SBClientsGuard {
    statusbar_handle: StatusBarHandle,
}

impl SBClientsGuard {
    fn new(statusbar_handle: StatusBarHandle) -> Self {
        statusbar_handle.helper_connected();
        Self { statusbar_handle }
    }
}

impl Drop for SBClientsGuard {
    fn drop(&mut self) {
        self.statusbar_handle.helper_disconnected();
    }
}

struct Protocol {
    from_helper: mpsc::Sender<Msg>,
    to_helpers: broadcast::Sender<Msg>,
    statusbar_handle: StatusBarHandle,
    vt_parser: Arc<Mutex<vt100::Parser>>,
}

impl Protocol {
    fn new(
        to_helpers: broadcast::Sender<Msg>,
        from_helper: mpsc::Sender<Msg>,
        statusbar_handle: StatusBarHandle,
        vt_parser: Arc<Mutex<vt100::Parser>>,
    ) -> Self {
        Self {
            from_helper,
            to_helpers,
            statusbar_handle,
            vt_parser,
        }
    }
}

impl std::fmt::Debug for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Protocol")
            .field("from_helper", &self.from_helper)
            .field("to_helpers", &self.to_helpers)
            .field("statusbar_handle", &self.statusbar_handle)
            .field("vt_parser", &"<vt100::Parser>") // Placeholder string for Debug output
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

        // Handle inc and dec on drop
        let _sb_clients_guard = SBClientsGuard::new(self.statusbar_handle.clone());

        let mut to_helpers = self.to_helpers.subscribe();
        let from_helper = self.from_helper.clone();

        let (tx, rx) = c.accept_bi().await?;
        let mut raw_writer = tokio_util::codec::FramedWrite::new(tx, LengthDelimitedCodec::new());
        let mut raw_reader = tokio_util::codec::FramedRead::new(rx, LengthDelimitedCodec::new());

        // Send full screen buffer first
        let initial_state = {
            let parser = self.vt_parser.lock().unwrap();
            parser.screen().state_formatted() // Generates full ANSI redraw payload
        };

        if let Ok(encoded) = encode(&Msg::Data(initial_state)) {
            let _ = raw_writer.send(Bytes::from(encoded)).await;
        }

        // Helper -> Victim
        let helper_victim = tokio::spawn(async move {
            loop {
                let bytes = raw_reader.next().await.ok_or(anyhow!("reader is none"))??;
                let msg = decode(&bytes)?;
                from_helper.send(msg).await?;
            }

            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        // Victim -> Helpers
        let victim_helper = tokio::spawn(async move {
            loop {
                let msg = to_helpers.recv().await?;
                raw_writer.send(Bytes::from(encode(&msg)?)).await?;
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
        app_config: AppConfig<AppVersion>,
        protocol: Protocol,
        statusbar_handle: StatusBarHandle,
    ) -> anyhow::Result<Self> {
        let secret_key = SecretKey::generate();
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
                let _ = async {
                    let mailbox = MailboxConnection::create(app_config.clone(), 2).await?;
                    let code = mailbox.code();
                    statusbar_handle.set_code(code.clone());

                    let mut wormhole = Wormhole::connect(mailbox).await?;
                    wormhole.send(public_key.as_bytes().to_vec()).await?;

                    Ok::<(), anyhow::Error>(())
                }
                .await;
            }
        });

        Ok(Self { secret_key, router })
    }
}

#[derive(Debug)]
pub struct Victim {
    #[allow(dead_code)]
    code: Code,
}

impl Victim {
    pub async fn run(app_config: AppConfig<AppVersion>) -> Result<()> {
        enable_raw_mode()?;
        let _guard = TermGuard;

        let (mut cols, mut rows) = size()?;
        let mut pty_rows = rows.saturating_sub(1).max(1);
        let vt_parser = Arc::new(Mutex::new(vt100::Parser::new(pty_rows, cols, 1000)));
        let (statusbar_handle, mut statusbar_rx) = StatusBarHandle::new(Role::Victim);

        // Link channels
        let to_helpers = broadcast::Sender::<Msg>::new(1000);
        let (from_helper_tx, mut from_helper_rx) = mpsc::channel::<Msg>(1000);
        let protocol = Protocol::new(
            to_helpers.clone(),
            from_helper_tx,
            statusbar_handle.clone(),
            vt_parser.clone(),
        );
        let _link = Link::new(app_config, protocol, statusbar_handle)
            .await
            .context("Link creation")?;

        // Pty
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: pty_rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell = find_shell();
        let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.env("TERM", term);
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn {shell}"))?;
        drop(pair.slave); // slave belongs to child now

        let master: Box<dyn MasterPty + Send> = pair.master;
        let mut pty_reader = master.try_clone_reader()?;
        let pty_writer = master.take_writer()?;

        // ── 3. Threads for I/O ────────────────────────────────────

        // PTY Reader Thread: PTY output -> async channel
        let (pty_out_tx, mut pty_out_rx) = mpsc::channel::<Vec<u8>>(64);
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

        // PTY Writer Thread: async channel -> PTY input
        let (to_pty_tx, mut to_pty_rx) = mpsc::channel::<Vec<u8>>(64);
        let mut pty_writer = pty_writer;
        tokio::task::spawn_blocking(move || {
            while let Some(bytes) = to_pty_rx.blocking_recv() {
                if pty_writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });

        // Stdin Reader Thread: std::thread so Tokio doesn't block on exit
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Local Stdout Writer Thread
        let (stdout_tx, mut stdout_rx) = mpsc::channel::<Vec<u8>>(64);
        let stdout_handle = tokio::task::spawn_blocking(move || {
            let mut stdout = std::io::stdout();
            while let Some(bytes) = stdout_rx.blocking_recv() {
                if stdout.write_all(&bytes).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        });

        let mut child_exit = tokio::task::spawn_blocking(move || child.wait());

        #[cfg(unix)]
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

        // ── 4. Main Event Multiplexer Loop ────────────────────────
        let res: anyhow::Result<()> = loop {
            #[cfg(unix)]
            let sigwinch_recv = sigwinch.recv();
            #[cfg(not(unix))]
            let sigwinch_recv = std::future::pending::<Option<()>>();

            tokio::select! {
                // statusbar update
                _ = statusbar_rx.changed() => {
                    let state = statusbar_rx.borrow();
                    let frame = render_local_screen(vt_parser.clone(), &state, cols, rows);
                    stdout_tx.send(frame).await?;
                }

                // Shell output -> Mirror locally AND send to Helpers
                Some(bytes) = pty_out_rx.recv() => {
                    vt_parser.lock().unwrap().process(&bytes);
                    let state = statusbar_rx.borrow();
                    let frame = render_local_screen(vt_parser.clone(), &state, cols, rows);
                    stdout_tx.send(frame).await?;

                    // Returns Err when no helpers connected
                    let _ = to_helpers.send(Msg::Data(bytes));
                }

                // Victim local typing -> Send to PTY
                Some(bytes) = stdin_rx.recv() => {
                    if is_detach_key(&bytes) {
                        break Ok(());
                    }
                    to_pty_tx.send(bytes).await?;
                }

                // Remote messages from Helper -> Send to PTY / Resize
                incoming = from_helper_rx.recv() => {
                    match incoming {
                        Some(msg) => match msg {
                            Msg::Data(bytes) => {
                                to_pty_tx.send(bytes).await?;
                            }
                            Msg::Resize { cols, rows } => {
                                master.resize(PtySize {
                                    rows, cols, pixel_width: 0, pixel_height: 0,
                                })?;
                            }
                            Msg::Bye => {},
                        },
                        None => {}, // Gracefully ignore channel closure
                    }
                }

                // Window resized locally -> update PTY & VT Parser
                _ = sigwinch_recv => {
                    if let Ok((new_cols, new_rows)) = size() {
                        cols = new_cols;
                        rows = new_rows;
                        pty_rows = rows.saturating_sub(1).max(1);

                        let _ = master.resize(PtySize {
                            rows: pty_rows, cols, pixel_width: 0, pixel_height: 0,
                        });

                        vt_parser.lock().unwrap().screen_mut().set_size(pty_rows, cols);
                        let state = statusbar_rx.borrow();

                        // Redraw screen on resize
                        let frame = render_local_screen(vt_parser.clone(), &state, cols, rows);
                        stdout_tx.send(frame).await?;
                    }
                }

                // Child Shell exited
                status = &mut child_exit => {
                    match status {
                        Ok(Ok(_))     => {}
                        Ok(Err(e))    => eprintln!("wait failed: {e}"),
                        Err(join_err) => eprintln!("wait task: {join_err}"),
                    }
                    break Ok(());
                }
            }
        };

        let _ = res?;

        let _ = to_helpers.send(Msg::Bye);

        // Drop sender and wait for the stdout thread to finish writing remaining frames
        drop(stdout_tx);
        let _ = stdout_handle.await;

        // Reset terminal attributes, clear the screen, move cursor to (0,0) and show it
        let mut stdout = std::io::stdout();
        let _ = execute!(
            stdout,
            SetAttribute(Attribute::Reset),
            Clear(ClearType::All),
            MoveTo(0, 0),
            Show
        );

        println!("\r\n[session ended]");

        Ok(())
    }
}

fn find_shell() -> String {
    if let Ok(s) = std::env::var("SHELL")
        && std::path::Path::new(&s).exists()
    {
        return s;
    }
    for c in ["/bin/bash", "/bin/sh", "/bin/ash", "/bin/dash"] {
        if std::path::Path::new(c).exists() {
            return c.into();
        }
    }
    "sh".into()
}
