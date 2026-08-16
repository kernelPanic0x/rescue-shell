use std::{
    borrow::Cow,
    fmt,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, anyhow};
use bytes::{Bytes, BytesMut};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::DisableMouseCapture,
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        Clear, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};
use magic_wormhole::Code;
use tokio::sync::{mpsc, watch};
use vte::{Params, Perform};

pub const SCROLLBACK_LINES: usize = 1000;

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

/// Check if the terminal environment supports UTF-8 glyphs
fn supports_unicode() -> bool {
    // Windows Terminal / modern shells or UTF-8 locale on Linux/macOS
    if let Ok(lang) = std::env::var("LANG")
        && (lang.contains("UTF-8") || lang.contains("utf8") || lang.contains("UTF8"))
    {
        return true;
    }
    // Check for modern terminal emulators
    std::env::var("TERM_PROGRAM").is_ok() || std::env::var("WT_SESSION").is_ok()
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
        let is_utf8 = supports_unicode();
        let width = cols as usize;

        let bar_bg = Color::AnsiValue(17);
        let default_fg = Color::White;
        let separator_fg = Color::White;
        let code_fg = Color::White;

        let (net_str, net_fg) = match self.internet_state {
            InternetState::Online(ping) => {
                let symbol = if is_utf8 { "●" } else { "[+]" };
                (
                    format!("{symbol} Online ({} ms)", ping.as_millis()),
                    Color::Green,
                )
            }
            InternetState::Offline => {
                let symbol = if is_utf8 { "×" } else { "[-]" };
                (format!("{symbol} Offline"), Color::Red)
            }
        };

        let (helper_str, helper_fg) = if self.connected_helpers > 0 {
            (
                format!("{} Connected", self.connected_helpers).into(),
                Color::Green,
            )
        } else {
            (Cow::Borrowed("0 Connected"), Color::DarkGrey)
        };

        let title = format!("rescue-shell {}", env!("CARGO_PKG_VERSION"));
        let role = self.role.to_string();

        // Segments: (text, fg_color, is_bold)
        let mut segments: Vec<(Cow<'_, str>, Color, Attribute)> = vec![
            match &self.code {
                Some(code) => (code.to_string().into(), code_fg, Attribute::Bold),
                None => ("No code".into(), code_fg, Attribute::NormalIntensity),
            },
            (" ║ ".into(), separator_fg, Attribute::NormalIntensity),
            (title.into(), default_fg, Attribute::NormalIntensity),
            (" ║ ".into(), separator_fg, Attribute::NormalIntensity),
            (role.into(), default_fg, Attribute::NormalIntensity),
            (" ║ ".into(), separator_fg, Attribute::NormalIntensity),
            (helper_str, helper_fg, Attribute::NormalIntensity),
            (" ║ ".into(), separator_fg, Attribute::NormalIntensity),
            (net_str.into(), net_fg, Attribute::NormalIntensity),
            (
                " ║ CTRL+] to exit".into(),
                default_fg,
                Attribute::NormalIntensity,
            ),
        ];

        let content_len: usize = segments.iter().map(|(s, _, _)| s.chars().count()).sum();

        let mut styled_chars: Vec<(char, Color, Attribute)> = Vec::new();

        if content_len <= width {
            for (text, fg, bold) in segments {
                styled_chars.extend(text.chars().map(|c| (c, fg, bold)));
            }
            styled_chars.extend(std::iter::repeat_n(
                (' ', default_fg, Attribute::NormalIntensity),
                width - content_len,
            ));
        } else {
            segments.push(("   ***   ".into(), separator_fg, Attribute::NormalIntensity));
            for (text, fg, bold) in segments {
                styled_chars.extend(text.chars().map(|c| (c, fg, bold)));
            }
        }

        let offset = if content_len <= width {
            0
        } else {
            self.tick % styled_chars.len()
        };

        let visible_window: Vec<(char, Color, Attribute)> = styled_chars
            .into_iter()
            .cycle()
            .skip(offset)
            .take(width)
            .collect();

        let _ = queue!(buf, MoveTo(0, 0), SetBackgroundColor(bar_bg));

        for chunk in visible_window.chunk_by(|a, b| a.1 == b.1 && a.2 == b.2) {
            let fg = chunk[0].1;
            let attr = chunk[0].2;
            let text: String = chunk.iter().map(|(c, _, _)| c).collect();

            let _ = queue!(buf, SetForegroundColor(fg), SetAttribute(attr), Print(text));
        }

        let _ = queue!(buf, ResetColor, SetAttribute(Attribute::NormalIntensity));
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

    // Override the remote app's mouse state on OUR physical terminal.
    // Force SGR mouse reporting so the wheel arrives as ESC[<64/65;…M
    // instead of being collapsed into Up/Down arrows by the terminal's
    // alternate-screen scroll fallback. Must come AFTER input_mode_formatted().
    buf.extend_from_slice(b"\x1b[?1000h\x1b[?1006h");

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

    // 4. Move terminal cursor to virtual cursor position (live view only)
    let scrolled = screen.scrollback() > 0;
    if !scrolled {
        let (cur_r, cur_c) = screen.cursor_position();
        let _ = queue!(
            buf,
            SetAttribute(crossterm::style::Attribute::Reset),
            MoveTo(cur_c, cur_r + 1)
        );
    }

    // 5. Cursor visibility: hide while scrolled back
    if scrolled || screen.hide_cursor() {
        let _ = queue!(buf, Hide);
    } else {
        let _ = queue!(buf, Show);
    }

    Bytes::from(buf)
}

