use bytes::Bytes;
use wincode::{SchemaRead, SchemaWrite};

/// Every message on the wire. The wormhole channel is already
/// message-framed (send/receive Vec<u8>), so we just bincode-encode.
#[derive(SchemaWrite, SchemaRead, Debug, Clone)]
pub enum Msg {
    /// Terminal bytes. Direction depends on context:
    /// helper→victim: keystrokes for the PTY.
    /// victim→helper: shell output for the screen.
    Data(Bytes),

    /// Helper's terminal size changed (or initial size on connect).
    Resize {
        cols: u16,
        rows: u16,
    },

    /// Graceful shutdown either direction.
    Bye,

    ConnectedHelpers(u8),
}

pub fn encode(msg: &Msg) -> anyhow::Result<Bytes> {
    Ok(Bytes::from(wincode::serialize(msg)?))
}

pub fn decode(bytes: &[u8]) -> anyhow::Result<Msg> {
    Ok(wincode::deserialize(bytes)?)
}
