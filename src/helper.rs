use crate::{
    ConnectArgs, app_config,
    common::{ALPN, ConnectionStateWatcher},
    console::{
        LocalConsole, Osc52Extractor, Role, StatusBarHandle, TermGuard, is_detach_key,
        render_local_screen, window_change_signal,
    },
    protocol::{Msg, decode, encode},
};
use anyhow::{Context, Result, anyhow};
use crossterm::terminal::{enable_raw_mode, size};
use futures::{SinkExt, StreamExt};
use iroh::{Endpoint, PublicKey, SecretKey, endpoint::presets};
use magic_wormhole::{MailboxConnection, Wormhole};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::BufReader, sync::mpsc, time::timeout};
use tokio_util::codec::LengthDelimitedCodec;

pub struct VictimHub {
    to_victim_tx: mpsc::Sender<Msg>,
    from_victim_rx: tokio::sync::Mutex<mpsc::Receiver<Msg>>,
    #[allow(unused)]
    link: Link,
}

impl VictimHub {
    pub async fn connect(args: ConnectArgs, statusbar_handle: StatusBarHandle) -> Result<Self> {
        let (to_victim_tx, to_victim_rx) = mpsc::channel::<Msg>(64);
        let (from_victim_tx, from_victim_rx) = mpsc::channel::<Msg>(64);

        let link = Link::connect(args, statusbar_handle, to_victim_rx, from_victim_tx).await?;

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

    pub async fn shutdown(&self) {
        self.link.shutdown().await
    }
}

#[derive(Default)]
pub struct Helper;

impl Helper {
    pub async fn run(args: ConnectArgs) -> Result<()> {
        enable_raw_mode()?;
        #[cfg(windows)]
        crate::console::enable_vt_input()?;
        let _guard = TermGuard;

        let (mut cols, mut rows) = size()?;
        let mut pty_rows = rows.saturating_sub(1).max(1);
        let vt_parser = Arc::new(Mutex::new(vt100::Parser::new(pty_rows, cols, 1000)));
        let statusbar_handle = StatusBarHandle::new(Role::Helper);
        let mut statusbar_rx = statusbar_handle.subscribe();

        let console = LocalConsole::new();
        let mut osc52_extractor = Osc52Extractor::default();
        let hub = VictimHub::connect(args, statusbar_handle.clone()).await?;

        // Send initial terminal dimensions to victim shell
        hub.send(Msg::Resize {
            cols,
            rows: pty_rows,
        })
        .await?;

        let mut sigwinch = window_change_signal();

        let res: Result<()> = loop {
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
                _ = sigwinch.recv() => {
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
                            if let Some(output) = osc52_extractor.extract(&bytes) {
                                console.write_stdout(output).await?;
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
        hub.shutdown().await;
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
        args: ConnectArgs,
        statusbar_handle: StatusBarHandle,
        mut to_victim_rx: mpsc::Receiver<Msg>,
        from_victim_tx: mpsc::Sender<Msg>,
    ) -> anyhow::Result<Self> {
        let mailbox = MailboxConnection::connect(
            app_config(&args.common),
            args.common.code.expect("code always set"),
            false,
        )
        .await?;
        let mut wormhole = Wormhole::connect(mailbox).await?;
        let mut buf = [0u8; 32];
        let bytes = wormhole.receive().await?;
        buf.copy_from_slice(&bytes);
        let victim_public_key: PublicKey = PublicKey::from_bytes(&buf)?;

        let secret_key = args
            .common
            .private_key
            .clone()
            .unwrap_or_else(SecretKey::generate);

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
        let encoder = async_compression::tokio::write::Lz4Encoder::new(tx);
        let decoder = async_compression::tokio::bufread::Lz4Decoder::new(BufReader::new(rx));
        let mut raw_writer =
            tokio_util::codec::FramedWrite::new(encoder, LengthDelimitedCodec::new());
        let mut raw_reader =
            tokio_util::codec::FramedRead::new(decoder, LengthDelimitedCodec::new());

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

                raw_writer.send(encode(&msg)?).await?;
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

    pub async fn shutdown(&self) {
        self.endpoint.close().await
    }
}
