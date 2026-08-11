use crate::{
    common::{ALPN, ConnectionStateWatcher},
    console::{LocalConsole, Role, StatusBarHandle, TermGuard, is_detach_key, render_local_screen},
    osc_extractor::Osc52Extractor,
    protocol::{Msg, decode, encode},
};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use crossterm::terminal::{enable_raw_mode, size};
use futures::{SinkExt, StreamExt};
use iroh::{Endpoint, PublicKey, SecretKey, endpoint::presets};
use magic_wormhole::{AppConfig, Code, MailboxConnection, Wormhole, transfer::AppVersion};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::mpsc, time::timeout};
use tokio_util::codec::LengthDelimitedCodec;

pub struct VictimHub {
    to_victim_tx: mpsc::Sender<Msg>,
    from_victim_rx: tokio::sync::Mutex<mpsc::Receiver<Msg>>,
    #[allow(unused)]
    link: Link,
}

impl VictimHub {
    pub async fn connect(
        app_config: AppConfig<AppVersion>,
        code: Code,
        statusbar_handle: StatusBarHandle,
    ) -> Result<Self> {
        let (to_victim_tx, to_victim_rx) = mpsc::channel::<Msg>(1000);
        let (from_victim_tx, from_victim_rx) = mpsc::channel::<Msg>(1000);

        let link = Link::connect(
            app_config,
            code,
            statusbar_handle,
            to_victim_rx,
            from_victim_tx,
        )
        .await?;

        Ok(Self {
            to_victim_tx,
            from_victim_rx: tokio::sync::Mutex::new(from_victim_rx),
            link,
        })
    }

    pub async fn send(&self, msg: Msg) -> Result<()> {
        self.to_victim_tx.send(msg).await.map_err(|e| anyhow!(e))
    }

    pub async fn recv(&self) -> Option<Msg> {
        self.from_victim_rx.lock().await.recv().await
    }
}

#[derive(Default)]
pub struct Helper;

impl Helper {
    pub async fn run(app_config: AppConfig<AppVersion>, code: Code) -> Result<()> {
        enable_raw_mode()?;
        let _guard = TermGuard;

        let (mut cols, mut rows) = size()?;
        let mut pty_rows = rows.saturating_sub(1).max(1);
        let vt_parser = Arc::new(Mutex::new(vt100::Parser::new(pty_rows, cols, 1000)));
        let (statusbar_handle, mut statusbar_rx) = StatusBarHandle::new(Role::Helper);

        let console = LocalConsole::new();
        let hub = VictimHub::connect(app_config, code, statusbar_handle.clone()).await?;

        // Send initial terminal dimensions to victim shell
        hub.send(Msg::Resize {
            cols,
            rows: pty_rows,
        })
        .await?;

        #[cfg(unix)]
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

        let mut osc_extractor = Osc52Extractor::default();

        let res: Result<()> = loop {
            #[cfg(unix)]
            let sigwinch_recv = sigwinch.recv();
            #[cfg(not(unix))]
            let sigwinch_recv = std::future::pending::<Option<()>>();

            tokio::select! {
                // Status bar update -> render frame locally
                _ = statusbar_rx.changed() => {
                    let frame = render_local_screen(vt_parser.clone(), &statusbar_rx.borrow(), cols, rows);
                    console.write_stdout(frame).await?;
                }

                // Raw stdin bytes -> filter -> send to victim
                Some(bytes) = console.read_stdin() => {
                    if bytes.is_empty() {
                        continue;
                    }

                    if is_detach_key(&bytes) {
                        break Ok(());
                    }

                    hub.send(Msg::Data(bytes)).await?;
                }

                // Window resize signal (SIGWINCH)
                _ = sigwinch_recv => {
                    if let Ok((new_cols, new_rows)) = size() {
                        cols = new_cols;
                        rows = new_rows;
                        pty_rows = rows.saturating_sub(1).max(1);

                        vt_parser.lock().unwrap().screen_mut().set_size(pty_rows, cols);

                        // Notify victim shell of new dimensions
                        hub.send(Msg::Resize { cols: new_cols, rows: pty_rows }).await?;

                        let frame = render_local_screen(vt_parser.clone(), &statusbar_rx.borrow(), cols, rows);
                        console.write_stdout(frame).await?;
                    }
                }

                // Victim output -> feed into local VT parser & render screen
                incoming = hub.recv() => {
                    match incoming {
                        Some(Msg::Data(bytes)) => {
                            // Isolate OSC 52 sequences and send directly to local terminal (Alacritty)
                            let osc52_bytes = osc_extractor.extract(&bytes);
                            if !osc52_bytes.is_empty() {
                                console.write_stdout(osc52_bytes).await?;
                            }

                            vt_parser.lock().unwrap().process(&bytes);
                            let frame = render_local_screen(vt_parser.clone(), &statusbar_rx.borrow(), cols, rows);
                            console.write_stdout(frame).await?;
                        }
                        Some(Msg::Bye) => break Ok(()),
                        Some(Msg::ConnectedHelpers(n)) => {
                            statusbar_handle.set_connected(n);
                        },
                        Some(Msg::Resize {..}) => {},
                        None => break Err(anyhow!("channel closed")).context("Recv from victim"),
                    }
                }
            }
        };

        let _ = hub.send(Msg::Bye).await;
        console.flush_and_close().await;

        res
    }
}

