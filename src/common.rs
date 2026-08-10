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

use crate::screen::StatusBarHandle;

pub const ALPN: &[u8; 12] = b"rescue-shell";

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
