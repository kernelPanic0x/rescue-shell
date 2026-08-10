use crate::{
    common::{ALPN, ConnectionStateWatcher, TermGuard, is_detach_key},
    osc_extractor::Osc52Extractor,
    protocol::{Msg, decode, encode},
    screen::{Role, StatusBarHandle, render_local_screen},
};
use anyhow::{Result, anyhow};
use bytes::Bytes;
use crossterm::{
    cursor::{MoveTo, Show},
    execute,
    style::{Attribute, SetAttribute},
    terminal::{Clear, ClearType, enable_raw_mode, size},
};
use futures::{SinkExt, StreamExt};
use iroh::{Endpoint, PublicKey, SecretKey, endpoint::presets};
use magic_wormhole::{AppConfig, Code, MailboxConnection, Wormhole, transfer::AppVersion};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::mpsc, time::timeout};
use tokio_util::codec::LengthDelimitedCodec;

struct Link {
    #[allow(dead_code)]
    endpoint: Endpoint,
}

impl Link {
    async fn new(
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
                let msg = to_victim_rx.recv().await.ok_or(anyhow!("channel closed"))?;
                raw_writer.send(Bytes::from(encode(&msg)?)).await?;
            }

            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        // Spawn background task supervisor so Link::new returns immediately
        tokio::spawn(async move {
            let (res, _, _) = futures::future::select_all(vec![victim_helper, helper_victim]).await;
            if let Err(e) = res {
                eprintln!("Link task finished with error: {e}");
            }
        });

        Ok(Self { endpoint })
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

        let (to_victim_tx, to_victim_rx) = mpsc::channel::<Msg>(1000);
        let (from_victim_tx, mut from_victim_rx) = mpsc::channel::<Msg>(1000);
        let _link = Link::new(
            app_config,
            code,
            statusbar_handle,
            to_victim_rx,
            from_victim_tx,
        )
        .await?;

        // Send initial terminal dimensions to victim shell
        let _ = to_victim_tx
            .send(Msg::Resize {
                cols,
                rows: pty_rows,
            })
            .await;

        #[cfg(unix)]
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

        // ── 1. Stdin Reader Thread ─────────────────────────────────
        // Use std::thread so Tokio runtime shutdown won't wait for the read() syscall on exit.
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

        // ── 2. Stdout Writer Thread ────────────────────────────────
        let (stdout_tx, mut stdout_rx) = mpsc::channel::<Vec<u8>>(64);
        let stdout_handle = tokio::task::spawn_blocking(move || {
            let mut stdout = std::io::stdout();
            while let Some(bytes) = stdout_rx.blocking_recv() {
                if stdout.write_all(&bytes).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        });

        let mut osc_extractor = Osc52Extractor::default();

        // ── 3. Main Event Loop ─────────────────────────────────────
        let result = loop {
            #[cfg(unix)]
            let sigwinch_recv = sigwinch.recv();
            #[cfg(not(unix))]
            let sigwinch_recv = std::future::pending::<Option<()>>();

            tokio::select! {
                // Status bar update -> render frame locally
                _ = statusbar_rx.changed() => {
                    let state = statusbar_rx.borrow();
                    let frame = render_local_screen(vt_parser.clone(), &state, cols, rows);
                    stdout_tx.send(frame).await?;
                }

                // Raw stdin bytes -> filter -> send to victim
                Some(bytes) = stdin_rx.recv() => {
                    if bytes.is_empty() {
                        continue;
                    }

                    if is_detach_key(&bytes) {
                        break Ok(());
                    }

                    to_victim_tx.send(Msg::Data(bytes)).await?;
                }

                // Window resize signal (SIGWINCH)
                _ = sigwinch_recv => {
                    if let Ok((new_cols, new_rows)) = size() {
                        cols = new_cols;
                        rows = new_rows;
                        pty_rows = rows.saturating_sub(1).max(1);

                        vt_parser.lock().unwrap().screen_mut().set_size(pty_rows, cols);

                        // Notify victim shell of new dimensions
                        to_victim_tx.send(Msg::Resize { cols: new_cols, rows: pty_rows }).await?;

                        let state = statusbar_rx.borrow();
                        let frame = render_local_screen(vt_parser.clone(), &state, cols, rows);
                        stdout_tx.send(frame).await?;
                    }
                }

                // Victim output -> feed into local VT parser & render screen
                incoming = from_victim_rx.recv() => {
                    match incoming {
                        Some(raw) => match raw {
                            Msg::Data(bytes) => {
                                // Isolate OSC 52 sequences and send directly to local terminal (Alacritty)
                                let osc52_bytes = osc_extractor.extract(&bytes);
                                if !osc52_bytes.is_empty() {
                                    stdout_tx.send(osc52_bytes).await?;
                                }

                                vt_parser.lock().unwrap().process(&bytes);
                                let state = statusbar_rx.borrow();
                                let frame = render_local_screen(vt_parser.clone(), &state, cols, rows);
                                stdout_tx.send(frame).await?;
                            }
                            Msg::Bye => break Ok(()),
                            _ => {}
                        },
                        None => break Err(anyhow!("channel closed")),
                    }
                }
            }
        };

        let _ = result?;
        to_victim_tx.send(Msg::Bye).await?;

        // Drop sender and wait for stdout thread to flush remaining screen writes
        drop(stdout_tx);
        let _ = stdout_handle.await;

        // Reset terminal attributes, clear screen, and show cursor
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
