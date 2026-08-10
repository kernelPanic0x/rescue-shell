use crate::{
    common::{TermGuard, is_detach_key},
    link,
    osc_filter::OscFilter,
    protocol::Msg,
};
use anyhow::Result;
use crossterm::{
    cursor::{MoveTo, Show},
    execute,
    style::{Attribute, SetAttribute},
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size},
};
use magic_wormhole::transit::Transit;
use std::io::{Read, Write};
use tokio::sync::mpsc;

#[derive(Default)]
pub struct Helper;

impl Helper {
    pub async fn run(transit: Transit) -> Result<()> {
        let (mut tx, mut rx) = link::channel(transit);

        enable_raw_mode()?;
        let _guard = TermGuard;

        let (cols, rows) = size()?;
        tx.send(&Msg::Resize { cols, rows }).await?;

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

        let mut filter = OscFilter::default();

        // ── 3. Main Event Loop ─────────────────────────────────────
        let result = loop {
            #[cfg(unix)]
            let sigwinch_recv = sigwinch.recv();
            #[cfg(not(unix))]
            let sigwinch_recv = std::future::pending::<Option<()>>();

            tokio::select! {
                // Raw stdin bytes -> filter -> send to victim
                Some(raw_bytes) = stdin_rx.recv() => {
                    let bytes = filter.filter(&raw_bytes);

                    if bytes.is_empty() {
                        continue;
                    }

                    if is_detach_key(&bytes) {
                        break Ok(());
                    }

                    tx.send(&Msg::Data(bytes)).await?;
                }

                // Window resize signal (SIGWINCH)
                _ = sigwinch_recv => {
                    if let Ok((cols, rows)) = size() {
                        tx.send(&Msg::Resize { cols, rows }).await?;
                    }
                }

                // Victim output -> my screen
                incoming = rx.recv() => {
                    match incoming {
                        Ok(raw) => match raw {
                            Msg::Data(bytes) => {
                                let _ = stdout_tx.send(bytes).await;
                            }
                            Msg::Bye => break Ok(()),
                            _ => {}
                        },
                        Err(e) => break Err(e),
                    }
                }
            }
        };

        todo!();
        // let _ = to_helpers.send(Msg::Bye);

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
        result
    }
}