struct Link {
    #[allow(unused)]
    endpoint: Endpoint,
}

impl Link {
    async fn connect(
        app_config: AppConfig<AppVersion>,
        code: Code,
        statusbar_handle: StatusBarHandle,
        mut to_victim_rx: mpsc::Receiver<Msg>,
        from_victim_tx: mpsc::Sender<Msg>,
    ) -> anyhow::Result<Self> {
        let mailbox = MailboxConnection::connect(app_config, code, false).await?;
        let mut wormhole = Wormhole::connect(mailbox).await?;
        let mut buf = [0u8; 32];
        let bytes = wormhole.receive().await?;
        buf.copy_from_slice(&bytes);
        let victim_public_key: PublicKey = PublicKey::from_bytes(&buf)?;

        let secret_key = SecretKey::generate();

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .bind()
            .await?;

        let connection_watcher =
            ConnectionStateWatcher::new(endpoint.clone(), statusbar_handle.clone());
        tokio::task::spawn(async move { connection_watcher.watch().await });

        let connection = timeout(
            Duration::from_secs(5),
            endpoint.connect(victim_public_key, ALPN),
        )
        .await??;

        // Note: Helper is the initiator, so it opens the bi-directional stream using open_bi()
        let (tx, rx) = connection.open_bi().await?;
        let mut raw_writer = tokio_util::codec::FramedWrite::new(tx, LengthDelimitedCodec::new());
        let mut raw_reader = tokio_util::codec::FramedRead::new(rx, LengthDelimitedCodec::new());

        // Victim -> Helper
        let victim_helper = tokio::spawn(async move {
            loop {
                let bytes = raw_reader.next().await.ok_or(anyhow!("reader is none"))??;
                let msg = decode(&bytes)?;
                from_victim_tx.send(msg).await?;
            }

            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        // Helper -> Victim
        let helper_victim = tokio::spawn(async move {
            loop {
                let msg = to_victim_rx
                    .recv()
                    .await
                    .ok_or(anyhow!("channel closed"))
                    .context("Helper to victim")?;
                raw_writer.send(Bytes::from(encode(&msg)?)).await?;
            }

            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        // Spawn background task supervisor so Link::connect returns immediately
        tokio::spawn(async move {
            let (res, _, _) = futures::future::select_all(vec![victim_helper, helper_victim]).await;
            if let Err(e) = res {
                eprintln!("Link task finished with error: {e}");
            }
        });

        Ok(Self { endpoint })
    }
}
