// SPDX-License-Identifier: EUPL-1.2
// Copyright (c) 2026–present rescue-shell contributors

//! Stdin key classifier: key table + longest-prefix match — the same
//! architecture as tmux's `tty-keys.c` and crossterm's `event/sys/unix/parse.rs`.
//!
//! Rules that keep this small where the vte version kept growing hacks:
//!   1. Verbatim is the default. Only sequences that need rewriting, local
//!      handling, or filtering get a table row. Unknown input is forwarded —
//!      the failure mode is "the app sees the key", never "input freezes".
//!   2. Time resolves ESC. The stdin reader coalesces bytes while a burst can
//!      still grow into a sequence (`maybe_partial`); after `ESC_QUIET_PERIOD`
//!      of fd silence, a trailing ESC *is* a lone ESC press.
//!   3. No hidden state. `process` is a pure function of one complete burst.

#![expect(clippy::indexing_slicing)]

use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use std::io::Write;

use crate::console::LocalEvent;

/// Silence window after which a trailing ESC can no longer start a sequence.
/// tmux: 500 ms, vim `ttimeoutlen`: 50 ms, fish: 30 ms.
pub const ESC_QUIET_PERIOD: Duration = Duration::from_millis(50);

/// Physical row of the first PTY row (row 0 is the status bar).
const ROW_OFFSET: u16 = 1;

// ---------------------------------------------------------------------------
// Key table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Action {
    /// Forward the matched bytes unchanged.
    Forward,
    /// Cursor key: pick the CSI or SS3 (DECCKM) spelling for the remote app.
    Cursor {
        normal: &'static [u8],
        app: &'static [u8],
    },
    /// Scroll the local viewport by `n * page_size` lines. Forwarded verbatim
    /// while the remote app owns the screen (alternate screen).
    ScrollPage(i32),
    /// Scroll the local viewport by `n` lines (same alt-screen rule).
    ScrollLines(i32),
    /// Terminal auto-response — never a keystroke, swallow.
    Swallow,
}

macro_rules! cursor_key {
    ($c:literal) => {
        (
            &[0x1b, b'[', $c],
            Action::Cursor {
                normal: &[0x1b, b'[', $c],
                app: &[0x1b, b'O', $c],
            },
        )
    };
}

/// Longest match wins (see `table_match`). Deliberately minimal: F1–F12,
/// modified keys, kitty-protocol sequences etc. need NO row — the fallback
/// forwards them verbatim, which is exactly what the remote app expects.
static KEY_TABLE: &[(&[u8], Action)] = &[
    // Arrows/Home/End: physical terminals send CSI; remote apps with DECCKM
    // (application cursor mode) expect SS3.
    cursor_key!(b'A'),
    cursor_key!(b'B'),
    cursor_key!(b'C'),
    cursor_key!(b'D'),
    cursor_key!(b'H'),
    cursor_key!(b'F'),
    // ...and the reverse: a user terminal stuck in app mode (crashed program)
    // sends SS3; normalize back to CSI for normal-mode apps.
    (
        b"\x1bOA",
        Action::Cursor {
            normal: b"\x1b[A",
            app: b"\x1bOA",
        },
    ),
    (
        b"\x1bOB",
        Action::Cursor {
            normal: b"\x1b[B",
            app: b"\x1bOB",
        },
    ),
    (
        b"\x1bOC",
        Action::Cursor {
            normal: b"\x1b[C",
            app: b"\x1bOC",
        },
    ),
    (
        b"\x1bOD",
        Action::Cursor {
            normal: b"\x1b[D",
            app: b"\x1bOD",
        },
    ),
    (
        b"\x1bOH",
        Action::Cursor {
            normal: b"\x1b[H",
            app: b"\x1bOH",
        },
    ),
    (
        b"\x1bOF",
        Action::Cursor {
            normal: b"\x1b[F",
            app: b"\x1bOF",
        },
    ),
    // Legacy Home/End spellings (vt100 `1~`/`4~`, rxvt `7~`/`8~`).
    (
        b"\x1b[1~",
        Action::Cursor {
            normal: b"\x1b[1~",
            app: b"\x1bOH",
        },
    ),
    (
        b"\x1b[7~",
        Action::Cursor {
            normal: b"\x1b[7~",
            app: b"\x1bOH",
        },
    ),
    (
        b"\x1b[4~",
        Action::Cursor {
            normal: b"\x1b[4~",
            app: b"\x1bOF",
        },
    ),
    (
        b"\x1b[8~",
        Action::Cursor {
            normal: b"\x1b[8~",
            app: b"\x1bOF",
        },
    ),
    // Insert / Delete: no DECCKM variant.
    (b"\x1b[2~", Action::Forward),
    (b"\x1b[3~", Action::Forward),
    // Scrollback bindings (main screen only; verbatim in the alt screen).
    (b"\x1b[5~", Action::ScrollPage(1)),     // PageUp
    (b"\x1b[5;2~", Action::ScrollPage(1)),   // Shift+PageUp
    (b"\x1b[6~", Action::ScrollPage(-1)),    // PageDown
    (b"\x1b[6;2~", Action::ScrollPage(-1)),  // Shift+PageDown
    (b"\x1b[1;5A", Action::ScrollLines(1)),  // Ctrl+Up
    (b"\x1b[1;5B", Action::ScrollLines(-1)), // Ctrl+Down
    // DSR-OK auto-response (`\x1b[0n`).
    (b"\x1b[0n", Action::Swallow),
];

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

