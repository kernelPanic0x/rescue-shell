use std::{fmt, pin::Pin};

use anyhow::Result;
use futures::{Sink, SinkExt, Stream, StreamExt};
use magic_wormhole::transit::{Transit, TransitError};

use crate::protocol::{Msg, decode, encode};

type Tx = Pin<Box<dyn Sink<Box<[u8]>, Error = TransitError> + Send>>;
type Rx = Pin<Box<dyn Stream<Item = Result<Box<[u8]>, TransitError>> + Send>>;

/// Sending half: encodes `Msg` and pushes it down the transit sink.
pub struct Sender(Tx);

/// Receiving half: pulls records off the transit stream and decodes `Msg`.
pub struct Receiver(Rx);

/// Split a freshly established transit into protocol-aware halves.
/// The `Box::pin` lives here and *only* here.
pub fn channel(transit: Transit) -> (Sender, Receiver) {
    let (tx, rx) = transit.split();
    (Sender(Box::pin(tx)), Receiver(Box::pin(rx)))
}

impl Sender {
    pub async fn send(&mut self, msg: &Msg) -> Result<()> {
        Ok(self.0.send(encode(msg)?.into_boxed_slice()).await?)
    }
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Result<Msg> {
        match self.0.next().await {
            Some(Ok(raw)) => decode(&raw),
            Some(Err(e)) => Err(e.into()),
            None => anyhow::bail!("transit connection closed"),
        }
    }
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}
