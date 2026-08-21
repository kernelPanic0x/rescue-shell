use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::anyhow;
use bytes::{Bytes, BytesMut};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::DisableMouseCapture,
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use magic_wormhole::Code;
use tokio::{
    sync::{mpsc, watch},
    time::sleep,
};
use vte::{Params, Perform};

use crate::protocol::{HelperId, TIMEOUT, TerminalSize};

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
                let symbol = if is_utf8 { "●" } else { "[*]" };
                (
                    format!("{symbol} Online ({} ms)", ping.as_millis()),
                    Color::Green,
                )
            }
            InternetState::Offline => {
                let symbol = if is_utf8 { "×" } else { "[!]" };
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
            sleep(Duration::from_secs(3)).await;

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

pub fn is_detach_key(bytes: &[u8]) -> bool {
    // Ctrl+] (0x1D) detaches locally
    bytes.contains(&0x1d)
}

pub struct LocalConsole {
    stdin_rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    stdout_tx: Option<mpsc::Sender<Bytes>>,
    stdout_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,

    #[cfg(windows)]
    #[allow(unused)]
    win_vt_input: winvt::ConsoleHandle,

    parser: Arc<Mutex<vt100::Parser>>,
    prev_screen: Option<vt100::Screen>,
    prev_physical_size: Option<(u16, u16)>,
    statusbar_rx: watch::Receiver<StatusBarState>,
}

impl LocalConsole {
    pub fn new(
        current_parser: Arc<Mutex<vt100::Parser>>,
        statusbar_handle: &StatusBarHandle,
    ) -> anyhow::Result<Self> {
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen)?;

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
                            break;
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

            #[cfg(windows)]
            win_vt_input: winvt::ConsoleHandle::new()?.enable_virtual_terminal_input()?,

            parser: current_parser,
            prev_screen: None,
            prev_physical_size: None,
            statusbar_rx: statusbar_handle.subscribe(),
        })
    }

    pub async fn render(&mut self) -> anyhow::Result<()> {
        let (phys_cols, phys_rows) = crossterm::terminal::size()?;

        let buf = {
            let mut parser = self.parser.lock().unwrap();
            let screen = parser.screen_mut();
            let (screen_rows, screen_cols) = screen.size();

            // Trigger full redraw if either virtual VT size OR physical terminal dimensions change
            let size_changed = match (&self.prev_screen, self.prev_physical_size) {
                (Some(s), Some(prev_phys)) => {
                    screen.size() != s.size() || (phys_cols, phys_rows) != prev_phys
                }
                _ => true,
            };

            let scrolled = screen.scrollback() > 0;
            let statusbar = self.statusbar_rx.borrow();
            let mut buf = Vec::new();

            // 1. Render status bar across the FULL PHYSICAL width on Row 0
            statusbar.render_to(&mut buf, phys_cols);

            let is_utf8 = supports_unicode();
            let v_border = if is_utf8 { "│" } else { "|" };
            let h_border = if is_utf8 { '─' } else { '-' };
            let corner_border = if is_utf8 { "┘" } else { "+" };
            let border_fg = Color::DarkGrey;

            let has_right_border = phys_cols > screen_cols;
            let has_bottom_border = phys_rows > screen_rows + 1;

            // 2. Render screen contents (Full redraw vs. Diff)
            if size_changed || self.prev_screen.is_none() {
                buf.extend_from_slice(&screen.input_mode_formatted());

                for (r, row_bytes) in screen.rows_formatted(0, screen_cols).enumerate() {
                    let physical_row = (r as u16) + 1;
                    if physical_row >= phys_rows {
                        break;
                    }

                    let _ = queue!(
                        buf,
                        MoveTo(0, physical_row),
                        ResetColor,
                        SetAttribute(Attribute::Reset),
                        Clear(ClearType::UntilNewLine)
                    );

                    buf.extend_from_slice(&row_bytes);

                    // Draw vertical right border and clear margin with default background
                    if has_right_border {
                        let _ = queue!(
                            buf,
                            MoveTo(screen_cols, physical_row),
                            ResetColor,
                            SetForegroundColor(border_fg),
                            SetAttribute(Attribute::NormalIntensity),
                            Print(v_border),
                            ResetColor,
                            Clear(ClearType::UntilNewLine)
                        );
                    }
                }

                // Draw horizontal bottom border (tmux style)
                if has_bottom_border {
                    let border_row = screen_rows + 1;
                    let h_line: String =
                        std::iter::repeat_n(h_border, screen_cols as usize).collect();

                    let _ = queue!(
                        buf,
                        MoveTo(0, border_row),
                        ResetColor,
                        SetForegroundColor(border_fg),
                        SetAttribute(Attribute::NormalIntensity),
                        Print(h_line)
                    );

                    if has_right_border {
                        let _ = queue!(
                            buf,
                            Print(corner_border),
                            ResetColor,
                            Clear(ClearType::UntilNewLine)
                        );
                    } else {
                        let _ = queue!(buf, ResetColor, Clear(ClearType::UntilNewLine));
                    }

                    // Clear any remaining lines below the bottom border
                    for r in (border_row + 1)..phys_rows {
                        let _ = queue!(
                            buf,
                            MoveTo(0, r),
                            ResetColor,
                            SetAttribute(Attribute::Reset),
                            Clear(ClearType::UntilNewLine)
                        );
                    }
                }
            } else if let Some(s) = &self.prev_screen {
                let input_diff = screen.input_mode_diff(s);
                if !input_diff.is_empty() {
                    buf.extend_from_slice(&input_diff);
                    buf.extend_from_slice(b"\x1b[?1000h\x1b[?1006h");
                }

                for (r, diff_bytes) in screen.rows_diff(s, 0, screen_cols).enumerate() {
                    if !diff_bytes.is_empty() {
                        let physical_row = (r as u16) + 1;
                        if physical_row >= phys_rows {
                            break;
                        }

                        let _ = queue!(buf, MoveTo(0, physical_row));
                        buf.extend_from_slice(&diff_bytes);

                        // Reset active colors, repaint the right border, and clear the margin
                        if has_right_border {
                            let _ = queue!(
                                buf,
                                MoveTo(screen_cols, physical_row),
                                ResetColor,
                                SetForegroundColor(border_fg),
                                SetAttribute(Attribute::NormalIntensity),
                                Print(v_border),
                                ResetColor,
                                Clear(ClearType::UntilNewLine)
                            );
                        } else {
                            let _ = queue!(buf, ResetColor, SetAttribute(Attribute::Reset));
                        }
                    }
                }
            }

            // Move physical cursor to virtual shell cursor position
            if !scrolled {
                let (cur_r, cur_c) = screen.cursor_position();
                let target_c = cur_c.min(screen_cols.saturating_sub(1));
                let target_r = (cur_r + 1).min(phys_rows.saturating_sub(1));
                let _ = queue!(
                    buf,
                    ResetColor,
                    SetAttribute(Attribute::Reset),
                    MoveTo(target_c, target_r)
                );
            }

            if scrolled || screen.hide_cursor() {
                let _ = queue!(buf, Hide);
            } else {
                let _ = queue!(buf, Show);
            }

            self.prev_screen = Some(screen.clone());
            self.prev_physical_size = Some((phys_cols, phys_rows));

            Bytes::from(buf)
        };

        self.write_stdout(buf).await
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

    /// Adjust the vt100 viewport and return the new scrollback offset.
    /// Positive delta scrolls back (older), negative scrolls forward (toward live).
    pub fn apply_scroll(&self, delta: i32) -> usize {
        let mut parser = self.parser.lock().unwrap();
        let current = parser.screen().scrollback() as i64;
        let next = (current + delta as i64).clamp(0, i64::MAX) as usize;
        // set_scrollback clamps the upper bound to the actual history length,
        // so we never need to know the max ourselves.
        parser.screen_mut().set_scrollback(next);
        parser.screen().scrollback()
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
#[cfg(unix)]
const DA2_RESP: &[u8] = b"\x1b[>1;277;0c";

/// Device Status Report reply. termwiz only knows the query `5n`; the answer
/// "ready, no malfunction" must be emitted directly.
#[cfg(unix)]
const DSR_OK_RESP: &[u8] = b"\x1b[0n";

/// XTVERSION reply, a DCS sequence `ESC P >| <prog> <version> ESC \`.
/// termwiz models `>q` as the request only; the response is the DCS body.
#[cfg(unix)]
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
mod winvt {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle,
        STD_INPUT_HANDLE, SetConsoleMode,
    };

    pub struct ConsoleHandle {
        handle: HANDLE,
        original_mode: CONSOLE_MODE,
    }

    impl ConsoleHandle {
        pub fn new() -> std::io::Result<Self> {
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            let mut original_mode: CONSOLE_MODE = 0;

            if unsafe { GetConsoleMode(handle, &mut original_mode) } == 0 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(Self {
                handle,
                original_mode,
            })
        }

        pub fn enable_virtual_terminal_input(self) -> std::io::Result<Self> {
            let mut mode: CONSOLE_MODE = 0;

            if unsafe { GetConsoleMode(self.handle, &mut mode) } == 0 {
                return Err(std::io::Error::last_os_error());
            }

            mode |= ENABLE_VIRTUAL_TERMINAL_INPUT;

            if unsafe { SetConsoleMode(self.handle, mode) } == 0 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(self)
        }
    }

    impl Drop for ConsoleHandle {
        fn drop(&mut self) {
            unsafe { SetConsoleMode(self.handle, self.original_mode) };
        }
    }
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

/// Adjusts SGR mouse coordinates by subtracting `row_offset` (e.g., 1 for the status bar).
///
/// Returns:
/// - `Some(adjusted_bytes)`: The modified SGR sequence to send to the PTY.
/// - `None`: If the click landed on the status bar (should be dropped).
pub fn translate_sgr_mouse(bytes: &[u8], row_offset: u16) -> Option<Bytes> {
    // SGR mouse format: \x1b[<BUTTON;COL;ROW(M|m)
    if !bytes.starts_with(b"\x1b[<") {
        return Some(Bytes::copy_from_slice(bytes));
    }

    let s = std::str::from_utf8(bytes).ok()?;
    let last_char = s.chars().last()?;
    if last_char != 'M' && last_char != 'm' {
        return Some(Bytes::copy_from_slice(bytes));
    }

    let inner = &s[3..s.len() - 1]; // Strip "\x1b[<" and trailing "M"/"m"
    let mut parts = inner.split(';');

    let btn = parts.next()?;
    let col = parts.next()?;
    let row: u16 = parts.next()?.parse().ok()?;

    if parts.next().is_some() {
        return Some(Bytes::copy_from_slice(bytes));
    }

    // If the click is on the status bar (Row <= row_offset), drop it
    if row <= row_offset {
        return None;
    }

    let adjusted_row = row - row_offset;
    Some(Bytes::from(format!(
        "\x1b[<{btn};{col};{adjusted_row}{last_char}"
    )))
}

struct HelperLifetime {
    size: TerminalSize,
    deadline: Instant,
}

pub struct TerminalSizeNegotiator {
    local_size: TerminalSize,
    helpers: HashMap<HelperId, HelperLifetime>,
}

impl TerminalSizeNegotiator {
    pub fn new(local_size: TerminalSize) -> Self {
        Self {
            local_size,
            helpers: HashMap::new(),
        }
    }

    /// Update local window size
    pub fn update_local(&mut self, size: TerminalSize) -> TerminalSize {
        self.local_size = size;
        self.best_size()
    }

    /// Update or insert a remote helper's size hint
    pub fn update_helper(&mut self, id: HelperId, size: TerminalSize) -> TerminalSize {
        self.helpers.insert(
            id,
            HelperLifetime {
                size,
                deadline: Instant::now() + TIMEOUT,
            },
        );
        self.best_size()
    }

    /// Remove a helper that disconnected
    pub fn remove_helper(&mut self, id: HelperId) -> TerminalSize {
        self.helpers.remove(&id);
        self.best_size()
    }

    /// Calculate the minimum size across local terminal and all active helpers
    pub fn best_size(&mut self) -> TerminalSize {
        let now = Instant::now();
        self.helpers.retain(|_, h| now < h.deadline);

        self.helpers
            .values()
            .map(|h| h.size)
            .fold(self.local_size, |acc, s| acc.min_dimensions(s))
    }
}