/// Drop-in replacement for the previous vte-based `StdinProcessor`:
/// same constructor, same `set_state`, same `process` signature.
pub struct StdinProcessor {
    alt_screen: bool,
    mouse_on: bool,
    app_cursor: bool,
    page_size: i32,
}

impl StdinProcessor {
    pub fn new(page_size: i32) -> Self {
        Self {
            alt_screen: false,
            mouse_on: false,
            app_cursor: false,
            page_size,
        }
    }

    /// Refresh the mirrored remote-screen state (caller does this before each
    /// burst, exactly as before).
    pub fn set_state(
        &mut self,
        alt_screen: bool,
        mouse_on: bool,
        page_size: i32,
        app_cursor: bool,
    ) {
        *self = Self {
            alt_screen,
            mouse_on,
            app_cursor,
            page_size,
        };
    }

    /// Classify one complete input burst into local events + PTY bytes.
    pub fn process(&mut self, mut input: &[u8]) -> (Vec<LocalEvent>, Bytes) {
        let mut events = Vec::new();
        let mut out = BytesMut::new();

        while let Some((&first, rest)) = input.split_first() {
            match first {
                0x1b => {
                    let consumed = self.escape(input, &mut events, &mut out);
                    input = input.get(consumed..).unwrap_or_default();
                }
                // Ctrl+]: local detach chord.
                0x1d => {
                    events.push(LocalEvent::Detach);
                    input = rest;
                }
                // Plain bytes (text, Ctrl+letters, DEL, UTF-8): forward.
                b => {
                    out.put_u8(b);
                    input = rest;
                }
            }
        }

        (events, out.freeze())
    }

    /// Handle a sequence starting at `seq[0] == 0x1b`; returns bytes consumed.
    fn escape(&mut self, seq: &[u8], events: &mut Vec<LocalEvent>, out: &mut BytesMut) -> usize {
        // 1. Fixed table, longest match wins.
        if let Some((len, action)) = table_match(seq) {
            return self.apply(action, &seq[..len], events, out);
        }

        // 2. Variable-length families that need span scanning.
        match seq.get(1) {
            // CSI: mouse, CPR/DA filtering, everything-else passthrough.
            Some(b'[') => self.csi(seq, events, out),
            // OSC (BEL/ST-terminated): forward verbatim.
            Some(b']') => forward_span(seq, st_length(seq, true), out),
            // DCS / SOS / PM / APC (ST-terminated): forward verbatim.
            Some(b'P' | b'X' | b'^' | b'_') => forward_span(seq, st_length(seq, false), out),
            // Alt+<printable> (`\x1bx`), SS3 leftovers (`\x1bO5`): forward the
            // chord; trailing bytes flow through the main loop unchanged, so
            // e.g. `\x1bOP` (F1) still ends up forwarded byte-for-byte.
            Some(&b) if (0x20..=0x7e).contains(&b) => {
                out.extend_from_slice(&seq[..2]);
                2
            }
            // Lone ESC (quiet period elapsed), or ESC + C0 control
            // (`\x1b\r` Alt+Enter, `\x1b\x7f` Alt+Backspace): forward the ESC
            // itself; the next byte is handled normally, so the chord still
            // reaches the PTY intact. No special cases needed.
            _ => {
                out.put_u8(0x1b);
                1
            }
        }
    }

