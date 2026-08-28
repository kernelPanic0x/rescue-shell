use crate::{
    ConnectArgs, app_config,
    common::{ALPN, CODEC_BUFFER_SIZE, ConnectionStateWatcher, QUEUE_SIZE},
    console::{
        LocalConsole, LocalEvent, Osc52Extractor, Role, StatusBarHandle, StdinProcessor,
        window_change_signal,
    },
    protocol::{Encoder, HandshakePayload, HelperId, PtySize, TIMEOUT, ToHelper, ToVictim},
};
use color_eyre::eyre::{Context, eyre};
use futures_util::{SinkExt, StreamExt};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, SecretKey, endpoint::presets};
use magic_wormhole::{MailboxConnection, Wormhole};
use parking_lot::Mutex;
use std::time::Duration;
use tokio::{io::BufReader, sync::mpsc, time::timeout};
use tokio_util::codec::LengthDelimitedCodec;

pub struct VictimHub {
    to_victim_tx: mpsc::Sender<ToVictim>,
    from_victim_rx: tokio::sync::Mutex<mpsc::Receiver<ToHelper>>,
    #[allow(unused)]
    link: Link,
}

impl VictimHub {
    pub async fn connect(
        args: ConnectArgs,
        statusbar_handle: StatusBarHandle,
    ) -> color_eyre::Result<Self> {
        let (to_victim_tx, to_victim_rx) = mpsc::channel::<ToVictim>(QUEUE_SIZE);
        let (from_victim_tx, from_victim_rx) = mpsc::channel::<ToHelper>(QUEUE_SIZE);

        let link = timeout(
            Duration::from_secs(10),
            Link::connect(args, statusbar_handle, to_victim_rx, from_victim_tx),
        )
        .await??;

        Ok(Self {
            to_victim_tx,
            from_victim_rx: tokio::sync::Mutex::new(from_victim_rx),
            link,
        })
    }

    pub async fn send(&self, msg: ToVictim) -> color_eyre::Result<()> {
        self.to_victim_tx.send(msg).await.map_err(|e| eyre!(e))
    }

    pub async fn recv(&self) -> Option<ToHelper> {
        self.from_victim_rx.lock().await.recv().await
    }

    pub async fn close_with_bye(self, id: HelperId) {
        let _ = self.to_victim_tx.send(ToVictim::Bye { id }).await;
        drop(self.to_victim_tx);
        self.link.shutdown().await;
    }
}

#[derive(Default)]
pub struct Helper;

impl Helper {
    pub async fn run(args: ConnectArgs) -> color_eyre::Result<()> {
        let id = getrandom::u64()?.into();
        let statusbar_handle = StatusBarHandle::new(Role::Helper);
        let hub = VictimHub::connect(args, statusbar_handle.clone()).await?;
        let console = LocalConsole::new(&statusbar_handle)?;
        console.render().await?;
        let mut statusbar_rx = statusbar_handle.subscribe();

        let mut osc52_extractor = Osc52Extractor::default();

        // Send initial terminal dimensions to victim shell
        hub.send(ToVictim::SizeHint {
            id,
            size: LocalConsole::get_pty_size(),
        })
        .await?;

        let mut sigwinch = window_change_signal();
        let mut screen_size_resend =
            tokio::time::interval(Duration::from_secs((TIMEOUT / 2).as_secs()));

        let pty_size: PtySize = LocalConsole::get_pty_size();
        let mut stdin_processor = StdinProcessor::new(pty_size.rows.into());

        let res: color_eyre::Result<()> = 'main_loop: loop {
            tokio::select! {
                // Peroidically resend term size to keep TermSizeNegotiator alive
                _ = screen_size_resend.tick() => {
                    hub.send(ToVictim::SizeHint { id, size: LocalConsole::get_pty_size() }).await?;
                }

                // Status bar update -> render frame locally
                _ = statusbar_rx.changed() => {
                    console.render().await?;
                }

                // Raw stdin bytes -> filter -> send to victim
                Some(bytes) = console.read_stdin() => {
                    let (alt, mouse_on, scrolled, app_cursor) = {
                        console.access_parser_mut(|p| (
                            p.screen().alternate_screen(),
                            p.screen().mouse_protocol_mode()!= vt100::MouseProtocolMode::None,
                            p.screen().scrollback() > 0,
                            p.screen().application_cursor(),
                        ))
                    };
                    let pty_size: PtySize = LocalConsole::get_pty_size();

                    stdin_processor.set_state(alt, mouse_on, pty_size.rows.into(), app_cursor);

                    // Parse bytes safely (streaming tokenizer)
                    let (events, pty_bytes) = stdin_processor.process(&bytes);

                    // 1. Handle local events
                    for event in events {
                        match event {
                            LocalEvent::Detach => break 'main_loop Ok(()),
                            LocalEvent::Scroll(delta) => {
                                let offset = console.apply_scroll(delta.try_into()?).try_into()?;
                                hub.send(ToVictim::RequestScrollTo { offset }).await?;
                                console.render().await?;
                            }
                        }
                    }

                    // 2. Reset scrollback if user typed/forwarded regular keys while scrolled back
                    if !alt && scrolled && !pty_bytes.is_empty() {
                        console.access_parser_mut(|p| p.screen_mut().set_scrollback(0));
                        hub.send(ToVictim::RequestScrollTo { offset: 0 }).await?;
                        console.render().await?;
                    }

                    hub.send(ToVictim::Data(pty_bytes)).await?;
                }

                // Window resize signal (SIGWINCH)
                _ = sigwinch.recv() => {
                    hub.send(ToVictim::SizeHint { id, size: LocalConsole::get_pty_size() } ).await?;
                }

                // Victim output -> feed into local VT parser & render screen
                incoming = hub.recv() => {
                    match incoming {
                        Some(ToHelper::Data(bytes)) => {
                            if let Some(output) = osc52_extractor.extract(&bytes) {
                                console.write_stdout(output).await?;
                            }

                            console.access_parser_mut(|p| p.process(&bytes));
                            console.render().await?;
                        }
                        Some(ToHelper::Bye) => break Ok(()),
                        Some(ToHelper::ConnectedHelpers(n)) => {
                            statusbar_handle.set_connected(n);
                        },
                        Some(ToHelper::SetSize(size)) => {
                            // Negotiated size from victim
                            console.resize_parser(size);
                            console.render().await?;
                            // TODO: draw border if screen size < term size
                        },
                        Some(ToHelper::ScrollTo { offset }) => {
                            console.access_parser_mut(|p| p.screen_mut().set_scrollback(offset as usize));
                            console.render().await?;
                        }
                        None => break Err(eyre!("channel closed")).context("Recv from victim"),
                    }
                }
            }
        };

