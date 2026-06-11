//! The Wi-Fi sync session task.
//!
//! Parked until the app sends `SyncCommand::Start`. The session is one
//! way by design: it asks the display task to loan the reader's scratch
//! memory (plus dram2) as radio heap, joins the configured network in
//! STA mode, exchanges the active book's position with a kosync server,
//! and the only path back to reading is the software reset on
//! `SyncCommand::Exit`. Credentials are compile-time options for the
//! dev-bring-up phase; AP-mode onboarding replaces them later.

use crate::sync_mem::{self, SyncBookInfo, SyncLoan};
use crate::{
    StorageCommand, SyncCommand, SyncEvent, STORAGE_COMMANDS, SYNC_COMMANDS, SYNC_EVENTS,
    SYNC_LOANS,
};
use app_core::{PersistedAppState, SyncError};
use embassy_executor::Spawner;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config as NetConfig, IpAddress, Runner, Stack, StackResources};
use embassy_time::{with_timeout, Duration, Timer};
use esp_hal::peripherals::{RADIO_CLK, RNG, SYSTIMER, WIFI};
use esp_hal::rng::Rng;
use esp_hal::timer::systimer::SystemTimer;
use esp_wifi::wifi::{
    new_with_mode, AuthMethod, ClientConfiguration, Configuration, WifiController, WifiDevice,
    WifiStaDevice,
};
use esp_wifi::EspWifiController;
use proto::kosync;

const JOIN_TIMEOUT: Duration = Duration::from_secs(20);
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const DEVICE_NAME: &str = "xteink-x4";

/// Compile-time station credentials for the dev phase:
/// `XTEINK_WIFI_SSID=... XTEINK_WIFI_PASS=... cargo build ...`
pub fn credentials() -> Option<(&'static str, &'static str)> {
    Some((
        option_env!("XTEINK_WIFI_SSID")?,
        option_env!("XTEINK_WIFI_PASS")?,
    ))
}

/// kosync account, also compile-time for now. Host accepts `host` or
/// `host:port`; plain HTTP, so self-hosted servers are the v1 target.
fn kosync_account() -> Option<(&'static str, &'static str, &'static str)> {
    Some((
        option_env!("XTEINK_KOSYNC_HOST")?,
        option_env!("XTEINK_KOSYNC_USER")?,
        option_env!("XTEINK_KOSYNC_PASS")?,
    ))
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
pub async fn run(spawner: Spawner, wifi: WIFI, systimer: SYSTIMER, rng: RNG, radio_clk: RADIO_CLK) {
    // Idle until the first Start; Exit before any radio work is a no-op
    // because nothing has been loaned yet.
    loop {
        match SYNC_COMMANDS.receive().await {
            SyncCommand::Start => break,
            SyncCommand::Exit => {}
        }
    }

    let Some((ssid, password)) = credentials() else {
        // The reducer keeps Confirm inert without credentials; reaching
        // here means the build and the screen disagree, so say so.
        send_event(SyncEvent::Failed(SyncError::NoCredentials));
        return;
    };

    // The loan request runs through the storage queue so it serializes
    // behind any in-flight SD work, then the memory comes back to us.
    STORAGE_COMMANDS.send(StorageCommand::LoanSyncMemory).await;
    let loan = SYNC_LOANS.receive().await;
    sync_mem::donate_heap(loan.heap_a, loan.heap_b);
    let SyncLoan {
        tcp_rx,
        tcp_tx,
        http_a,
        http_b,
        book,
        ..
    } = loan;

    let mut rng = Rng::new(rng);
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let timer = SystemTimer::new(systimer);
    let inited = match esp_wifi::init(timer.alarm0, rng, radio_clk) {
        Ok(inited) => inited,
        Err(err) => {
            esp_println::println!("wifi: init failed: {:?}", err);
            send_event(SyncEvent::Failed(SyncError::RadioInit));
            park_until_exit().await;
        }
    };
    // Everything radio-shaped lives in the loaned heap; Box::leak is
    // honest here because the session never ends except by reset.
    let inited: &'static EspWifiController<'static> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(inited));
    let (device, controller) = match new_with_mode(inited, wifi, WifiStaDevice) {
        Ok(parts) => parts,
        Err(err) => {
            esp_println::println!("wifi: sta mode failed: {:?}", err);
            send_event(SyncEvent::Failed(SyncError::RadioInit));
            park_until_exit().await;
        }
    };

    let resources: &'static mut StackResources<4> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(StackResources::new()));
    let (stack, runner) = embassy_net::new(
        device,
        NetConfig::dhcpv4(Default::default()),
        resources,
        seed,
    );
    if spawner.spawn(net_task(runner)).is_err() {
        send_event(SyncEvent::Failed(SyncError::RadioInit));
        park_until_exit().await;
    }

    let mut session = Session {
        controller,
        stack,
        tcp_rx,
        tcp_tx,
        http_a,
        http_b,
        book,
        started: false,
    };

    // First Start already consumed; later Starts are Confirm retries
    // from the error screen.
    loop {
        let event = match session.attempt(ssid, password).await {
            Ok(event) => event,
            Err(error) => SyncEvent::Failed(error),
        };
        send_event(event);
        // Both arms leave this state: Start retries the session, Exit
        // resets the device.
        match SYNC_COMMANDS.receive().await {
            SyncCommand::Start => {}
            SyncCommand::Exit => reset_now(),
        }
    }
}

