use std::{
    fmt,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::anyhow;
use bytes::{Bytes, BytesMut};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute, queue,
    style::{Attribute, Print, SetAttribute, Stylize},
    terminal::{Clear, ClearType, LeaveAlternateScreen, disable_raw_mode},
};
use magic_wormhole::Code;
use tokio::sync::{mpsc, watch};
use vte::{Params, Perform};

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

    // 5. Sync cursor visibility (DECTCEM: CSI ? 25 l / h) to the physical terminal
    if screen.hide_cursor() {
        let _ = queue!(buf, Hide);
    } else {
        let _ = queue!(buf, Show);
    }

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
    stdin_rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>, // local keyboard
    stdout_tx: mpsc::Sender<Bytes>,                      // rendered frames -> local terminal
    stdout_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LocalConsole {
    pub fn new() -> Self {
        // Keyboard reader: blocking std::io::stdin() on a plain OS thread,
        // pushed into the channel that the async loop consumes.
        let (stdin_tx, stdin_rx) = mpsc::channel::<Bytes>(64);
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match std::io::stdin().read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdin_tx
                            .blocking_send(Bytes::copy_from_slice(&buf[..n]))
                            .is_err()
                        {
                            break; // loop ended -> stop reading
                        }
                    }
                }
            }
        });

        // Local-screen writer: single std::io::stdout() task, ordered by mpsc.
        let (stdout_tx, mut stdout_rx) = mpsc::channel::<Bytes>(64);
        let stdout_handle = tokio::task::spawn_blocking(move || {
            let mut stdout = std::io::stdout();
            while let Some(b) = stdout_rx.blocking_recv() {
                if stdout.write_all(&b).is_err() || stdout.flush().is_err() {
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
        self.stdin_rx.lock().await.recv().await
    }

    pub async fn write_stdout(&self, bytes: Bytes) -> anyhow::Result<()> {
        self.stdout_tx.send(bytes).await.map_err(|e| anyhow!(e))
    }

    pub async fn flush_and_close(self) {
        drop(self.stdout_tx);
        let handle = self.stdout_handle.lock().await.take();
        if let Some(h) = handle {
            let _ = h.await; // drain remaining frames to the local terminal
        }
    }
}

/// Secondary Device Attributes reply. DCS/DA responses like this one have no
/// typed variant in termwiz (which models only the *request*), so the shell
/// wire format is a named constant. WezTerm answers identically: Pp=1 (VT220),
/// Pv=277 (xterm patch level -> fish enables ttymouse=sgr / 24-bit color).
const DA2_RESP: &[u8] = b"\x1b[>1;277;0c";

/// Device Status Report reply. termwiz only knows the query `5n`; the answer
/// "ready, no malfunction" must be emitted directly.
const DSR_OK_RESP: &[u8] = b"\x1b[0n";

/// XTVERSION reply, a DCS sequence `ESC P >| <prog> <version> ESC \`.
/// termwiz models `>q` as the request only; the response is the DCS body.
fn xtversion(program: &str, version: &str) -> Vec<u8> {
    format!("\x1bP>|{program} {version}\x1b\\").into_bytes()
}

/// OSC 52 is advertised via DA1 extension flag `52`.
/// - 62 = VT220 class, 1/9/15/22/29 = common VT220 attrs,
/// - 52 = "supports OSC 52 clipboard writes" (new agreed extension;
///   read by vim 9.1.1666+, tmux, Windows Terminal, upcoming nvim).
const DA1_RESP: &[u8] = b"\x1b[?62;1;9;15;22;29;52c";

struct PtyDispatcher<'a> {
    vt100_parser: &'a Arc<Mutex<vt100::Parser>>,
    reply: Vec<u8>,
}

impl Perform for PtyDispatcher<'_> {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Retrieve the first parameter if present, or default to 0
        let first_param = params
            .iter()
            .next()
            .and_then(|sub| sub.first())
            .copied()
            .unwrap_or(0);

        match (action, intermediates) {
            // DA1 (CSI c / CSI 0 c): answer on ALL platforms — ConPTY sends this to us.
            ('c', []) if first_param == 0 => {
                self.reply.extend_from_slice(DA1_RESP);
            }

            ('n', []) => match first_param {
                // CPR (DSR 6n): answer on ALL platforms — ConPTY needs the cursor position.
                6 => {
                    let (row, col) = self.vt100_parser.lock().unwrap().screen().cursor_position();
                    let cpr = format!("\x1b[{};{}R", row + 1, col + 1);
                    self.reply.extend_from_slice(cpr.as_bytes());
                }
                // DSR 5n: Unix only. On Windows ConPTY answers this for the shell
                // itself; emitting our own leaks an ESC keystroke.
                #[cfg(unix)]
                5 => {
                    self.reply.extend_from_slice(DSR_OK_RESP);
                }
                _ => {}
            },

            // Unix only: the shell queries us directly. On Windows these never reach
            // us (ConPTY answers them), and answering unsolicited leaks an ESC.
            #[cfg(unix)]
            ('c', b">") if first_param == 0 => {
                self.reply.extend_from_slice(DA2_RESP);
            }
            #[cfg(unix)]
            ('q', b">") => {
                self.reply.extend_from_slice(&xtversion(
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION"),
                ));
            }

            _ => {}
        }
    }
}

