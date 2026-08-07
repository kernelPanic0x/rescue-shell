use crate::{common::is_detach_key, link, protocol::Msg};
use anyhow::{Context, Result, bail};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use magic_wormhole::transit::Transit;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

/// Restore Victim host terminal mode on exit.
struct RawGuard;
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

#[derive(Default)]
pub struct Victim;

impl Victim {
    pub async fn run(transit: Transit) -> Result<()> {
        let (mut tx, mut rx) = link::channel(transit);

        // ── 1. Enable Raw Mode on Victim's local host terminal ─────
        enable_raw_mode()?;
        let _guard = RawGuard;

        let (cols, rows) = size().unwrap_or((80, 24));

        // ── 2. Initialize PTY ──────────────────────────────────────
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
        drop(pair.slave); // slave belongs to child now

        let master: Box<dyn MasterPty + Send> = pair.master;
        let mut pty_reader = master.try_clone_reader()?;
        let pty_writer = master.take_writer()?;

        // ── 3. Threads for PTY Blocking I/O ───────────────────────

        // Reader Thread: PTY output -> async channel
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

        // Writer Thread: async channel -> PTY input
        let (to_pty_tx, mut to_pty_rx) = mpsc::channel::<Vec<u8>>(64);
        let mut pty_writer = pty_writer;
        tokio::task::spawn_blocking(move || {
            while let Some(bytes) = to_pty_rx.blocking_recv() {
                if pty_writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });

        let mut child_exit = tokio::task::spawn_blocking(move || child.wait());

        #[cfg(unix)]
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

        let mut stdout = tokio::io::stdout();
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1024];

        // ── 4. Main Event Multiplexer Loop ────────────────────────
        let result = loop {
            #[cfg(unix)]
            let sigwinch_recv = sigwinch.recv();
            #[cfg(not(unix))]
            let sigwinch_recv = std::future::pending::<Option<()>>();

            tokio::select! {
                // Shell output -> Mirror locally AND send to Helper
                Some(bytes) = pty_out_rx.recv() => {
                    stdout.write_all(&bytes).await?;
                    stdout.flush().await?;
                    tx.send(&Msg::Data(bytes)).await?;
                }

                // Victim local typing -> Send to PTY
                res = stdin.read(&mut buf) => {
                    match res {
                        Ok(0) => bail!("No more input on stdin"),
                        Ok(n) => {
                            let bytes = buf[..n].to_vec();

                            if is_detach_key(&bytes) {
                                break Ok(());
                            }

                            to_pty_tx.send(bytes).await?;
                        }
                        Err(e) => bail!(e),
                    }
                }

                // Remote messages from Helper -> Send to PTY / Resize
                incoming = rx.recv() => {
                    match incoming {
                        Ok(raw) => match raw {
                            Msg::Data(bytes) => {
                                to_pty_tx.send(bytes).await?;
                            }
                            Msg::Resize { cols, rows } => {
                                master.resize(PtySize {
                                    rows, cols, pixel_width: 0, pixel_height: 0,
                                })?;
                            }
                            Msg::Bye => break Ok(()),
                        },
                        Err(e) => break Err(e),
                    }
                }

                // Victim window resized locally -> update PTY master
                _ = sigwinch_recv => {
                    if let Ok((cols, rows)) = size() {
                        let _ = master.resize(PtySize {
                            rows, cols, pixel_width: 0, pixel_height: 0,
                        });
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

        println!("[session ended]");
        tx.send(&Msg::Bye).await?;
        result
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