async fn park_until_exit() -> ! {
    loop {
        if let SyncCommand::Exit = SYNC_COMMANDS.receive().await {
            reset_now();
        }
    }
}

fn reset_now() -> ! {
    esp_println::println!("wifi: sync session over, resetting");
    // Let the message drain the UART before the reset takes the port.
    esp_hal::reset::software_reset();
    #[allow(clippy::empty_loop)]
    loop {}
}

fn send_event(event: SyncEvent) {
    if SYNC_EVENTS.try_send(event).is_err() {
        esp_println::println!("wifi: sync event queue full");
    }
}

struct Session {
    controller: WifiController<'static>,
    stack: Stack<'static>,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    http_a: &'static mut [u8],
    http_b: &'static mut [u8],
    book: Option<SyncBookInfo>,
    started: bool,
}

impl Session {
    async fn attempt(&mut self, ssid: &str, password: &str) -> Result<SyncEvent, SyncError> {
        send_event(SyncEvent::Connecting);
        self.join(ssid, password).await?;

        let config = with_timeout(DHCP_TIMEOUT, async {
            loop {
                if let Some(config) = self.stack.config_v4() {
                    return config;
                }
                Timer::after_millis(100).await;
            }
        })
        .await
        .map_err(|_| SyncError::Dhcp)?;
        let ip = config.address.address().octets();
        esp_println::println!("wifi: up at {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        send_event(SyncEvent::Connected(ip));

        let Some((host_port, username, password)) = kosync_account() else {
            // Wi-Fi works but there is no server to talk to; report an
            // honest "nothing exchanged" so the screen has an ending.
            return Ok(SyncEvent::Done {
                pushed: false,
                pulled: false,
            });
        };
        send_event(SyncEvent::Syncing);
        let key_hex = kosync::hex_digest(kosync::md5(password.as_bytes()));
        let (host, port) = split_host_port(host_port);
        let account = kosync::Account {
            host,
            port,
            username,
            key_hex: &key_hex,
        };
        self.exchange(&account).await
    }

    async fn join(&mut self, ssid: &str, password: &str) -> Result<(), SyncError> {
        if !self.started {
            let config = Configuration::Client(ClientConfiguration {
                ssid: ssid.try_into().map_err(|_| SyncError::Join)?,
                password: password.try_into().map_err(|_| SyncError::Join)?,
                auth_method: if password.is_empty() {
                    AuthMethod::None
                } else {
                    AuthMethod::WPA2Personal
                },
                ..Default::default()
            });
            self.controller
                .set_configuration(&config)
                .map_err(|_| SyncError::Join)?;
            self.controller
                .start_async()
                .await
                .map_err(|_| SyncError::Join)?;
            self.started = true;
        }
        with_timeout(JOIN_TIMEOUT, self.controller.connect_async())
            .await
            .map_err(|_| SyncError::Join)?
            .map_err(|err| {
                esp_println::println!("wifi: join failed: {:?}", err);
                SyncError::Join
            })
    }

    /// GET the server position, then push or pull whichever side is
    /// ahead. Equal positions exchange nothing.
    async fn exchange(&mut self, account: &kosync::Account<'_>) -> Result<SyncEvent, SyncError> {
        // Cloned so the borrow does not pin `self` across the socket work.
        let Some(book) = self.book.clone() else {
            return Ok(SyncEvent::Done {
                pushed: false,
                pulled: false,
            });
        };
        let book = &book;
        let document_hex = kosync::hex_digest(book.document_md5);
        let device_id = kosync::hex_digest(kosync::md5(DEVICE_NAME.as_bytes()));

        let address = self.resolve(account.host).await?;

        let request_len = kosync::build_get_progress_request(self.http_a, account, &document_hex)
            .ok_or(SyncError::Protocol)?;
        let response_len = self
            .http_round_trip(address, account.port, request_len)
            .await?;
        let response =
            kosync::parse_response(&self.http_b[..response_len]).ok_or(SyncError::Protocol)?;
        let server_permille = match response.status {
            200 => kosync::parse_percentage_permille(response.body),
            // 404/502 from kosync mean "no position stored yet".
            404 | 502 => None,
            401 | 403 => return Err(SyncError::Protocol),
            _ => return Err(SyncError::Protocol),
        };

        let local_permille = book.percent_permille;
        let (push, pull) = match server_permille {
            Some(server) if server > local_permille => (false, true),
            Some(server) if server < local_permille => (true, false),
            Some(_) => (false, false),
            None => (true, false),
        };

        if pull {
            let server = server_permille.unwrap_or(0);
            let record = pulled_record(book, server);
            STORAGE_COMMANDS
                .send(StorageCommand::StoreProgress(record))
                .await;
            esp_println::println!("kosync: pulled permille={}", server);
        }
        if push {
            let request_len = kosync::build_put_progress_request(
                self.http_a,
                account,
                &document_hex,
                local_permille,
                book.doc_fragment_1based as usize,
                DEVICE_NAME,
                &device_id,
            )
            .ok_or(SyncError::Protocol)?;
            let response_len = self
                .http_round_trip(address, account.port, request_len)
                .await?;
            let response =
                kosync::parse_response(&self.http_b[..response_len]).ok_or(SyncError::Protocol)?;
            if !matches!(response.status, 200 | 202) {
                return Err(SyncError::Protocol);
            }
            esp_println::println!("kosync: pushed permille={}", local_permille);
        }
        Ok(SyncEvent::Done {
            pushed: push,
            pulled: pull,
        })
    }

    async fn resolve(&mut self, host: &str) -> Result<IpAddress, SyncError> {
        if let Ok(address) = host.parse::<core::net::Ipv4Addr>() {
            return Ok(IpAddress::v4(
                address.octets()[0],
                address.octets()[1],
                address.octets()[2],
                address.octets()[3],
            ));
        }
        let addresses = with_timeout(HTTP_TIMEOUT, self.stack.dns_query(host, DnsQueryType::A))
            .await
            .map_err(|_| SyncError::Server)?
            .map_err(|_| SyncError::Server)?;
        addresses.first().copied().ok_or(SyncError::Server)
    }

    /// One request/response on a fresh connection; both sides use
    /// `connection: close`, so EOF delimits the response.
    async fn http_round_trip(
        &mut self,
        address: IpAddress,
        port: u16,
        request_len: usize,
    ) -> Result<usize, SyncError> {
        let mut socket = TcpSocket::new(self.stack, self.tcp_rx, self.tcp_tx);
        socket.set_timeout(Some(HTTP_TIMEOUT));
        socket
            .connect((address, port))
            .await
            .map_err(|_| SyncError::Server)?;

        let mut written = 0;
        while written < request_len {
            let sent = socket
                .write(&self.http_a[written..request_len])
                .await
                .map_err(|_| SyncError::Server)?;
            if sent == 0 {
                return Err(SyncError::Server);
            }
            written += sent;
        }

        let mut filled = 0;
        loop {
            if filled == self.http_b.len() {
                break;
            }
            let read = socket
                .read(&mut self.http_b[filled..])
                .await
                .map_err(|_| SyncError::Server)?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        socket.close();
        Ok(filled)
    }
}

/// Maps a pulled server position back onto a saved-state record: page
/// from the permille, chapter from the shipped chapter start pages
/// (mirroring the app's `sd_chapter_for_page`).
fn pulled_record(book: &SyncBookInfo, server_permille: u16) -> PersistedAppState {
    let page_count = book.page_count.max(1);
    let position = (u64::from(server_permille) * u64::from(page_count)).div_ceil(1000);
    let screen = (position.max(1) as u32 - 1).min(page_count - 1);
    let mut chapter = 0u16;
    for index in 0..usize::from(book.chapter_count) {
        let start = *book.chapter_pages.get(index).unwrap_or(&0);
        if u32::from(start) <= screen {
            chapter = index as u16;
        } else {
            break;
        }
    }
    PersistedAppState {
        chapter,
        screen,
        ..book.persisted
    }
}

fn split_host_port(host_port: &str) -> (&str, u16) {
    match host_port.split_once(':') {
        Some((host, port)) => (host, port.parse().unwrap_or(80)),
        None => (host_port, 80),
    }
}
