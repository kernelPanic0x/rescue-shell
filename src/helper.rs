use crate::{link, protocol::Msg};
use anyhow::{Result, bail};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use magic_wormhole::transit::Transit;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

        #[cfg(unix)]
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

        let mut stdout = tokio::io::stdout();
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1024];

        let result = loop {
            #[cfg(unix)]
            let sigwinch_recv = sigwinch.recv();
            #[cfg(not(unix))]
            let sigwinch_recv = std::future::pending::<Option<()>>();

            tokio::select! {
                // Raw stdin bytes -> victim
                res = stdin.read(&mut buf) => {
                    match res {
                        Ok(0) => bail!("No more input from stdin"),
                        Ok(n) => {
                            let bytes = buf[..n].to_vec();

                            // Ctrl+] is ASCII 0x1D (29) -> Detach shortcut
                            if bytes.contains(&0x1d) {
                                break Ok(());
                            }

                            tx.send(&Msg::Data(bytes)).await?;
                        }
                        Err(e) => bail!(e),
                    }
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
                                stdout.write_all(&bytes).await?;
                                stdout.flush().await?;
                            }
                            Msg::Bye => break Ok(()),
                            _ => {}
                        },
                        Err(e) => break Err(e),
                    }
                }
            }
        };

        tx.send(&Msg::Bye).await?;
        println!("\r\n[session ended]");
        result
    }
}