    fn apply(
        &mut self,
        action: Action,
        matched: &[u8],
        events: &mut Vec<LocalEvent>,
        out: &mut BytesMut,
    ) -> usize {
        let len = matched.len();
        match action {
            Action::Forward => out.extend_from_slice(matched),
            Action::Swallow => {}
            Action::Cursor { normal, app } => {
                out.extend_from_slice(if self.app_cursor { app } else { normal });
            }
            Action::ScrollPage(n) => {
                if self.alt_screen {
                    out.extend_from_slice(matched);
                } else {
                    events.push(LocalEvent::Scroll(n * self.page_size));
                }
            }
            Action::ScrollLines(n) => {
                if self.alt_screen {
                    out.extend_from_slice(matched);
                } else {
                    events.push(LocalEvent::Scroll(n));
                }
            }
        }
        len
    }

    /// CSI family `\x1b[ P...p I...i F`: scan to the final byte, classify.
    fn csi(&mut self, seq: &[u8], events: &mut Vec<LocalEvent>, out: &mut BytesMut) -> usize {
        for (i, &b) in seq.iter().enumerate().skip(2) {
            match b {
                // Final byte: normal end of CSI sequence.
                0x40..=0x7e => return self.classify_csi(&seq[..=i], events, out),

                // Parameter or intermediate byte: keep scanning.
                0x20..=0x3f => {}

                // Malformed (control byte mid-CSI): forward what we scanned,
                // resume at the offending byte. Fail open.
                _ => {
                    out.extend_from_slice(&seq[..i]);
                    return i;
                }
            }
        }

        // No final byte and the quiet period already elapsed: orphan.
        // Fail open — forward verbatim (tmux does the same on its timeout).
        out.extend_from_slice(seq);
        seq.len()
    }

    fn classify_csi(
        &mut self,
        span: &[u8],
        events: &mut Vec<LocalEvent>,
        out: &mut BytesMut,
    ) -> usize {
        // Safely peel off the leading `\x1b[` and the trailing `final_byte`.
        // Leaves `params` as the middle slice without any manual index arithmetic.
        let Some((&final_byte, params)) = span
            .strip_prefix(b"\x1b[")
            .and_then(|rest| rest.split_last())
        else {
            out.extend_from_slice(span); // fail open
            return span.len();
        };

        match final_byte {
            // SGR mouse: `\x1b[<btn;col;rowM|m`.
            b'M' | b'm' => match params.strip_prefix(b"<") {
                Some(mouse_args) => self.mouse(span, mouse_args, final_byte, events, out),
                None => out.extend_from_slice(span),
            },
            // CPR auto-response `\x1b[<row>;<col>R`.
            b'R' => {}
            // DA1 auto-response `\x1b[?62;...c`.
            b'c' if params.starts_with(b"?") => {}
            // Everything else passes through untouched: F-keys, kitty-protocol, etc.
            _ => out.extend_from_slice(span),
        }

        span.len()
    }

