use std::{
    fmt,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::anyhow;
use crossterm::{
    cursor::{MoveTo, Show},
    execute, queue,
    style::{Attribute, Print, SetAttribute, Stylize},
    terminal::{Clear, ClearType, LeaveAlternateScreen, disable_raw_mode},
};
use magic_wormhole::Code;
use tokio::sync::{mpsc, watch};

#[derive(Clone, Debug)]
pub enum InternetState {
    Online(Duration),
    Offline,
}

impl fmt::Display for InternetState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InternetState::Online(duration) => {
                write!(f, "Online ({} ms)", duration.as_millis())
            }
            InternetState::Offline => write!(f, "Offline"),
        }
    }
}

#[derive(Clone, Debug, strum::Display)]
pub enum Role {
    Victim,
    Helper,
}

#[derive(Clone, Debug)]
pub struct StatusBarState {
    code: Option<Code>,
    role: Role,
    connected_helpers: u8,
    internet_state: InternetState,
}

impl StatusBarState {
    fn new(role: Role) -> Self {
        Self {
            code: None,
            role,
            connected_helpers: 0,
            internet_state: InternetState::Offline,
        }
    }

    pub fn render_to(&self, buf: &mut Vec<u8>, cols: u16) {
        let code = match &self.code {
            Some(code) => code.to_string(),
            None => "No code".to_string(),
        };

        let title = format!("rescue-shell {}", env!("CARGO_PKG_VERSION"));
        let role = self.role.to_string();
        let connected_helpers = format!("Connected: {}", self.connected_helpers);
        let internet_state = format!("{}", self.internet_state);
        let raw_text = format!(
            "{code} | {title} | {role} | {connected_helpers} | {internet_state} | CTRL+] to exit",
        );

        let width = cols as usize;
        let char_count = raw_text.chars().count();

        // Safely truncate UTF-8 string or pad to width
        let padded_text = if char_count > width {
            raw_text.chars().take(width).collect::<String>()
        } else {
            format!("{:<width$}", raw_text, width = width)
        };

        // Render at top row (Row 0, Col 0 in 0-based crossterm coordinates)
        let _ = queue!(buf, MoveTo(0, 0), Print(padded_text.black().on_white()));
    }
}

#[derive(Clone, Debug)]
pub struct StatusBarHandle {
    tx: watch::Sender<StatusBarState>,
}

impl StatusBarHandle {
    pub fn new(role: Role) -> (Self, watch::Receiver<StatusBarState>) {
        let (tx, rx) = watch::channel(StatusBarState::new(role));
        (Self { tx }, rx)
    }

    pub fn set_code(&self, code: Option<Code>) {
        self.tx.send_modify(|s| s.code = code);
    }

    pub fn inc_connected(&self) {
        self.tx
            .send_modify(|s| s.connected_helpers = s.connected_helpers.saturating_add_signed(1));
    }

    pub fn dec_connected(&self) {
        self.tx
            .send_modify(|s| s.connected_helpers = s.connected_helpers.saturating_sub_signed(1));
    }

    pub fn set_connected(&self, n: u8) {
        self.tx.send_modify(|s| s.connected_helpers = n);
    }

    pub fn get_connected(&self) -> u8 {
        self.tx.borrow().connected_helpers
    }

    pub fn offline(&self) {
        self.tx
            .send_modify(|s| s.internet_state = InternetState::Offline);
    }

    pub fn online(&self, ping: Duration) {
        self.tx
            .send_modify(|s| s.internet_state = InternetState::Online(ping));
    }
}

pub fn render_local_screen(
    parser: Arc<Mutex<vt100::Parser>>,
    status_bar: &StatusBarState,
    cols: u16,
    total_rows: u16,
) -> Vec<u8> {
    let parser = parser.lock().unwrap();
    let mut buf = Vec::new();
    let screen = parser.screen();

    // 1. Sync physical terminal input modes (DECCKM for arrow keys, mouse, etc.)
    buf.extend_from_slice(&screen.input_mode_formatted());

    // 2. Draw status bar on physical Row 0
    status_bar.render_to(&mut buf, cols);

    // 3. Draw VT100 rows starting on physical Row 1
    for (r, row_bytes) in screen.rows_formatted(0, cols).enumerate() {
        let physical_row = (r as u16) + 1;
        if physical_row >= total_rows {
            break;
        }

        // Move to row AND clear line to the right before drawing
        let _ = queue!(
            buf,
            MoveTo(0, physical_row),
            SetAttribute(crossterm::style::Attribute::Reset),
            Clear(crossterm::terminal::ClearType::UntilNewLine)
        );
        buf.extend_from_slice(&row_bytes);
    }

    // 4. Move terminal cursor to virtual cursor position
    let (cur_r, cur_c) = screen.cursor_position();
    let _ = queue!(
        buf,
        SetAttribute(crossterm::style::Attribute::Reset),
        MoveTo(cur_c, cur_r + 1)
    );

    buf
}

/// Restore Victim host terminal mode on exit.
pub struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();

        let _ = execute!(
            stdout,
            SetAttribute(Attribute::Reset),
            LeaveAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0),
            Show
        );

        let _ = disable_raw_mode();

        println!("\r\n[session ended]");
    }
}

pub fn is_detach_key(bytes: &[u8]) -> bool {
    // Ctrl+] (0x1D) detaches locally
    bytes.contains(&0x1d)
}

pub struct LocalConsole {
    stdin_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    stdout_tx: mpsc::Sender<Vec<u8>>,
    stdout_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LocalConsole {
    pub fn new() -> Self {
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(64);
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

        let (stdout_tx, mut stdout_rx) = mpsc::channel::<Vec<u8>>(64);
        let stdout_handle = tokio::task::spawn_blocking(move || {
            let mut stdout = std::io::stdout();
            while let Some(bytes) = stdout_rx.blocking_recv() {
                if stdout.write_all(&bytes).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        });

        Self {
            stdin_rx: tokio::sync::Mutex::new(stdin_rx),
            stdout_tx,
            stdout_handle: tokio::sync::Mutex::new(Some(stdout_handle)),
        }
    }

    pub async fn read_stdin(&self) -> Option<Vec<u8>> {
        self.stdin_rx.lock().await.recv().await
    }

    pub async fn write_stdout(&self, frame: Vec<u8>) -> anyhow::Result<()> {
        self.stdout_tx.send(frame).await.map_err(|e| anyhow!(e))
    }

    pub async fn flush_and_close(self) {
        drop(self.stdout_tx);
        let handle = self.stdout_handle.lock().await.take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}
