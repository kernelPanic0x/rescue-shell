use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    io::{Read, Write},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::{BufMut, Bytes, BytesMut};
use color_eyre::eyre::eyre;
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
use parking_lot::Mutex;
use tokio::{
    sync::{mpsc, watch},
    time::sleep,
};
use vte::{Params, Perform};

use crate::{
    common::QUEUE_SIZE,
    protocol::{HelperId, PtySize, TIMEOUT},
};

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

#[derive(Clone, Debug, strum::IntoStaticStr)]
pub enum Role {
    Victim,
    Helper,
}

/// Check if the terminal environment supports UTF-8 glyphs
fn supports_unicode() -> bool {
    // 1. If it's a Linux virtual console (TTY), assume no rich Unicode/border support
    if std::env::var("TERM").is_ok_and(|term| term.to_lowercase().starts_with("linux")) {
        return false;
    }

    // 2. Modern terminal emulators / multiplexers (almost universally support UTF-8)
    if std::env::var("WT_SESSION").is_ok()          // Windows Terminal
        || std::env::var("TERM_PROGRAM").is_ok()    // iTerm2, Apple Terminal, VS Code, WezTerm, Ghostty, Alacritty
        || std::env::var("KONSOLE_VERSION").is_ok() // KDE Konsole
        || std::env::var("TMUX").is_ok()
    {
        return true;
    }

    // 3. Check POSIX locale hierarchy (LC_ALL > LC_CTYPE > LANG)
    let locale = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_CTYPE"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default()
        .to_lowercase();

    if locale.contains("utf-8") || locale.contains("utf8") {
        return true;
    }

    // 4. Fallback on modern Windows 10+ (cmd.exe / powershell usually support UTF-8)
    #[cfg(windows)]
    {
        true
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    TrueColor, // 24-bit (16 million colors)
    Ansi256,   // 8-bit (256 colors)
    Basic,     // 8/16 standard ANSI colors (Linux console, basic TTYs)
}

pub fn detect_color_support() -> ColorSupport {
    if let Ok(ct) = std::env::var("COLORTERM")
        && (ct == "truecolor" || ct == "24bit")
    {
        return ColorSupport::TrueColor;
    }

    if let Ok(term) = std::env::var("TERM") {
        let term_lower = term.to_lowercase();
        if term_lower.contains("256color") || term_lower.contains("256") {
            return ColorSupport::Ansi256;
        }
        if term_lower == "linux" || term_lower == "dumb" || term_lower.starts_with("vt") {
            return ColorSupport::Basic;
        }
    }

    // Default fallback: most modern terminal emulators support at least 256 colors
    ColorSupport::Ansi256
}

#[derive(Clone, Debug)]
pub struct StatusBarState {
    pub code: WormholeCodeState,
    pub role: Role,
    pub connected_helpers: u8,
    pub internet_state: InternetState,
    pub tick: usize,
    is_utf8: bool,
    color_mode: ColorSupport,
    title: &'static str,
}

impl StatusBarState {
    fn new(role: Role) -> Self {
        Self {
            code: WormholeCodeState::NoCode,
            role,
            connected_helpers: 0,
            internet_state: InternetState::Offline,
            tick: 0,
            is_utf8: supports_unicode(),
            color_mode: detect_color_support(),
            title: concat!(env!("CARGO_BIN_NAME"), " ", env!("CARGO_PKG_VERSION")),
        }
    }

    pub fn render_to(&self, buf: &mut BytesMut, cols: u16) {
        let width = cols as usize;

        // --- PALETTE FALLBACKS ---
        let (bar_bg, default_fg, separator_fg, code_fg, connection_fg) = match self.color_mode {
            ColorSupport::TrueColor | ColorSupport::Ansi256 => (
                Color::AnsiValue(17), // Deep Navy Blue
                Color::White,
                Color::White,
                Color::White,
                Color::DarkGrey, // Readable on navy blue in 256-color
            ),
            ColorSupport::Basic => (
                Color::DarkBlue, // Standard ANSI Blue (\x1b[44m) - supported by Linux console!
                Color::White,
                Color::White,
                Color::White,
                Color::Grey, // Standard ANSI Light Gray (\x1b[37m) - visible on blue/black
            ),
        };

        let (net_str, net_fg) = match self.internet_state {
            InternetState::Online(ping) => {
                let symbol = if self.is_utf8 { "●" } else { "*" };
                (
                    Cow::Owned(format!("{symbol} Online ({} ms)", ping.as_millis())),
                    Color::Green,
                )
            }
            InternetState::Offline => (Cow::Borrowed("x Offline"), Color::Red),
        };

        let (helper_str, helper_fg) = if self.connected_helpers > 0 {
            (
                format!("{} Connected", self.connected_helpers).into(),
                Color::Green,
            )
        } else {
            (Cow::Borrowed("0 Connected"), connection_fg)
        };

        let role = Cow::from(<&'static str>::from(&self.role));
        let title = Cow::Borrowed(self.title);

        let mut segments: Vec<(Cow<'_, str>, Color, Attribute)> = vec![
            match &self.code {
                WormholeCodeState::Code(code) => {
                    (code.to_string().into(), code_fg, Attribute::Bold)
                }
                WormholeCodeState::NoCode => {
                    ("No code".into(), code_fg, Attribute::NormalIntensity)
                }
                WormholeCodeState::Generating => {
                    ("Generating...".into(), code_fg, Attribute::NormalIntensity)
                }
            },
            (" ║ ".into(), separator_fg, Attribute::NormalIntensity),
            (title, default_fg, Attribute::NormalIntensity),
            (" ║ ".into(), separator_fg, Attribute::NormalIntensity),
            (role, default_fg, Attribute::NormalIntensity),
            (" ║ ".into(), separator_fg, Attribute::NormalIntensity),
            (helper_str, helper_fg, Attribute::NormalIntensity),
            (" ║ ".into(), separator_fg, Attribute::NormalIntensity),
            (net_str, net_fg, Attribute::NormalIntensity),
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

        let _ = queue!(buf.writer(), MoveTo(0, 0), SetBackgroundColor(bar_bg));

        for chunk in visible_window.chunk_by(|a, b| a.1 == b.1 && a.2 == b.2) {
            let Some(&(_, fg, attr)) = chunk.first() else {
                continue;
            };

            let _ = queue!(buf.writer(), SetForegroundColor(fg), SetAttribute(attr),);

            let mut writer = buf.writer();
            for &(c, _, _) in chunk {
                let _ = write!(writer, "{c}");
            }
        }

        let _ = queue!(buf.writer(), ResetColor, SetAttribute(Attribute::Reset));
    }
}

#[derive(Debug, Clone)]
pub enum WormholeCodeState {
    Generating,
    NoCode,
    Code(Code),
}

impl std::fmt::Display for WormholeCodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            WormholeCodeState::Generating => Cow::Borrowed("Generating..."),
            WormholeCodeState::NoCode => Cow::Borrowed("No code"),
            WormholeCodeState::Code(code) => Cow::Owned(code.to_string()),
        };
        write!(f, "{s}")
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

    pub fn set_code_state(&self, code: WormholeCodeState) {
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

struct LocalConsoleInner {
    #[cfg(windows)]
    #[allow(unused)]
    win_vt_input: winvt::ConsoleHandle,

    parser: vt100::Parser,
    prev_screen: Vec<BytesMut>,
    buf: BytesMut,
    prev_physical_size: Option<(u16, u16)>,
    prev_virtual_size: Option<(u16, u16)>,
}

impl Drop for LocalConsoleInner {
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

#[derive(Clone)]
pub struct LocalConsole {
    inner: Arc<Mutex<LocalConsoleInner>>,
    stdout_tx: Arc<Mutex<Option<mpsc::Sender<Bytes>>>>,
    stdin_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Bytes>>>,
    stdout_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    statusbar_rx: watch::Receiver<StatusBarState>,
    is_utf8: bool,
}

impl LocalConsole {
    pub fn new(statusbar_handle: &StatusBarHandle) -> color_eyre::Result<Self> {
        let (cols, rows) = crossterm::terminal::size()?;
        let pty_size: PtySize = PtySize {
            cols,
            rows: rows.saturating_sub(1).max(1),
        };
        let parser = vt100::Parser::new(pty_size.rows, pty_size.cols, SCROLLBACK_LINES);

        let (stdin_tx, stdin_rx) = mpsc::channel::<Bytes>(QUEUE_SIZE);
        let (stdout_tx, mut stdout_rx) = mpsc::channel::<Bytes>(QUEUE_SIZE);

        let inner = Arc::new(Mutex::new(LocalConsoleInner {
            parser,
            prev_screen: Vec::new(),
            buf: BytesMut::new(),
            prev_physical_size: None,
            prev_virtual_size: None,
            #[cfg(windows)]
            win_vt_input: winvt::ConsoleHandle::new()?.enable_virtual_terminal_input()?,
        }));

        // SECURITY: Only enter into raw mode after inner allocation so that on failure the drop fn restores it again.
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen)?;

        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match std::io::stdin().read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        #[expect(clippy::indexing_slicing)]
                        let slice = &buf[..n];
                        if stdin_tx
                            .blocking_send(Bytes::copy_from_slice(slice))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        let stdout_handle = tokio::task::spawn_blocking(move || {
            let mut stdout = std::io::stdout();
            while let Some(b) = stdout_rx.blocking_recv() {
                if stdout.write_all(&b).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            stdout_tx: Arc::new(Mutex::new(Some(stdout_tx))),
            stdin_rx: Arc::new(tokio::sync::Mutex::new(stdin_rx)),
            stdout_handle: Arc::new(Mutex::new(Some(stdout_handle))),
            statusbar_rx: statusbar_handle.subscribe(),
            is_utf8: supports_unicode(),
            inner,
        })
    }

    pub async fn render(&self) -> color_eyre::Result<()> {
        let (phys_cols, phys_rows) = crossterm::terminal::size()?;

        let buf = {
            let mut inner = self.inner.lock();
            let LocalConsoleInner {
                parser,
                prev_screen,
                buf,
                prev_physical_size,
                prev_virtual_size,
                ..
            } = &mut *inner;
            let screen = parser.screen();
            let (screen_rows, screen_cols) = screen.size();

            // Trigger full redraw if either virtual VT size OR physical terminal dimensions change
            let size_changed = match (prev_virtual_size, prev_physical_size) {
                (Some(prev_virt), Some(prev_phys)) => {
                    (screen_rows, screen_cols) != *prev_virt || (phys_cols, phys_rows) != *prev_phys
                }
                _ => true,
            };

            let scrolled = screen.scrollback() > 0;
            let statusbar = self.statusbar_rx.borrow();

            // 1. Render status bar across the FULL PHYSICAL width on Row 0
            statusbar.render_to(buf, phys_cols);

            let v_border = if self.is_utf8 { "│" } else { "|" };
            let h_border = if self.is_utf8 { '─' } else { '-' };
            let corner_border = if self.is_utf8 { "┘" } else { "+" };
            let border_fg = Color::DarkGrey;

            let has_right_border = phys_cols > screen_cols;
            let has_bottom_border = phys_rows > screen_rows + 1;

            // 2. Render screen contents (Full redraw vs. Diff)
            if size_changed || prev_screen.is_empty() {
                buf.extend_from_slice(&screen.input_mode_formatted());
                buf.extend_from_slice(b"\x1b[?1000h\x1b[?1006h");

                let mut new_prev_screen = Vec::with_capacity(screen_rows as usize);

                for (physical_row, (_, row_bytes)) in
                    (1..phys_rows).zip(screen.rows_formatted(0, screen_cols).enumerate())
                {
                    let _ = queue!(
                        buf.writer(),
                        MoveTo(0, physical_row),
                        ResetColor,
                        SetAttribute(Attribute::Reset),
                        Clear(ClearType::UntilNewLine)
                    );

                    buf.extend_from_slice(&row_bytes);

                    // Draw vertical right border and clear margin with default background
                    if has_right_border {
                        let _ = queue!(
                            buf.writer(),
                            MoveTo(screen_cols, physical_row),
                            ResetColor,
                            SetForegroundColor(border_fg),
                            SetAttribute(Attribute::NormalIntensity),
                            Print(v_border),
                            ResetColor,
                            Clear(ClearType::UntilNewLine)
                        );
                    }

                    let mut row_buf = BytesMut::with_capacity(row_bytes.len());
                    row_buf.extend_from_slice(&row_bytes);
                    new_prev_screen.push(row_buf);
                }

                // Draw horizontal bottom border (tmux style)
                if has_bottom_border {
                    let border_row = screen_rows + 1;
                    let h_line: String =
                        std::iter::repeat_n(h_border, screen_cols as usize).collect();

                    let _ = queue!(
                        buf.writer(),
                        MoveTo(0, border_row),
                        ResetColor,
                        SetForegroundColor(border_fg),
                        SetAttribute(Attribute::NormalIntensity),
                        Print(h_line)
                    );

                    if has_right_border {
                        let _ = queue!(
                            buf.writer(),
                            Print(corner_border),
                            ResetColor,
                            Clear(ClearType::UntilNewLine)
                        );
                    } else {
                        let _ = queue!(buf.writer(), ResetColor, Clear(ClearType::UntilNewLine));
                    }

                    // Clear any remaining lines below the bottom border
                    for r in (border_row + 1)..phys_rows {
                        let _ = queue!(
                            buf.writer(),
                            MoveTo(0, r),
                            ResetColor,
                            SetAttribute(Attribute::Reset),
                            Clear(ClearType::UntilNewLine)
                        );
                    }
                }

                let _ = std::mem::replace(prev_screen, new_prev_screen);
            } else {
                // Ensure prev_screen has enough slot capacity if row count grew
                if prev_screen.len() < screen_rows as usize {
                    prev_screen.resize_with(screen_rows as usize, BytesMut::new);
                }

                for (physical_row, (r, row_bytes)) in
                    (1u16..).zip(screen.rows_formatted(0, screen_cols).enumerate())
                {
                    // Compare against cached previous row
                    if let Some(prev_row) = prev_screen.get(r)
                        && prev_row.as_ref() == row_bytes.as_slice()
                    {
                        continue; // Unchanged row, skip rendering!
                    }

                    let _ = queue!(
                        buf.writer(),
                        MoveTo(0, physical_row),
                        ResetColor,
                        SetAttribute(Attribute::Reset),
                        Clear(ClearType::UntilNewLine)
                    );

                    buf.extend_from_slice(&row_bytes);

                    if has_right_border {
                        let _ = queue!(
                            buf.writer(),
                            MoveTo(screen_cols, physical_row),
                            ResetColor,
                            SetForegroundColor(border_fg),
                            SetAttribute(Attribute::NormalIntensity),
                            Print(v_border),
                            ResetColor,
                            Clear(ClearType::UntilNewLine)
                        );
                    }

                    // Update cached row in-place (re-using existing BytesMut capacity)
                    if let Some(prev_row) = prev_screen.get_mut(r) {
                        prev_row.clear();
                        prev_row.extend_from_slice(&row_bytes);
                    }
                }
            }

            // Move physical cursor to virtual shell cursor position
            if !scrolled {
                let (cur_r, cur_c) = screen.cursor_position();
                let target_c = cur_c.min(screen_cols.saturating_sub(1));
                let target_r = (cur_r + 1).min(phys_rows.saturating_sub(1));
                let _ = queue!(
                    buf.writer(),
                    ResetColor,
                    SetAttribute(Attribute::Reset),
                    MoveTo(target_c, target_r)
                );
            }

            if scrolled || screen.hide_cursor() {
                let _ = queue!(buf.writer(), Hide);
            } else {
                let _ = queue!(buf.writer(), Show);
            }

            inner.prev_virtual_size = Some((screen_rows, screen_cols));
            inner.prev_physical_size = Some((phys_cols, phys_rows));

            inner.buf.split().freeze()
        };

        self.write_stdout(buf).await
    }

    pub async fn read_stdin(&self) -> Option<Bytes> {
        self.stdin_rx.lock().await.recv().await
    }

    pub async fn write_stdout(&self, bytes: Bytes) -> color_eyre::Result<()> {
        let tx = { self.stdout_tx.lock().clone() }.ok_or_else(|| eyre!("Console stdout closed"))?;
        tx.send(bytes).await.map_err(|e| eyre!(e))
    }

    pub async fn flush_and_close(&self) {
        self.stdout_tx.lock().take();
        let handle = self.stdout_handle.lock().take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    /// Adjust the vt100 viewport and return the new scrollback offset.
    /// Positive delta scrolls back (older), negative scrolls forward (toward live).
    pub fn apply_scroll(&self, delta: isize) -> usize {
        let mut inner = self.inner.lock();
        let screen = inner.parser.screen_mut();

        let current = screen.scrollback();
        let next = current.saturating_add_signed(delta);

        screen.set_scrollback(next);
        screen.scrollback()
    }

    pub fn get_initial_screen_state(&self) -> Bytes {
        let mut inner = self.inner.lock();
        let LocalConsoleInner { parser, .. } = &mut *inner;
        let mut initial_state = Vec::new();

        let is_alternate_screen = parser.screen().alternate_screen();
        if is_alternate_screen {
            let _ = queue!(initial_state, EnterAlternateScreen);
        } else {
            let _ = queue!(initial_state, LeaveAlternateScreen);
        }

        initial_state.extend_from_slice(&parser.screen().state_formatted());

        Bytes::from(initial_state)
    }

    /// Returns the maximum available PTY size for the current OS terminal window (rows - 1 for statusbar)
    pub fn get_pty_size() -> PtySize {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        PtySize {
            cols,
            rows: rows.saturating_sub(1).max(1),
        }
    }

    pub fn resize_parser(&self, size: PtySize) {
        let mut inner = self.inner.lock();
        let LocalConsoleInner { parser, .. } = &mut *inner;
        parser.screen_mut().set_size(size.rows, size.cols);
    }

    pub fn access_parser_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut vt100::Parser) -> R,
    {
        let mut inner = self.inner.lock();
        let LocalConsoleInner { parser, .. } = &mut *inner;
        f(parser)
    }
}

/// Secondary Device Attributes reply. DCS/DA responses like this one have no
/// typed variant in termwiz (which models only the *request*), so the shell
/// wire format is a named constant. `WezTerm` answers identically: Pp=1 (VT220),
/// Pv=277 (xterm patch level -> fish enables ttymouse=sgr / 24-bit color).
#[cfg(unix)]
const DA2_RESP: &[u8] = b"\x1b[>1;277;0c";

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

pub struct PtyResponder {
    parser: vte::Parser,
    buffer: BytesMut,
    console: LocalConsole,
}

impl PtyResponder {
    pub fn new(console: LocalConsole) -> Self {
        Self {
            parser: vte::Parser::new(),
            buffer: BytesMut::new(),
            console,
        }
    }

    pub fn process(&mut self, chunk: &[u8]) -> Option<Bytes> {
        let mut handler = PtyResponderHandler {
            console: &self.console,
            buffer: &mut self.buffer,
        };
        self.parser.advance(&mut handler, chunk);
        (!self.buffer.is_empty()).then_some(self.buffer.split().freeze())
    }
}

struct PtyResponderHandler<'a> {
    console: &'a LocalConsole,
    buffer: &'a mut BytesMut,
}

impl Perform for PtyResponderHandler<'_> {
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
                self.buffer.extend_from_slice(DA1_RESP);
            }

            ('n', []) if first_param == 6 => {
                let (row, col) = self
                    .console
                    .access_parser_mut(|p| p.screen().cursor_position());
                let cpr = format!("\x1b[{};{}R", row + 1, col + 1);
                self.buffer.extend_from_slice(cpr.as_bytes());
            }

            // Unix only: the shell queries us directly. On Windows these never reach
            // us (ConPTY answers them), and answering unsolicited leaks an ESC.
            #[cfg(unix)]
            ('c', b">") if first_param == 0 => {
                self.buffer.extend_from_slice(DA2_RESP);
            }
            #[cfg(unix)]
            ('q', b">") => {
                self.buffer.extend_from_slice(&xtversion(
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION"),
                ));
            }

            _ => {}
        }
    }
}