pub fn is_detach_key(bytes: &[u8]) -> bool {
    // Ctrl+] (0x1D) detaches locally
    bytes.contains(&0x1d)
}

pub struct LocalConsole {
    stdin_rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    stdout_tx: Option<mpsc::Sender<Bytes>>,
    stdout_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LocalConsole {
    pub fn new() -> anyhow::Result<Self> {
        LocalConsole::setup().context("Console setup")?;

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

        Ok(Self {
            stdin_rx: tokio::sync::Mutex::new(stdin_rx),
            stdout_tx: Some(stdout_tx),
            stdout_handle: tokio::sync::Mutex::new(Some(stdout_handle)),
        })
    }

    fn setup() -> anyhow::Result<()> {
        enable_raw_mode()?;

        #[cfg(windows)]
        crate::console::enable_vt_input()?;

        let mut stdout = std::io::stdout();

        execute!(stdout, EnterAlternateScreen)?;

        Ok(())
    }

    pub async fn read_stdin(&self) -> Option<Bytes> {
        self.stdin_rx.lock().await.recv().await
    }

    pub async fn write_stdout(&self, bytes: Bytes) -> anyhow::Result<()> {
        self.stdout_tx
            .as_ref()
            .ok_or_else(|| anyhow!("Console stdout closed"))?
            .send(bytes)
            .await
            .map_err(|e| anyhow!(e))
    }

    pub async fn flush_and_close(mut self) {
        self.stdout_tx.take();
        let handle = self.stdout_handle.lock().await.take();
        if let Some(h) = handle {
            let _ = h.await; // drain remaining frames to the local terminal
        }
    }
}

impl Drop for LocalConsole {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();

        let _ = execute!(
            stdout,
            SetAttribute(Attribute::Reset),
            LeaveAlternateScreen,
            DisableMouseCapture,
            Show
        );

        let _ = disable_raw_mode();

        println!("[session ended]");
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

/// Adjust the vt100 viewport and return the new scrollback offset.
/// Positive delta scrolls back (older), negative scrolls forward (toward live).
pub fn apply_scroll(parser: &Arc<Mutex<vt100::Parser>>, delta: i32) -> usize {
    let mut parser = parser.lock().unwrap();
    let current = parser.screen().scrollback() as i64;
    let next = (current + delta as i64).clamp(0, i64::MAX) as usize;
    // set_scrollback clamps the upper bound to the actual history length,
    // so we never need to know the max ourselves.
    parser.screen_mut().set_scrollback(next);
    parser.screen().scrollback()
}

/// Returns Some(delta) when `bytes` is a local scroll gesture and must NOT be
/// forwarded to the remote shell. Positive = scroll back (older).
pub fn scroll_delta(bytes: &[u8], page_size: i32) -> Option<i32> {
    match bytes {
        b"\x1b[5~" => Some(page_size),    // PageUp
        b"\x1b[6~" => Some(-page_size),   // PageDown
        b"\x1b[5;2~" => Some(page_size),  // Shift+PageUp
        b"\x1b[6;2~" => Some(-page_size), // Shift+PageDown
        b"\x1b[1;5A" => Some(1),          // Ctrl+Up
        b"\x1b[1;5B" => Some(-1),         // Ctrl+Down
        _ => sgr_wheel_delta(bytes, 3),   // mouse wheel, if enabled (see below)
    }
}

/// SGR mouse wheel: ESC [ < 64/65 ; row ; col M/m
fn sgr_wheel_delta(bytes: &[u8], lines: i32) -> Option<i32> {
    let body = bytes.strip_prefix(b"\x1b[<")?;

    // Find the index of the first semicolon to replace `split_once`
    let idx = body.iter().position(|&b| b == b';')?;
    let button = &body[..idx];
    let rest = &body[idx + 1..];

    if !rest.contains(&b';') || *rest.last()? != b'M' {
        return None; // press only; ignore the matching release `m`
    }

    let button: i32 = std::str::from_utf8(button).ok()?.parse().ok()?;
    match button {
        64 => Some(lines),  // wheel up
        65 => Some(-lines), // wheel down
        _ => None,
    }
}

pub fn is_sgr_mouse(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x1b[<") && matches!(bytes.last(), Some(b'M') | Some(b'm'))
}
