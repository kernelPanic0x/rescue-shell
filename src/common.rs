use std::sync::Arc;

use magic_wormhole::{
    Key, Wormhole,
    transit::{self, TransitKey, TransitRole},
};

use crate::protocol::Handshake;

/// TIOCGWINSZ on /dev/tty — for size negotiation when mirroring.
#[allow(unused)]
pub fn console_size() -> Option<(u16, u16)> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open("/dev/tty").ok()?;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
}

pub async fn establish_transit(
    wormhole: &mut Wormhole,
    relay_hints: Vec<transit::RelayHint>,
    abilities: transit::Abilities,
    role: TransitRole,
) -> anyhow::Result<transit::Transit> {
    // 1. Prepare our side (binds sockets, does STUN, if direct is allowed).
    let connector = transit::init(abilities, None, relay_hints).await?;

    // 2. Send our abilities + hints to the peer over the control channel.
    wormhole
        .send(
            Handshake {
                abilities: *connector.our_abilities(),
                hints: connector.our_hints().as_ref().clone(),
            }
            .encode()?,
        )
        .await?;

    let hs = Handshake::decode(&wormhole.receive().await?)?;

    // 4. Derive the transit key from the wormhole session key.
    let transit_key: Key<TransitKey> = wormhole
        .key()
        .derive_subkey_from_purpose::<TransitKey>("rescue-shell");

    // 5. Connect. Leader/Follower must not both be the same.
    let (transit, info) = connector
        .connect(role, transit_key, hs.abilities, Arc::new(hs.hints))
        .await?;

    println!("Transit established: {:?}", info.conn_type); // Direct or Relay
    Ok(transit)
}
