use crate::common::{ALPN, ConnectionStateWatcher};
use crate::console::{
    LocalConsole, Role, StatusBarHandle, TermGuard, is_detach_key, render_local_screen,
};
use crate::protocol::Msg;
use crate::protocol::{decode, encode};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use crossterm::terminal::{enable_raw_mode, size};
use futures::{SinkExt, StreamExt};
use iroh::{
    Endpoint, SecretKey,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use magic_wormhole::{AppConfig, Code, MailboxConnection, Wormhole, transfer::AppVersion};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
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
        app_config: AppConfig<AppVersion>,
        statusbar_handle: StatusBarHandle,
        vt_parser: Arc<Mutex<vt100::Parser>>,
    ) -> Result<Self> {
        let (to_helpers, _) = broadcast::channel(1000);
        let (from_helpers_tx, from_helpers_rx) = mpsc::channel(1000);

        let protocol = Protocol::new(
            to_helpers.clone(),
            from_helpers_tx,
            statusbar_handle.clone(),
            vt_parser,
        );

        let link = Link::new(app_config, protocol, statusbar_handle)
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
}

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    to_pty_tx: mpsc::Sender<Vec<u8>>,
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

        let (to_pty_tx, mut to_pty_rx) = mpsc::channel::<Vec<u8>>(64);
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

    pub async fn write_input(&self, bytes: Vec<u8>) -> Result<()> {
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
    pub async fn run(app_config: AppConfig<AppVersion>) -> Result<()> {
        enable_raw_mode()?;
        let _guard = TermGuard;

        let (mut cols, mut rows) = size()?;
        let mut pty_rows = rows.saturating_sub(1).max(1);
        let vt_parser = Arc::new(Mutex::new(vt100::Parser::new(pty_rows, cols, 1000)));
        let (statusbar_handle, mut statusbar_rx) = StatusBarHandle::new(Role::Victim);

        let pty = PtySession::spawn(cols, pty_rows)?;
        let console = LocalConsole::new();
        let hub = HelperHub::start(app_config, statusbar_handle.clone(), vt_parser.clone()).await?;

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
                    vt_parser.lock().unwrap().process(&bytes);
                    let frame = render_local_screen(vt_parser.clone(), &statusbar_rx.borrow(), cols, rows);
                    console.write_stdout(frame).await?;

                    hub.broadcast(Msg::Data(bytes));
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

        let _sb_clients_guard = StatusBarClientsGuard::new(self.statusbar_handle.clone());

        let mut to_helpers = self.to_helpers.subscribe();
        let from_helper = self.from_helper.clone();

        let (tx, rx) = c.accept_bi().await?;
        let mut raw_writer = tokio_util::codec::FramedWrite::new(tx, LengthDelimitedCodec::new());
        let mut raw_reader = tokio_util::codec::FramedRead::new(rx, LengthDelimitedCodec::new());

        let initial_state = {
            let parser = self.vt_parser.lock().unwrap();
            parser.screen().state_formatted()
        };

        if let Ok(encoded) = encode(&Msg::Data(initial_state)) {
            let _ = raw_writer.send(Bytes::from(encoded)).await;
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

fn find_shell() -> String {
    #[cfg(target_os = "windows")]
    {
        if let Ok(s) = std::env::var("COMSPEC") {
            return s; // Typically C:\Windows\System32\cmd.exe
        }
        "powershell.exe".into()
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(s) = std::env::var("SHELL")
            && std::path::Path::new(&s).exists()
        {
            return s;
        }

        // Android/termux shell
        if let Ok(s) = std::env::var("PREFIX") {
            let s = format!("{}/bin/sh", s);
            if std::path::Path::new(&s).exists() {
                return s;
            }
        }

        for c in ["/bin/bash", "/bin/sh", "/bin/ash", "/bin/dash"] {
            if std::path::Path::new(c).exists() {
                return c.into();
            }
        }
        "sh".into()
    }
}