#[derive(Default)]
pub struct Osc52Extractor {
    parser: vte::Parser,
    buffer: Vec<u8>,
}

impl Osc52Extractor {
    /// Strips everything EXCEPT complete OSC 52 escape sequences from the byte stream.
    pub fn extract(&mut self, chunk: &[u8]) -> Option<Bytes> {
        let mut handler = Osc52Handler {
            buffer: &mut self.buffer,
        };
        self.parser.advance(&mut handler, chunk);
        (!self.buffer.is_empty()).then_some(Bytes::from(std::mem::take(&mut self.buffer)))
    }
}

/// Private helper that collects filtered OSC 52 sequences dispatched by `vte`
struct Osc52Handler<'a> {
    buffer: &'a mut Vec<u8>,
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

            self.buffer.extend_from_slice(b"\x1b]52;");
            self.buffer.extend_from_slice(target);

            // Re-attach payload (params[2] and beyond)
            let payload = params.get(2..).unwrap_or_default();
            if payload.is_empty() {
                self.buffer.extend_from_slice(b";");
            } else {
                for p in payload {
                    self.buffer.extend_from_slice(b";");
                    self.buffer.extend_from_slice(p);
                }
            }

            // Re-attach original terminator
            if bell_terminated {
                self.buffer.extend_from_slice(&[0x07]);
            } else {
                self.buffer.extend_from_slice(b"\x1b\\");
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

    unsafe impl Send for ConsoleHandle {}
    unsafe impl Sync for ConsoleHandle {}

    impl ConsoleHandle {
        pub fn new() -> std::io::Result<Self> {
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            let mut original_mode: CONSOLE_MODE = 0;

            if unsafe { GetConsoleMode(handle, &raw mut original_mode) } == 0 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(Self {
                handle,
                original_mode,
            })
        }

        pub fn enable_virtual_terminal_input(self) -> std::io::Result<Self> {
            let mut mode: CONSOLE_MODE = 0;

            if unsafe { GetConsoleMode(self.handle, &raw mut mode) } == 0 {
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
            if matches!(tx.try_send(()), Err(mpsc::error::TrySendError::Closed(()))) {
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
                if matches!(tx.try_send(()), Err(mpsc::error::TrySendError::Closed(()))) {
                    break;
                }
            }
        }
    });

    rx
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum LocalEvent {
    Detach,
    Scroll(i32),
}

pub struct StdinProcessor {
    parser: vte::Parser,
    performer: StdinPerformer,
    saw_escape: bool,
}

impl StdinProcessor {
    pub fn new(page_size: i32) -> Self {
        Self {
            parser: vte::Parser::new(),
            performer: StdinPerformer {
                row_offset: 1,
                alt_screen: false,
                mouse_on: false,
                app_cursor: false,
                page_size,
                events: Vec::new(),
                pty_output: BytesMut::new(),
            },
            saw_escape: false,
        }
    }

    pub fn set_state(
        &mut self,
        alt_screen: bool,
        mouse_on: bool,
        page_size: i32,
        app_cursor: bool,
    ) {
        self.performer.alt_screen = alt_screen;
        self.performer.mouse_on = mouse_on;
        self.performer.page_size = page_size;
        self.performer.app_cursor = app_cursor;
    }

    pub fn process(&mut self, incoming: &[u8]) -> (Vec<LocalEvent>, Bytes) {
        self.performer.events.clear();

        for &byte in incoming {
            // Fix for 0x7F (DEL) which VT500 standard ignores silently:
            if byte == 0x7f {
                if self.saw_escape {
                    // terminal sent \x1b\x7f (e.g. Alt+Backspace / Ctrl+Backspace)
                    // Reset parser back to Ground state and forward \x1b\x7f
                    self.parser = vte::Parser::new();
                    self.performer.pty_output.extend_from_slice(b"\x1b\x7f");
                    self.saw_escape = false;
                } else {
                    // Regular Backspace (DEL / 0x7F)
                    self.performer.pty_output.extend_from_slice(&[0x7f]);
                }
                continue;
            }

            // Track if single \x1b was seen to handle \x1b\x7f streaming across chunks
            if byte == 0x1b {
                if self.saw_escape {
                    // Double escape: flush first escape to PTY
                    self.performer.pty_output.extend_from_slice(&[0x1b]);
                }
                self.saw_escape = true;
                self.parser.advance(&mut self.performer, &[byte]);
                continue;
            }

            self.saw_escape = false;
            // Let vte stream-tokenize everything else safely
            self.parser.advance(&mut self.performer, &[byte]);
        }

        let events = self.performer.events.clone();
        let pty_bytes = self.performer.pty_output.split().freeze();

        (events, pty_bytes)
    }
}

pub struct StdinPerformer {
    pub row_offset: u16,
    pub alt_screen: bool,
    pub mouse_on: bool,
    pub app_cursor: bool,
    pub page_size: i32,
    pub events: Vec<LocalEvent>,
    pub pty_output: BytesMut,
}

impl Perform for StdinPerformer {
    // 1. Regular character typing
    fn print(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.pty_output
            .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }

    // 2. Control characters (Ctrl+C, \r, \n, \t, Ctrl+H, etc.)
    fn execute(&mut self, byte: u8) {
        if byte == 0x1d {
            self.events.push(LocalEvent::Detach);
            return;
        }
        self.pty_output.extend_from_slice(&[byte]);
    }

    // 3. CSI Sequences (Mouse, Arrow keys, PageUp/Down, etc.)
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let p_list: Vec<&[u16]> = params.iter().collect();

        // -------------------------------------------------------------
        // FILTER 1: Drop unsolicited terminal auto-responses from stdin
        // (CPR: \x1b[..R, DSR: \x1b[0n, DA1: \x1b[?...c)
        // -------------------------------------------------------------
        #[allow(clippy::match_same_arms)]
        match (action, intermediates) {
            // CPR Cursor Position Report response (e.g. \x1b[24;80R)
            ('R', []) => return,
            // DSR Device Status Report response (\x1b[0n)
            ('n', []) if p_list.as_slice() == [[0]] => return,
            // Primary Device Attributes response (\x1b[?62;...c)
            ('c', b"?") => return,
            _ => {}
        }

        // --- LOCAL SCROLL CHECK (when NOT in alternate screen) ---
        if !self.alt_screen {
            // PageUp: CSI 5 ~  |  Shift+PageUp: CSI 5 ; 2 ~
            if intermediates.is_empty() && action == '~' {
                match p_list.as_slice() {
                    [[5]] | [[5], [2]] => {
                        self.events.push(LocalEvent::Scroll(self.page_size));
                        return;
                    }
                    [[6]] | [[6], [2]] => {
                        self.events.push(LocalEvent::Scroll(-self.page_size));
                        return;
                    }
                    _ => {}
                }
            }

            // Ctrl+Up: CSI 1 ; 5 A  |  Ctrl+Down: CSI 1 ; 5 B
            if intermediates.is_empty()
                && let [[1], [5]] = p_list.as_slice()
            {
                if action == 'A' {
                    self.events.push(LocalEvent::Scroll(1));
                    return;
                } else if action == 'B' {
                    self.events.push(LocalEvent::Scroll(-1));
                    return;
                }
            }
        }

        // --- SGR MOUSE EVENTS: CSI < btn ; col ; row (M|m) ---
        if intermediates == b"<" && (action == 'M' || action == 'm') {
            let mut it = params.iter();
            if let (Some(&[btn]), Some(&[col]), Some(&[row])) = (it.next(), it.next(), it.next()) {
                // SGR Wheel Scroll (64: wheel up, 65: wheel down) on press ('M')
                if !self.alt_screen && action == 'M' && (btn == 64 || btn == 65) {
                    let delta = if btn == 64 { 3 } else { -3 };
                    self.events.push(LocalEvent::Scroll(delta));
                    return;
                }

                // If remote application didn't enable mouse, discard mouse sequences in main screen
                if !self.alt_screen && !self.mouse_on {
                    return;
                }

                // Discard click if it hits the status bar
                if row <= self.row_offset {
                    return;
                }

                // Adjust row offset and forward to PTY
                let adjusted_row = row - self.row_offset;
                let seq = format!("\x1b[<{btn};{col};{adjusted_row}{action}");
                self.pty_output.extend_from_slice(seq.as_bytes());
                return;
            }
        }

        // --- ARROW KEYS & CURSOR NAVIGATION (Up, Down, Right, Left, Home, End) ---
        // Handle plain arrows with no explicit parameter or default 0/1 param
        let is_plain_cursor = intermediates.is_empty()
            && (p_list.is_empty() || p_list.as_slice() == [[0]] || p_list.as_slice() == [[1]]);

        if is_plain_cursor && matches!(action, 'A' | 'B' | 'C' | 'D' | 'H' | 'F') {
            if self.app_cursor {
                // Application Cursor Mode -> \x1bOA .. \x1bOD
                self.pty_output.extend_from_slice(b"\x1bO");
                self.pty_output.extend_from_slice(&[action as u8]);
            } else {
                // Normal Cursor Mode -> \x1b[A .. \x1b[D (NEVER \x1b[0A!)
                self.pty_output.extend_from_slice(b"\x1b[");
                self.pty_output.extend_from_slice(&[action as u8]);
            }
            return;
        }

        // --- FALLBACK: Forward any other CSI sequence to PTY ---
        self.pty_output.extend_from_slice(b"\x1b[");
        self.pty_output.extend_from_slice(intermediates);

        // Only serialize parameters if they were explicitly present
        if !p_list.is_empty() && p_list.as_slice() != [[0]] {
            let mut first = true;
            for subparam in params {
                if !first {
                    self.pty_output.extend_from_slice(b";");
                }
                first = false;
                let mut sub_first = true;
                for &val in subparam {
                    if !sub_first {
                        self.pty_output.extend_from_slice(b":");
                    }
                    sub_first = false;
                    self.pty_output
                        .extend_from_slice(val.to_string().as_bytes());
                }
            }
        }

        let mut action_buf = [0u8; 4];
        self.pty_output
            .extend_from_slice(action.encode_utf8(&mut action_buf).as_bytes());
    }

    // 4. Escape sequences (e.g. Alt+keys)
    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.pty_output.extend_from_slice(&[0x1b]);
        self.pty_output.extend_from_slice(intermediates);
        self.pty_output.extend_from_slice(&[byte]);
    }
}

struct HelperLifetime {
    size: PtySize,
    deadline: Instant,
}

pub struct TerminalSizeNegotiator {
    local_size: PtySize,
    helpers: HashMap<HelperId, HelperLifetime>,
}

impl TerminalSizeNegotiator {
    pub fn new(local_size: PtySize) -> Self {
        Self {
            local_size,
            helpers: HashMap::new(),
        }
    }

    /// Update local window size
    pub fn update_local(&mut self, size: PtySize) -> PtySize {
        self.local_size = size;
        self.best_size()
    }

    /// Update or insert a remote helper's size hint
    pub fn update_helper(&mut self, id: HelperId, size: PtySize) -> PtySize {
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
    pub fn remove_helper(&mut self, id: HelperId) -> PtySize {
        self.helpers.remove(&id);
        self.best_size()
    }

    /// Calculate the minimum size across local terminal and all active helpers
    pub fn best_size(&mut self) -> PtySize {
        let now = Instant::now();
        self.helpers.retain(|_, h| now < h.deadline);

        self.helpers
            .values()
            .map(|h| h.size)
            .fold(self.local_size, super::protocol::PtySize::min_dimensions)
    }
}
