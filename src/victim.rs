use crate::common::{ALPN, ConnectionStateWatcher};
use crate::console::{
    LocalConsole, LocalEvent, Osc52Extractor, PtyResponder, Role, StatusBarHandle, StdinProcessor,
    TerminalSizeNegotiator, window_change_signal,
};
use crate::protocol::{Encoder, HandshakePayload, PtySize, ToHelper, ToVictim};
use crate::{ServeArgs, app_config};
use bytes::Bytes;
use color_eyre::eyre::{Context, eyre};
use futures_util::{SinkExt, StreamExt};
use iroh::{
    Endpoint, SecretKey,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use iroh::{PublicKey, RelayMode};
use magic_wormhole::{Code, MailboxConnection, Wormhole};
use portable_pty::{CommandBuilder, MasterPty, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::{broadcast, mpsc};
use tokio_util::codec::LengthDelimitedCodec;

pub struct HelperHub {
    to_helpers: broadcast::Sender<ToHelper>,
    from_helpers_rx: tokio::sync::Mutex<mpsc::Receiver<ToVictim>>,
    statusbar_handle: StatusBarHandle,
    #[allow(unused)]
    link: Link,
}

impl HelperHub {
    pub async fn start(
        args: ServeArgs,
        statusbar_handle: StatusBarHandle,
        console: Arc<LocalConsole>,
    ) -> color_eyre::Result<Self> {
        let (to_helpers, _) = broadcast::channel(1024);
        let (from_helpers_tx, from_helpers_rx) = mpsc::channel(1024);

        let authenticated_peer = Arc::new(Mutex::new(None));

        let protocol = Protocol::new(
            args.allowed_public_keys.clone(),
            to_helpers.clone(),
            from_helpers_tx,
            statusbar_handle.clone(),
            console,
            authenticated_peer.clone(),
        )?;

        let link = Link::new(args, protocol, statusbar_handle.clone(), authenticated_peer)
            .await
            .context("Link creation")?;

        Ok(Self {
            to_helpers,
            statusbar_handle,
            from_helpers_rx: tokio::sync::Mutex::new(from_helpers_rx),
            link,
        })
    }

    pub fn broadcast(&self, msg: ToHelper) {
        let _ = self.to_helpers.send(msg);
    }

    pub async fn recv(&self) -> Option<ToVictim> {
        self.from_helpers_rx.lock().await.recv().await
    }

    /// Clean shutdown: Sends Bye, flushes streams, and closes endpoint
    pub async fn close_with_bye(self) {
        self.broadcast(ToHelper::Bye);

        drop(self.to_helpers);

        let mut rx = self.statusbar_handle.subscribe();
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            rx.wait_for(|s| s.connected_helpers == 0),
        )
        .await;

        self.link.shutdown().await;
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
    pub fn spawn(size: crate::protocol::PtySize) -> color_eyre::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| eyre!("{e}"))?;

        let shell = find_shell();
        let mut cmd = CommandBuilder::new(&shell);
        cmd.env("TERM", "xterm-256color");
        cmd.env("RESCUE_SHELL", std::env::current_exe()?);
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| eyre!("{e}"))
            .with_context(|| format!("failed to spawn {shell}"))?;
        drop(pair.slave);

        let master = pair.master;
        let mut pty_reader = master.try_clone_reader().map_err(|e| eyre!("{e}"))?;
        let mut pty_writer = master.take_writer().map_err(|e| eyre!("{e}"))?;

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

    pub async fn write_input(&self, bytes: Bytes) -> color_eyre::Result<()> {
        self.to_pty_tx.send(bytes).await.map_err(|e| eyre!(e))
    }

    pub fn resize(&self, size: PtySize) -> color_eyre::Result<()> {
        self.master
            .resize(portable_pty::PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| eyre!("Failed to resize pty: {}", e))?;
        Ok(())
    }

    pub async fn wait_exit(&self) -> color_eyre::Result<()> {
        let mut handle = self.child_exit.lock().await;

        if let Some(handle) = handle.as_mut() {
            match handle.await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(eyre!("wait failed: {e}")),
                Err(e) => Err(eyre!("join failed: {e}")),
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
    pub async fn run(args: ServeArgs) -> color_eyre::Result<()> {
        let statusbar_handle = StatusBarHandle::new(Role::Victim);
        let console = Arc::new(LocalConsole::new(&statusbar_handle)?);
        console.render().await?;
        let mut size_negotiator = TerminalSizeNegotiator::new(console.get_pty_size());
        let mut statusbar_rx = statusbar_handle.subscribe();

        let pty_size: PtySize = console.get_pty_size();
        let pty = PtySession::spawn(pty_size)?;
        let mut osc52_extractor = Osc52Extractor::default();

        let mut sigwinch = window_change_signal();

        let mut stdin_processor = StdinProcessor::new(pty_size.rows as i32);
        let mut pty_responder = PtyResponder::new(Arc::clone(&console));
        let mut old_connected = 0;

        let hub =
            HelperHub::start(args.clone(), statusbar_handle.clone(), Arc::clone(&console)).await?;

        let res: color_eyre::Result<()> = 'main_loop: loop {
            tokio::select! {
                // Statusbar update
                _ = statusbar_rx.changed() => {
                    console.render().await?;

                    // only send network updates on actual var change
                    let current_connected = statusbar_handle.get_connected();
                    if current_connected != old_connected {
                        hub.broadcast(ToHelper::ConnectedHelpers(current_connected));
                        old_connected = current_connected;
                    }
                }

                // Shell output -> Mirror locally AND send to Helpers
                Some(bytes) = pty.read_output() => {
                    // Model the screen FIRST so the CPR reply uses the post-chunk cursor.
                    console.access_parser_mut(|p| p.process(&bytes));

                    // Answer device queries (DA1/DA2/DSR/CPR/XTVERSION) back to the shell.
                    //
                    // Unix only: there the shell talks to us over a raw byte pipe.
                    // On Windows, ConPTY already answers the shell's queries itself, and
                    // injecting our own ESC-prefixed replies leaks a lone ESC keystroke
                    // into PSReadLine (bound to RevertLine = clear the command line).
                    if let Some(reply) = pty_responder.process(&bytes) {
                        pty.write_input(reply).await?;
                    }

                    if let Some(output) = osc52_extractor.extract(&bytes) {
                        console.write_stdout(output).await?;
                    }

                    hub.broadcast(ToHelper::Data(Bytes::from(bytes)));
                    console.render().await?;
                }

                // Victim local typing -> Send to PTY
                Some(bytes) = console.read_stdin() => {
                    let (alt, mouse_on, scrolled, app_cursor) = {
                        console.access_parser_mut(|p| (
                            p.screen().alternate_screen(),
                            p.screen().mouse_protocol_mode()!= vt100::MouseProtocolMode::None,
                            p.screen().scrollback() > 0,
                            p.screen().application_cursor(),
                        ))
                    };
                    let pty_size: PtySize = console.get_pty_size();

                    stdin_processor.set_state(alt, mouse_on, pty_size.rows as i32, app_cursor);

                    // Parse bytes safely (streaming tokenizer)
                    let (events, pty_bytes) = stdin_processor.process(&bytes);

                    // 1. Handle local events
                    for event in events {
                        match event {
                            LocalEvent::Detach => break 'main_loop Ok(()),
                            LocalEvent::Scroll(delta) => {
                                let offset = console.apply_scroll(delta).await as u32;
                                hub.broadcast(ToHelper::ScrollTo { offset });
                                console.render().await?;
                            }
                        }
                    }

                    // 2. Reset scrollback if user typed/forwarded regular keys while scrolled back
                    if !alt && scrolled && !pty_bytes.is_empty() {
                        console.access_parser_mut(|p| p.screen_mut().set_scrollback(0));
                        hub.broadcast(ToHelper::ScrollTo { offset: 0 });
                        console.render().await?;
                    }

                    pty.write_input(pty_bytes).await?;
                }

                // Remote messages from Helper -> Send to PTY / Resize
                Some(msg) = hub.recv() => {
                    match msg {
                        ToVictim::Data(bytes) => {
                            if !args.read_only_helper {
                                pty.write_input(bytes).await?;
                            }
                        }
                        ToVictim::SizeHint { id, size } => {
                            let negotiated_size: PtySize = size_negotiator.update_helper(id, size);
                            pty.resize(negotiated_size)?;
                            console.resize_parser(negotiated_size);
                            hub.broadcast(ToHelper::SetSize(negotiated_size));
                            console.render().await?;
                        }
                        ToVictim::Bye{id} => {
                            size_negotiator.remove_helper(id);
                            let size = size_negotiator.update_local(console.get_pty_size());
                            hub.broadcast(ToHelper::SetSize(size));
                            console.resize_parser(size);
                            pty.resize(size)?;
                            console.render().await?;
                        },
                        ToVictim::RequestScrollTo { offset } => {
                            console.access_parser_mut(|p| p.screen_mut().set_scrollback(offset as usize));

                            // Rebroadcast so every helper (including the one that asked) converges.
                            hub.broadcast(ToHelper::ScrollTo { offset });

                            console.render().await?;
                        }
                    }
                }

                // Window resized locally -> VT Parser
                _ = sigwinch.recv() => {
                    let size = size_negotiator.update_local(console.get_pty_size());
                    console.resize_parser(size);
                    pty.resize(size)?;
                    hub.broadcast(ToHelper::SetSize(size));
                    console.render().await?;
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

        hub.close_with_bye().await;
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
    from_helper: mpsc::Sender<ToVictim>,
    to_helpers: broadcast::Sender<ToHelper>,
    statusbar_handle: StatusBarHandle,
    console: Arc<LocalConsole>,
    authenticated_peer: Arc<Mutex<Option<PublicKey>>>,
}

impl Protocol {
    fn new(
        allowed_peers: Option<Vec<PublicKey>>,
        to_helpers: broadcast::Sender<ToHelper>,
        from_helper: mpsc::Sender<ToVictim>,
        statusbar_handle: StatusBarHandle,
        console: Arc<LocalConsole>,
        authenticated_peer: Arc<Mutex<Option<PublicKey>>>,
    ) -> color_eyre::Result<Self> {
        Ok(Self {
            authenticated_peer,
            allowed_peers,
            from_helper,
            to_helpers,
            statusbar_handle,
            console,
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
        let remote_id = c.remote_id();

        let is_authorized = match &self.allowed_peers {
            Some(whitelist) => whitelist.contains(&remote_id),
            None => self
                .authenticated_peer
                .lock()
                .unwrap()
                .map(|expected| expected == remote_id)
                .unwrap_or(false),
        };

        if !is_authorized {
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
        let mut codec_builder = LengthDelimitedCodec::builder();
        codec_builder.max_frame_length(8 * 1024 * 1024);
        let mut raw_writer =
            tokio_util::codec::FramedWrite::new(encoder, codec_builder.new_codec());
        let mut raw_reader = tokio_util::codec::FramedRead::new(decoder, codec_builder.new_codec());

        let initial_state = self.console.get_initial_screen_state();
        if let Ok(encoded) = ToHelper::Data(initial_state).encode() {
            let _ = raw_writer.send(encoded).await;
        }

        let helper_victim = tokio::spawn(async move {
            loop {
                let bytes = raw_reader.next().await.ok_or(eyre!("reader is none"))??;
                let msg = ToVictim::decode(&bytes)?;
                from_helper.send(msg).await?;
            }

            #[allow(unreachable_code)]
            Ok::<(), color_eyre::eyre::Error>(())
        });

        let victim_helper = tokio::spawn(async move {
            loop {
                match to_helpers.recv().await {
                    Ok(msg) => {
                        raw_writer.send(msg.encode()?).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        return Err(eyre!("Helper lagged behind by {n} frames"));
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // Flush & close the LZ4 encoder and QUIC stream cleanly
            let _ = raw_writer.flush().await;
            let _ = raw_writer.close().await;

            Ok::<(), color_eyre::eyre::Error>(())
        });

        let (res, _, _) =
            futures_util::future::select_all(vec![victim_helper, helper_victim]).await;
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
        authenticated_peer: Arc<Mutex<Option<PublicKey>>>,
    ) -> color_eyre::Result<Self> {
        let secret_key = args
            .common
            .private_key
            .clone()
            .unwrap_or_else(SecretKey::generate);

        #[cfg(target_os = "android")]
        let endpoint = {
            use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

            use iroh::dns::DnsResolver;

            Endpoint::builder(presets::Minimal)
                .secret_key(secret_key.clone())
                .dns_resolver(DnsResolver::with_nameserver(SocketAddr::V4(
                    SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53),
                )))
                .relay_mode(RelayMode::Default)
                .bind()
                .await?
        };

        #[cfg(not(target_os = "android"))]
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(secret_key.clone())
            .relay_mode(RelayMode::Default)
            .bind()
            .await?;

        endpoint.online().await;

        let my_addr = endpoint.addr();
        let payload = HandshakePayload {
            public_key: *my_addr.id.as_bytes(),
            relay_url: my_addr.relay_urls().next().map(|u| u.to_string()),
            direct_addresses: my_addr.ip_addrs().copied().collect(),
        };
        let encoded_handshake = wincode::serialize(&payload)?;

        let router = Router::builder(endpoint.clone())
            .accept(ALPN, protocol)
            .spawn();

        let connection_watcher = ConnectionStateWatcher::new(endpoint, statusbar_handle.clone());
        tokio::task::spawn(async move { connection_watcher.watch().await });

        tokio::task::spawn(async move {
            loop {
                let res: color_eyre::Result<()> = async {
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
                    wormhole.send(encoded_handshake.clone()).await?;

                    let bytes = wormhole.receive().await?;
                    let helper_public_key = PublicKey::try_from(bytes.as_slice())?;

                    authenticated_peer
                        .lock()
                        .unwrap()
                        .replace(helper_public_key);

                    if !args.multiple_helpers {
                        // Timeout after 60s if the helper never connects via QUIC
                        tokio::time::timeout(
                            Duration::from_secs(10),
                            statusbar_handle
                                .subscribe()
                                .wait_for(|s| s.connected_helpers > 0),
                        )
                        .await
                        .map_err(|_| {
                            eyre!("Helper connected via wormhole but timed out dialing QUIC")
                        })??;

                        statusbar_handle.set_code(None);

                        let _ = statusbar_handle
                            .subscribe()
                            .wait_for(|s| s.connected_helpers == 0)
                            .await;
                        *authenticated_peer.lock().unwrap() = None;
                    }

                    Ok(())
                }
                .await;

                if res.is_err() {
                    statusbar_handle.set_code(None);
                    *authenticated_peer.lock().unwrap() = None;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), color_eyre::eyre::Error>(())
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
