use crate::{
    link,
    protocol::{Msg, decode, encode},
};
use anyhow::Result;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use magic_wormhole::transit::Transit;
use std::io::{Read, Write};
use tokio::sync::mpsc;

/// Restore the terminal no matter how we exit.
struct RawGuard;
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

#[derive(Default)]
pub struct Helper;

impl Helper {
    pub async fn run(transit: Transit) -> Result<()> {
        let (mut tx, mut rx) = link::channel(transit);

        enable_raw_mode()?;
        let _guard = RawGuard;

        let (cols, rows) = size()?;
        tx.send(&Msg::Resize { cols, rows }).await?;

        let mut stdout = std::io::stdout();

        // Pump raw stdin bytes to an async channel
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

        #[cfg(unix)]
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

        let result = loop {
            #[cfg(unix)]
            let sigwinch_recv = sigwinch.recv();
            #[cfg(not(unix))]
            let sigwinch_recv = std::future::pending::<Option<()>>();

            tokio::select! {
                // Raw stdin bytes -> victim
                Some(bytes) = stdin_rx.recv() => {
                    // Ctrl+] is ASCII 0x1D (29) -> Detach shortcut
                    if bytes.contains(&0x1d) {
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
                                stdout.write_all(&bytes)?;
                                stdout.flush()?;
                            }
                            Msg::Bye => break Ok(()),
                            _ => {}
                        },
                        Err(e) => break Err(e.into()),
                    }
                }
            }
        };

        let _ = tx.send(&Msg::Bye).await?;
        println!("\r\n[session ended]");
        result
    }
}