pub fn process_pty_output(
    chunk: &[u8],
    vt100_parser: Arc<Mutex<vt100::Parser>>,
    parser: &mut vte::Parser,
) -> anyhow::Result<Option<Bytes>> {
    let mut dispatcher = PtyDispatcher {
        vt100_parser: &vt100_parser,
        reply: Vec::new(),
    };

    parser.advance(&mut dispatcher, chunk);

    Ok((!dispatcher.reply.is_empty()).then_some(Bytes::from(dispatcher.reply)))
}

pub struct Osc52Extractor {
    parser: vte::Parser,
    buffer: BytesMut,
}

impl Default for Osc52Extractor {
    fn default() -> Self {
        let parser = vte::Parser::new();
        let buffer = BytesMut::new();
        Self { parser, buffer }
    }
}

impl Osc52Extractor {
    /// Strips everything EXCEPT complete OSC 52 escape sequences from the byte stream.
    pub fn extract(&mut self, chunk: &[u8]) -> Option<Bytes> {
        let mut handler = Osc52Handler {
            chunk: &mut self.buffer,
        };
        self.parser.advance(&mut handler, chunk);

        if self.buffer.is_empty() {
            None
        } else {
            Some(self.buffer.split().freeze())
        }
    }
}

/// Private helper that collects filtered OSC 52 sequences dispatched by `vte`
struct Osc52Handler<'a> {
    chunk: &'a mut BytesMut,
}

impl Perform for Osc52Handler<'_> {
    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        // OSC 52 sequence structure: OSC 52 ; <target> ; <base64> (ST | BEL)
        if let Some(&first) = params.first()
            && first == b"52"
            && params.len() >= 2
        {
            // Alacritty compatibility fix:
            // Replace empty/missing target (params[1]) with "c"
            let target = match params.get(1) {
                Some(t) if !t.is_empty() => *t,
                _ => b"c",
            };

            self.chunk.extend_from_slice(b"\x1b]52;");
            self.chunk.extend_from_slice(target);

            // Re-attach payload (params[2] and beyond)
            if params.len() > 2 {
                for p in &params[2..] {
                    self.chunk.extend_from_slice(b";");
                    self.chunk.extend_from_slice(p);
                }
            } else {
                self.chunk.extend_from_slice(b";");
            }

            // Re-attach original terminator
            if bell_terminated {
                self.chunk.extend_from_slice(&[0x07]);
            } else {
                self.chunk.extend_from_slice(b"\x1b\\");
            }
        }
    }
}

#[cfg(windows)]
pub fn enable_vt_input() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        SetConsoleMode,
    };

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode: u32 = 0;

    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    mode |= ENABLE_VIRTUAL_TERMINAL_INPUT;

    if unsafe { SetConsoleMode(handle, mode) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

pub fn window_change_signal() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel::<()>(1);

    #[cfg(unix)]
    tokio::spawn(async move {
        let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            .expect("SIGWINCH registration");

        while sig.recv().await.is_some() {
            // Drop duplicate signals if receiver hasn't processed the previous one
            if matches!(tx.try_send(()), Err(mpsc::error::TrySendError::Closed(_))) {
                break;
            }
        }
    });

    #[cfg(windows)]
    tokio::spawn(async move {
        use crossterm::terminal::size;

        let mut last = size().expect("console size");
        let mut interval = tokio::time::interval(Duration::from_millis(100));

        loop {
            interval.tick().await;
            let now = size().expect("console size");

            if now != last {
                last = now;

                // Coalesce events: send if empty, drop if full, break if closed
                if matches!(tx.try_send(()), Err(mpsc::error::TrySendError::Closed(_))) {
                    break;
                }
            }
        }
    });

    rx
}
