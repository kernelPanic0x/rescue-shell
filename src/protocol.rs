use magic_wormhole::transit;
use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

/// Every message on the wire. The wormhole channel is already
/// message-framed (send/receive Vec<u8>), so we just bincode-encode.
#[derive(SchemaWrite, SchemaRead, Debug, Clone)]
pub enum Msg {
    /// Terminal bytes. Direction depends on context:
    /// helper→victim: keystrokes for the PTY.
    /// victim→helper: shell output for the screen.
    Data(Vec<u8>),

    /// Helper's terminal size changed (or initial size on connect).
    Resize { cols: u16, rows: u16 },

    /// Graceful shutdown either direction.
    Bye,
}

pub fn encode(msg: &Msg) -> anyhow::Result<Vec<u8>> {
    Ok(wincode::serialize(msg)?)
}

pub fn decode(bytes: &[u8]) -> anyhow::Result<Msg> {
    Ok(wincode::deserialize(bytes)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handshake {
    pub abilities: transit::Abilities,
    pub hints: transit::Hints,
}

impl Handshake {
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }
}
