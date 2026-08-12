use std::{
    fmt,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::anyhow;
use bytes::Bytes;
use crossterm::{
    cursor::{MoveTo, Show},
    execute, queue,
    style::{Attribute, Print, SetAttribute, Stylize},
    terminal::{Clear, ClearType, LeaveAlternateScreen, disable_raw_mode},
};
use magic_wormhole::Code;
use termwiz::escape::{
    Action, CSI, OneBased,
    csi::{
        Cursor, Device, DeviceAttribute, DeviceAttributeCodes, DeviceAttributeFlags,
        DeviceAttributes,
    },
    parser::Parser,
};
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
    pub code: Option<Code>,
    pub role: Role,
    pub connected_helpers: u8,
    pub internet_state: InternetState,
    pub tick: usize,
}

impl StatusBarState {
    fn new(role: Role) -> Self {
        Self {
            code: None,
            role,
            connected_helpers: 0,
            internet_state: InternetState::Offline,
            tick: 0,
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

        // 2. Marquee logic: pad if fits, scroll if too long
        let padded_text = if char_count <= width {
            // Fits inside terminal width: pad right with spaces
            format!("{:<width$}", raw_text, width = width)
        } else {
            // Exceeds terminal width: cycle continuously with a separator
            let separator = "   ***   ";
            let full_text = format!("{raw_text}{separator}");
            let total_chars = full_text.chars().count();

            let offset = self.tick % total_chars;

            // Safely slice full_text across UTF-8 boundaries starting at `offset`
            full_text
                .chars()
                .cycle()
                .skip(offset)
                .take(width)
                .collect::<String>()
        };

        // Render at top row
        let _ = queue!(buf, MoveTo(0, 0), Print(padded_text.black().on_white()));
    }
}

#[derive(Clone, Debug)]
pub struct StatusBarHandle {
    tx: watch::Sender<StatusBarState>,
}

impl StatusBarHandle {
    pub fn new(role: Role) -> Self {
        let (tx, _) = watch::channel(StatusBarState::new(role));

        let tx_clone = tx.clone();
        tokio::spawn(async move {
            // Adjust scroll speed here (e.g. 300ms per character step)
            let mut interval = tokio::time::interval(Duration::from_millis(300));
            loop {
                interval.tick().await;

                // Stop loop automatically when receiver is dropped
                if tx_clone.receiver_count() == 0 {
                    break;
                }

                tx_clone.send_modify(|s| s.tick = s.tick.wrapping_add(1));
            }
        });

        Self { tx }
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

    pub fn subscribe(&self) -> watch::Receiver<StatusBarState> {
        self.tx.subscribe()
    }
}

pub fn render_local_screen(
    parser: Arc<Mutex<vt100::Parser>>,
    status_bar: &StatusBarState,
    cols: u16,
    total_rows: u16,
) -> Bytes {
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

    Bytes::from(buf)
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
    stdin_rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    stdout_tx: mpsc::Sender<Bytes>,
    stdout_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LocalConsole {
    pub fn new() -> Self {
        let (stdin_tx, stdin_rx) = mpsc::channel::<Bytes>(64);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 1024];

            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdin_tx
                            .blocking_send(Bytes::copy_from_slice(&buf[..n]))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        let (stdout_tx, mut stdout_rx) = mpsc::channel::<Bytes>(64);
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

    pub async fn read_stdin(&self) -> Option<Bytes> {
        self.stdin_rx
            .lock()
            .await
            .recv()
            .await
            .map(|v| Bytes::from(v))
    }

    pub async fn write_stdout(&self, frame: Bytes) -> anyhow::Result<()> {
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

pub fn process_pty_output<W: Write>(
    chunk: &[u8],
    vt100_cursor: (u16, u16), // (row, col) 0-indexed from your vt100 parser
    pty_writer: &mut W,
    parser: &mut Parser,
) -> anyhow::Result<()> {
    let actions = parser.parse_as_vec(chunk);

    // Collect replies in bytes, then write everything in one shot so a chunk
    // containing several queries (fish sends DA1+DA2+DSR+CPR together) is
    // answered atomically. `Vec<u8>` implements std::io::Write, so `write!`
    // renders a typed Action into bytes via its `Display` impl.
    let mut reply = Vec::new();

    for action in actions {
        if let Action::CSI(csi) = &action {
            match csi {
                CSI::Device(boxed) => match boxed.as_ref() {
                    // ---------- ESC [ c   (DA1, Primary Device Attributes) ----------
                    // Query parses as `RequestPrimaryDeviceAttributes`, NOT a
                    // `DeviceAttributes::Query(..)`.
                    Device::RequestPrimaryDeviceAttributes => {
                        // Report a VT220 with a few common attribute codes.
                        // This renders as: \x1b[?62;1;9;15;22;29c
                        let flags = DeviceAttributeFlags::new(vec![
                            DeviceAttribute::Code(DeviceAttributeCodes::Columns132),
                            DeviceAttribute::Code(
                                DeviceAttributeCodes::NationalReplacementCharsets,
                            ),
                            DeviceAttribute::Code(DeviceAttributeCodes::TechnicalCharacters),
                            DeviceAttribute::Code(DeviceAttributeCodes::AnsiColor),
                            DeviceAttribute::Code(DeviceAttributeCodes::AnsiTextLocator),
                        ]);
                        let response = Action::CSI(CSI::Device(Box::new(
                            Device::DeviceAttributes(DeviceAttributes::Vt220(flags)),
                        )));
                        write!(reply, "{}", response)?;
                    }

                    // ---------- ESC [ > c   (DA2, Secondary Device Attributes) ----------
                    // fish specifically reads the "Pp;Pv;Pc" reply: Pp=1 => VT220,
                    // Pv = firmware/patch version. 277 = xterm patch level, which
                    // makes fish treat us as an xterm-compatible terminal
                    // (sets ttymouse=sgr, 24-bit color, etc.). No typed reply
                    // variant exists, so emit the bytes directly (as wezterm does).
                    Device::RequestSecondaryDeviceAttributes => {
                        reply.extend_from_slice(b"\x1b[>1;277;0c");
                    }

                    // ---------- ESC [ > q   (XTVERSION, terminal name+version) ----------
                    // Reply is a DCS sequence: ESC P >| <prog> <version> ESC \
                    Device::RequestTerminalNameAndVersion => {
                        reply.extend_from_slice(b"\x1bP>|termwiz 0.23.3\x1b\\");
                    }

                    // ---------- ESC [ 5 n   (DSR, Device Status Report) ----------
                    // Parses as `Device::StatusReport` (NOT a Cursor variant).
                    // Answer "ready, no malfunction" with ESC [ 0 n, exactly
                    // like wezterm does (there is no typed response variant).
                    Device::StatusReport => {
                        reply.extend_from_slice(b"\x1b[0n");
                    }

                    _ => {}
                },

                // ---------- ESC [ 6 n   (CPR / DSR-6, Cursor Position Report) ----------
                // Parses as `Cursor::RequestActivePositionReport`. The reply
                // must be built as the struct variant `ActivePositionReport`
                // (NOT `Cursor::Position`, which is CUP = ESC [ r;c H).
                CSI::Cursor(Cursor::RequestActivePositionReport) => {
                    let (row_0_idx, col_0_idx) = vt100_cursor;
                    let response = Action::CSI(CSI::Cursor(Cursor::ActivePositionReport {
                        line: OneBased::from_zero_based(row_0_idx as u32),
                        col: OneBased::from_zero_based(col_0_idx as u32),
                    }));
                    write!(reply, "{}", response)?; // renders "\x1b[{line};{col}R"
                }

                _ => {}
            }
        }
    }

    if !reply.is_empty() {
        pty_writer.write_all(&reply)?;
        pty_writer.flush()?;
    }

    Ok(())
}
