use crate::{
    ConnectArgs, app_config,
    common::{ALPN, ConnectionStateWatcher},
    console::{
        LocalConsole, Osc52Extractor, Role, SCROLLBACK_LINES, SgrMouseTranslator, StatusBarHandle,
        is_detach_key, is_sgr_mouse, scroll_delta, window_change_signal,
    },
    protocol::{Encoder, TIMEOUT, TerminalSize, ToHelper, ToVictim},
};
use anyhow::{Context, Result, anyhow};
use crossterm::terminal::size;
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
    to_victim_tx: mpsc::Sender<ToVictim>,
    from_victim_rx: tokio::sync::Mutex<mpsc::Receiver<ToHelper>>,
    #[allow(unused)]
    link: Link,
}

impl VictimHub {
    pub async fn connect(args: ConnectArgs, statusbar_handle: StatusBarHandle) -> Result<Self> {
        let (to_victim_tx, to_victim_rx) = mpsc::channel::<ToVictim>(64);
        let (from_victim_tx, from_victim_rx) = mpsc::channel::<ToHelper>(64);

        let link = Link::connect(args, statusbar_handle, to_victim_rx, from_victim_tx).await?;

        Ok(Self {
            to_victim_tx,
            from_victim_rx: tokio::sync::Mutex::new(from_victim_rx),
            link,
        })
    }

    pub async fn send(&self, msg: ToVictim) -> Result<()> {
        self.to_victim_tx.send(msg).await.map_err(|e| anyhow!(e))
    }

    pub async fn recv(&self) -> Option<ToHelper> {
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
        let id = getrandom::u64()?.into();
        let (mut cols, mut rows) = size()?;
        let mut pty_rows = rows.saturating_sub(1).max(1);
        let vt_parser = Arc::new(Mutex::new(vt100::Parser::new(
            pty_rows,
            cols,
            SCROLLBACK_LINES,
        )));
        let statusbar_handle = StatusBarHandle::new(Role::Helper);
        let mut statusbar_rx = statusbar_handle.subscribe();

        let mut osc52_extractor = Osc52Extractor::default();
        let hub = VictimHub::connect(args, statusbar_handle.clone()).await?;

        // Send initial terminal dimensions to victim shell
        hub.send(ToVictim::SizeHint {
            id,
            size: TerminalSize { cols, pty_rows },
        })
        .await?;

        let mut sigwinch = window_change_signal();
        let mut screen_size_resend =
            tokio::time::interval(Duration::from_secs((TIMEOUT / 2).as_secs()));

        let mut mouse_translator = SgrMouseTranslator::default();
        let mut console = LocalConsole::new(vt_parser.clone(), &statusbar_handle)?;

        let res: Result<()> = loop {
            tokio::select! {
                // Peroidically resend term size to keep TermSizeNegotiator alive
                _ = screen_size_resend.tick() => {
                    hub.send(ToVictim::SizeHint { id, size: TerminalSize { cols, pty_rows } }).await?;
                }

                // Status bar update -> render frame locally
                _ = statusbar_rx.changed() => {
                    console.render().await?;
                }

                // Raw stdin bytes -> filter -> send to victim
                Some(bytes) = console.read_stdin() => {
                    if bytes.is_empty() {
                        continue;
                    }

                    if is_detach_key(&bytes) {
                        break Ok(());
                    }

                    let (alt, mouse_on, scrolled) = {
                        let p = vt_parser.lock().unwrap();
                        (
                            p.screen().alternate_screen(),
                            p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None,
                            p.screen().scrollback() > 0,
                        )
                    };

                    if !alt {
                        if let Some(delta) = scroll_delta(&bytes, pty_rows as i32) {
                            let offset = console.apply_scroll(delta);
                            hub.send(ToVictim::RequestScrollTo { offset: offset as u32 }).await?;
                            console.render().await?;
                            continue;
                        }

                        // Clicks/drag are now being reported too (we forced ?1000h). Only
                        // forward them if the remote app actually enabled mouse handling.
                        if is_sgr_mouse(&bytes) && !mouse_on {
                            continue;
                        }

                        if scrolled {
                            vt_parser.lock().unwrap().screen_mut().set_scrollback(0);
                            hub.send(ToVictim::RequestScrollTo { offset: 0 }).await?;
                            console.render().await?;
                        }
                    }


                    let adjusted = match mouse_translator.translate(&bytes) {
                        Some(adj) => adj,
                        None => continue,
                    };

                    hub.send(ToVictim::Data(adjusted)).await?;
                }

                // Window resize signal (SIGWINCH)
                _ = sigwinch.recv() => {
                    if let Ok((new_cols, new_rows)) = size() {
                        cols = new_cols;
                        rows = new_rows;
                        pty_rows = rows.saturating_sub(1).max(1);
                        hub.send(ToVictim::SizeHint { id, size: TerminalSize { cols, pty_rows}} ).await?;
                    }
                }

                // Victim output -> feed into local VT parser & render screen
                incoming = hub.recv() => {
                    match incoming {
                        Some(ToHelper::Data(bytes)) => {
                            if let Some(output) = osc52_extractor.extract(&bytes) {
                                console.write_stdout(output).await?;
                            }

                            vt_parser.lock().unwrap().process(&bytes);
                            console.render().await?;
                        }
                        Some(ToHelper::Bye) => break Ok(()),
                        Some(ToHelper::ConnectedHelpers(n)) => {
                            statusbar_handle.set_connected(n);
                        },
                        Some(ToHelper::SetSize(size)) => {
                            // Negotiated size from victim
                            vt_parser.lock().unwrap().screen_mut().set_size(size.pty_rows, size.cols);
                            console.render().await?;
                            // TODO: draw border if screen size < term size
                        },
                        Some(ToHelper::ScrollTo { offset }) => {
                            vt_parser.lock().unwrap().screen_mut().set_scrollback(offset as usize);
                            console.render().await?;
                        }
                        None => break Err(anyhow!("channel closed")).context("Recv from victim"),
                    }
                }
            }
        };

        let _ = hub.send(ToVictim::Bye { id }).await;
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
        mut to_victim_rx: mpsc::Receiver<ToVictim>,
        from_victim_tx: mpsc::Sender<ToHelper>,
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
                let msg = ToHelper::decode(&bytes)?;
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

                raw_writer.send(msg.encode()?).await?;
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
