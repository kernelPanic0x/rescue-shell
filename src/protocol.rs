use std::time::Duration;

use bytes::Bytes;
use wincode::{SchemaRead, SchemaWrite};

pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Every message on the wire. The wormhole channel is already
/// message-framed (send/receive Vec<u8>), so we just bincode-encode.
#[derive(SchemaWrite, SchemaRead, Debug, Clone)]
pub enum Msg {
    /// Terminal bytes. Direction depends on context:
    /// helper→victim: keystrokes for the PTY.
    /// victim→helper: shell output for the screen.
    Data(Bytes),

    /// Helper request a new size
    SizeHint {
        id: u64,
        size: TerminalSize,
    },
    /// Negotiated size
    SetSize(TerminalSize),

    /// Graceful shutdown either direction.
    Bye {
        id: u64,
    },

    ConnectedHelpers(u8),

    /// Authoritative scrollback viewport offset.
    /// `0` = live screen; `n` = n lines scrolled back (older content).
    /// Sent helper -> victim as a request, victim -> helpers as the synced state.
    ScrollTo {
        offset: u32,
    },
}

#[derive(SchemaWrite, SchemaRead, Debug, Clone, Copy, Default)]
pub struct TerminalSize {
    pub cols: u16,
    pub pty_rows: u16,
}

impl TerminalSize {
    pub fn min_dimensions(self, other: Self) -> Self {
        TerminalSize {
            cols: self.cols.min(other.cols),
            pty_rows: self.pty_rows.min(other.pty_rows),
        }
    }
}

pub fn encode(msg: &Msg) -> anyhow::Result<Bytes> {
    Ok(Bytes::from(wincode::serialize(msg)?))
}

pub fn decode(bytes: &[u8]) -> anyhow::Result<Msg> {
    Ok(wincode::deserialize(bytes)?)
}
