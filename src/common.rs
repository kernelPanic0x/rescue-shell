use std::{sync::Arc, time::Duration};

use crossterm::{
    cursor::Show,
    execute,
    style::{Attribute, SetAttribute},
    terminal::disable_raw_mode,
};
use iroh::{Endpoint, Watcher, endpoint::RelayStatus};
use magic_wormhole::{
    Key, Wormhole,
    transit::{self, TransitKey, TransitRole},
};
use tokio::time::sleep;

use crate::{protocol::Handshake, screen::StatusBarHandle};

/// Restore Victim host terminal mode on exit.
pub struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, SetAttribute(Attribute::Reset), Show);
        let _ = disable_raw_mode();
    }
}

pub fn is_detach_key(bytes: &[u8]) -> bool {
    // Ctrl+] (0x1D) detaches locally
    bytes.contains(&0x1d)
}

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

pub struct ConnectionStateWatcher {
    endpoint: Endpoint,
    statusbar_handle: StatusBarHandle,
}

impl ConnectionStateWatcher {
    pub fn new(endpoint: Endpoint, statusbar_handle: StatusBarHandle) -> Self {
        Self {
            endpoint,
            statusbar_handle,
        }
    }

    pub async fn watch(&self) {
        let mut relay_watcher = self.endpoint.home_relay_status();
        let mut net_report_watcher = self.endpoint.net_report();

        loop {
            let statuses = relay_watcher.get();

            // Find the first connected relay and extract its URL + latency
            let ping = statuses
                .iter()
                .find(|s| s.is_connected())
                .and_then(|status| {
                    let url = status.url();
                    // Look up that relay's best measured latency in the latest report
                    net_report_watcher
                        .get() // Option<NetReport>
                        .as_ref()
                        .and_then(|report| {
                            report
                                .relay_latency
                                .iter()
                                .filter(|(_, u, _)| *u == url)
                                .map(|(_, _, lat)| lat)
                                .min()
                        })
                });

            match ping {
                Some(ping) => {
                    self.statusbar_handle.online(ping);
                }
                None => {
                    self.statusbar_handle.offline();
                }
            }

            sleep(Duration::from_secs(1)).await;
        }
    }
}