    /// SGR mouse with 1-based coordinates. Wheel events scroll the local
    /// viewport; main-screen clicks are dropped unless the remote app enabled
    /// mouse reporting; status-bar clicks are always dropped; everything else
    /// is forwarded with the row shifted into PTY coordinates.
    fn mouse(
        &mut self,
        span: &[u8],
        params: &[u8],
        final_byte: u8,
        events: &mut Vec<LocalEvent>,
        out: &mut BytesMut,
    ) {
        let Some((btn, col, row)) = sgr_coords(params) else {
            out.extend_from_slice(span); // unparseable: fail open
            return;
        };

        if !self.alt_screen && final_byte == b'M' && (btn == 64 || btn == 65) {
            // Wheel on the main screen: local scroll (64 = up, 65 = down).
            events.push(LocalEvent::Scroll(if btn == 64 { 3 } else { -3 }));
        } else if !self.alt_screen && !self.mouse_on {
            // Remote app never enabled mouse: ignore clicks.
        } else if row <= u32::from(ROW_OFFSET) {
            // Click on the status bar.
        } else {
            let shifted = row - u32::from(ROW_OFFSET);
            let _ = write!(
                (out).writer(),
                "\x1b[<{btn};{col};{shifted}{}",
                final_byte as char
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Longest table entry that is a full prefix of `seq`.
fn table_match(seq: &[u8]) -> Option<(usize, Action)> {
    KEY_TABLE
        .iter()
        .filter(|(pat, _)| seq.starts_with(pat))
        .max_by_key(|(pat, _)| pat.len())
        .map(|(pat, action)| (pat.len(), *action))
}

/// Length of a string-terminated sequence starting at `seq[0] == 0x1b`
/// (OSC, optionally BEL-terminated; DCS/SOS/PM/APC, ST-terminated),
/// or `None` while unterminated.
fn st_length(seq: &[u8], bel_terminated: bool) -> Option<usize> {
    seq.iter().enumerate().skip(1).find_map(|(i, &b)| match b {
        0x07 if bel_terminated => Some(i + 1),
        0x1b if seq.get(i + 1) == Some(&b'\\') => Some(i + 2),
        _ => None,
    })
}

fn forward_span(seq: &[u8], len_opt: Option<usize>, out: &mut BytesMut) -> usize {
    let len = len_opt.unwrap_or(seq.len()); // unterminated orphan: fail open
    out.extend_from_slice(&seq[..len]);
    len
}

/// Parse `btn;col;row` of an SGR mouse sequence.
fn sgr_coords(params: &[u8]) -> Option<(u32, u32, u32)> {
    let mut it = params.split(|&b| b == b';');
    let parse = |s: Option<&[u8]>| std::str::from_utf8(s?).ok()?.parse().ok();
    Some((parse(it.next())?, parse(it.next())?, parse(it.next())?))
}

// ---------------------------------------------------------------------------
// Burst coalescing (stdin reader side)
// ---------------------------------------------------------------------------

/// True if `buf` might end inside a still-growing escape sequence, i.e. the
/// reader should keep reading while bytes keep arriving. After
/// `ESC_QUIET_PERIOD` of fd silence this only stays true for genuine orphans.
pub fn maybe_partial(buf: &[u8]) -> bool {
    let Some(pos) = buf.iter().rposition(|&b| b == 0x1b) else {
        return false; // no ESC at all: plain keystrokes/text, dispatch now
    };
    match buf.len() - pos {
        1 => true, // trailing lone ESC: could become any sequence
        _ => match buf[pos + 1] {
            b'[' => !buf[pos + 2..].iter().any(|&b| (0x40..=0x7e).contains(&b)),
            b']' => st_length(&buf[pos..], true).is_none(),
            b'P' | b'X' | b'^' | b'_' => st_length(&buf[pos..], false).is_none(),
            b'O' => buf.len() - pos == 2, // SS3 waiting for its final byte
            _ => false,                   // Alt+<byte> is complete as-is
        },
    }
}

/// Block up to `timeout` waiting for stdin to become readable.
#[cfg(unix)]
pub fn stdin_readable(timeout: Duration) -> bool {
    use std::os::fd::AsRawFd;

    let mut fds = [libc::pollfd {
        fd: std::io::stdin().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];

    #[expect(clippy::cast_possible_truncation, reason = "timeout never exceeds 50")]
    let millis = timeout.as_millis() as i32;

    // SAFETY: `fds` is a valid array of one pollfd for the whole call.
    unsafe { libc::poll(fds.as_mut_ptr(), 1, millis) > 0 }
}

/// `ConPTY` delivers VT input as complete per-read sequences, so the quiet
/// period is unnecessary on Windows.
#[cfg(not(unix))]
pub fn stdin_readable(_timeout: Duration) -> bool {
    false
}
