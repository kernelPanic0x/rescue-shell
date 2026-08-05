// src/helper.rs
use crate::protocol::{Msg, decode, encode};
use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use futures::StreamExt;
use magic_wormhole::{AppID, Code, MailboxConnection, Wormhole, transfer::APP_CONFIG};
use std::io::Write;

/// Restore the terminal no matter how we exit.
struct RawGuard;
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

pub async fn run(code: Code) -> Result<()> {
    // ── 1. Connect ────────────────────────────────────────────────
    println!("Connecting to {code} ...");
    let mailbox = MailboxConnection::connect(
        APP_CONFIG.id(AppID::new("rescue-shell-v1")),
        code,
        true, // allocate (passphrase already supplied by code)
    )
    .await?;
    let mut wormhole = Wormhole::connect(mailbox).await?;
    println!("Connected. Session started — Ctrl+] to detach.\n");

    // ── 2. Raw mode + initial size ────────────────────────────────
    enable_raw_mode()?;
    let _guard = RawGuard;

    let (cols, rows) = size()?;
    wormhole.send(encode(&Msg::Resize { cols, rows })?).await?;

    // ── 3. stdout pump: write incoming bytes straight to screen ───
    let mut stdout = std::io::stdout();
    let mut events = EventStream::new();

    let result = loop {
        tokio::select! {
            // Local keystrokes -> victim (or intercepted locally)
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(k))) => {
                        if is_escape(k) { break Ok(()); }   // Ctrl+] detach
                        if let Some(bytes) = key_to_bytes(k) {
                            wormhole.send(encode(&Msg::Data(bytes))?).await?;
                        }
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        wormhole.send(encode(&Msg::Resize { cols, rows })?).await?;
                    }
                    Some(Ok(Event::Paste(s))) => {
                        wormhole.send(encode(&Msg::Data(s.into_bytes()))?).await?;
                    }
                    Some(Err(_)) | None => break Ok(()),
                    _ => {}
                }
            }

            // Victim output -> my screen
            incoming = wormhole.receive() => {
                match incoming {
                    Ok(raw) => match decode(&raw)? {
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

    let _ = wormhole.send(encode(&Msg::Bye)?).await;
    println!("\r\n[session ended]");
    result
}

fn is_escape(k: KeyEvent) -> bool {
    k.code == KeyCode::Char(']') && k.modifiers.contains(KeyModifiers::CONTROL)
}

/// Translate crossterm keys into the bytes a real terminal would send.
fn key_to_bytes(k: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let v: Vec<u8> = match k.code {
        KeyCode::Char(c) if ctrl => {
            // Ctrl+A..Z => 0x01..0x1A  (what the shell expects)
            let c = c.to_ascii_lowercase();
            if ('a'..='z').contains(&c) {
                vec![c as u8 - b'a' + 1]
            } else {
                return None;
            }
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        _ => return None,
    };
    Some(v)
}