        hub.close_with_bye(id).await;
        console.flush_and_close().await;

        res
    }
}

struct Link {
    #[allow(unused)]
    endpoint: Endpoint,
    writer_handle: Mutex<Option<tokio::task::JoinHandle<color_eyre::Result<()>>>>,
}

impl Link {
    async fn connect(
        args: ConnectArgs,
        statusbar_handle: StatusBarHandle,
        mut to_victim_rx: mpsc::Receiver<ToVictim>,
        from_victim_tx: mpsc::Sender<ToHelper>,
    ) -> color_eyre::Result<Self> {
        let secret_key = args
            .common
            .private_key
            .clone()
            .unwrap_or_else(SecretKey::generate);
        let public_key = secret_key.public();

        let mailbox = MailboxConnection::connect(
            app_config(&args.common),
            args.common.code.expect("code always set"),
            false,
        )
        .await?;
        let mut wormhole = Wormhole::connect(mailbox).await?;
        let bytes = wormhole.receive().await?;
        let payload: HandshakePayload = wincode::deserialize(&bytes)?;

        wormhole.send(public_key.to_vec()).await?;

        #[cfg(target_os = "android")]
        let endpoint = {
            use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

            use iroh::dns::DnsResolver;

            Endpoint::builder(presets::Minimal)
                .secret_key(secret_key)
                .dns_resolver(DnsResolver::with_nameserver(SocketAddr::V4(
                    SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53),
                )))
                .relay_mode(RelayMode::Default)
                .bind()
                .await?
        };

        #[cfg(not(target_os = "android"))]
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(secret_key)
            .relay_mode(RelayMode::Default)
            .bind()
            .await?;

        let mut target_addr = EndpointAddr::new(PublicKey::from_bytes(&payload.public_key)?);
        if let Some(relay_str) = payload.relay_url {
            target_addr = target_addr.with_relay_url(relay_str.parse()?);
        }

        for ip in payload.direct_addresses {
            target_addr = target_addr.with_ip_addr(ip);
        }

        let connection_watcher =
            ConnectionStateWatcher::new(endpoint.clone(), statusbar_handle.clone());
        tokio::task::spawn(async move { connection_watcher.watch().await });

        let connection =
            timeout(Duration::from_secs(5), endpoint.connect(target_addr, ALPN)).await??;

        let (tx, rx) = connection.open_bi().await?;
        let encoder = async_compression::tokio::write::Lz4Encoder::new(tx);
        let decoder = async_compression::tokio::bufread::Lz4Decoder::new(BufReader::new(rx));
        let mut codec_builder = LengthDelimitedCodec::builder();
        codec_builder.max_frame_length(CODEC_BUFFER_SIZE);
        let mut raw_writer =
            tokio_util::codec::FramedWrite::new(encoder, codec_builder.new_codec());
        let mut raw_reader = tokio_util::codec::FramedRead::new(decoder, codec_builder.new_codec());

        // 1. Reader Task (Victim -> Helper)
        tokio::spawn(async move {
            while let Some(res) = raw_reader.next().await {
                match res {
                    Ok(bytes) => {
                        if let Ok(msg) = ToHelper::decode(&bytes) {
                            let _ = from_victim_tx.send(msg).await;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // 2. Writer Task (Helper -> Victim)
        let writer_handle = tokio::spawn(async move {
            while let Some(msg) = to_victim_rx.recv().await {
                if let Ok(encoded) = msg.encode()
                    && raw_writer.send(encoded).await.is_err()
                    && raw_writer.flush().await.is_err()
                {
                    break;
                }
            }

            let _ = raw_writer.flush().await;
            let _ = raw_writer.close().await;

            Ok::<(), color_eyre::eyre::Error>(())
        });

        Ok(Self {
            endpoint,
            writer_handle: Mutex::new(Some(writer_handle)),
        })
    }

    pub async fn shutdown(&self) {
        let handle = self.writer_handle.lock().take();

        if let Some(h) = handle {
            let _ = tokio::time::timeout(Duration::from_millis(500), h).await;
        }

        self.endpoint.close().await;
    }
}
