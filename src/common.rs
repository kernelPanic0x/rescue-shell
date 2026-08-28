use std::time::Duration;

use iroh::{Endpoint, Watcher};
use tokio::time::sleep;

use crate::console::StatusBarHandle;

pub const QUEUE_SIZE: usize = 1024;
pub const CODEC_BUFFER_SIZE: usize = 256 * 1024;

pub const ALPN: &[u8] = concat!("/", env!("CARGO_PKG_NAME"), "/").as_bytes();

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
                Some(p) => {
                    self.statusbar_handle.online(p);
                }
                None => {
                    self.statusbar_handle.offline();
                }
            }

            sleep(Duration::from_secs(1)).await;
        }
    }
}
