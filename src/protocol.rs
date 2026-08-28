use std::{net::SocketAddr, time::Duration};

use bytes::Bytes;
use wincode::{Deserialize, SchemaRead, SchemaWrite, Serialize};

pub const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(SchemaWrite, SchemaRead, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HelperId(pub u64);

impl From<u64> for HelperId {
    fn from(value: u64) -> Self {
        HelperId(value)
    }
}

/// Full terminal screen size
#[derive(SchemaRead, SchemaWrite, Debug, Clone, Copy, Default)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl PtySize {
    pub fn min_dimensions(self, other: Self) -> Self {
        PtySize {
            cols: self.cols.min(other.cols),
            rows: self.rows.min(other.rows),
        }
    }
}

pub trait Encoder: Sized {
    fn encode(&self) -> color_eyre::Result<Bytes>;
    fn decode(bytes: &[u8]) -> color_eyre::Result<Self>;
}

impl<T> Encoder for T
where
    T: Serialize<Src = T> + for<'de> Deserialize<'de, Dst = T>,
{
    fn encode(&self) -> color_eyre::Result<Bytes> {
        let serialized = wincode::serialize(self)?;
        Ok(Bytes::from(serialized))
    }

    fn decode(bytes: &[u8]) -> color_eyre::Result<Self> {
        let msg = wincode::deserialize(bytes)?;
        Ok(msg)
    }
}

#[derive(SchemaWrite, SchemaRead, Debug, Clone)]
pub enum ToVictim {
    Data(Bytes),
    SizeHint { id: HelperId, size: PtySize },
    Bye { id: HelperId },
    RequestScrollTo { offset: u16 },
}

#[derive(SchemaWrite, SchemaRead, Debug, Clone)]
pub enum ToHelper {
    Data(Bytes),
    SetSize(PtySize),
    Bye,
    ConnectedHelpers(u8),
    ScrollTo { offset: u16 },
}

#[derive(SchemaWrite, SchemaRead)]
pub struct HandshakePayload {
    pub public_key: [u8; 32],
    pub relay_url: Option<String>,
    pub direct_addresses: Vec<SocketAddr>,
}
