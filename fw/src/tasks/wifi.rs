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
use crate::upload::{sanitized_name, UploadBegin, UploadChunk};
use crate::{
    StorageCommand, SyncCommand, SyncEvent, STORAGE_COMMANDS, SYNC_COMMANDS, SYNC_EVENTS,
    SYNC_LOANS, UPLOAD_BEGINS, UPLOAD_CHUNKS, UPLOAD_RESULTS, UPLOAD_RETURNS,
};
use app_core::{PersistedAppState, SyncError, WifiCredentials};
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_futures::select::select;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{
    Config as NetConfig, IpAddress, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources,
    StaticConfigV4,
};
use embassy_time::{with_timeout, Duration, Timer};
use esp_hal::peripherals::{RADIO_CLK, RNG, SYSTIMER, WIFI};
use esp_hal::rng::Rng;
use esp_hal::timer::systimer::SystemTimer;
use esp_wifi::wifi::{
    new_with_mode, AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration,
    WifiApDevice, WifiController, WifiDevice, WifiStaDevice,
};
use esp_wifi::EspWifiController;
use proto::captive;
use proto::kosync;

// Measured first-association joins ran ~21 s; give them headroom.
const JOIN_TIMEOUT: Duration = Duration::from_secs(35);
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const DEVICE_NAME: &str = "xteink-x4";
const PORTAL_SSID: &str = "XTEINK-X4";
const PORTAL_IP: [u8; 4] = [192, 168, 4, 1];

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
async fn ap_net_task(mut runner: Runner<'static, WifiDevice<'static, WifiApDevice>>) -> ! {
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
        wifi: stored_credentials,
        catalog_len,
        ..
    } = loan;

    // Stored credentials from the portal beat the compile-time dev pair;
    // neither present means this session runs the onboarding portal.
    let resolved = stored_credentials.or_else(|| {
        credentials().and_then(|(ssid, password)| WifiCredentials::from_strs(ssid, password))
    });

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
    let Some(creds) = resolved else {
        run_portal(spawner, inited, wifi, seed, tcp_rx, tcp_tx, http_a, http_b).await;
    };

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

    // The kosync exchange gets heap scratch for responses; the loaned
    // http_b keeps the catalog listing the display task wrote for /list.
    let kosync_response: &'static mut [u8] = alloc::vec![0u8; 2048].leak();
    let mut session = Session {
        controller,
        stack,
        tcp_rx,
        tcp_tx,
        http_a,
        http_b: kosync_response,
        book,
        started: false,
    };

    // First Start already consumed; later Starts are Confirm retries
    // from the error screen. A completed exchange falls through to the
    // upload server, which runs until the session's reset.
    loop {
        let event = match session.attempt(creds.ssid(), creds.password()).await {
            Ok(event) => event,
            Err(error) => SyncEvent::Failed(error),
        };
        let done = matches!(event, SyncEvent::Done { .. });
        send_event(event);
        if done {
            break;
        }
        // Start retries the session, Exit resets the device.
        match SYNC_COMMANDS.receive().await {
            SyncCommand::Start => {}
            SyncCommand::Exit => reset_now(),
        }
    }

    let Session {
        stack,
        tcp_rx,
        tcp_tx,
        http_a,
        controller: _controller,
        ..
    } = session;
    let ip = stack
        .config_v4()
        .map(|config| config.address.address().octets())
        .unwrap_or(PORTAL_IP);
    esp_println::println!("upload: serving at {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
    send_event(SyncEvent::Serving(ip));
    select(
        exit_after_uploads(),
        upload_server(stack, tcp_rx, tcp_tx, http_a, http_b, catalog_len),
    )
    .await;
    unreachable!()
}

/// Exit during the serving phase defers the reset until any in-flight
/// book finishes writing (bounded), so a done press cannot truncate it.
async fn exit_after_uploads() -> ! {
    loop {
        if let SyncCommand::Exit = SYNC_COMMANDS.receive().await {
            let mut waited_ms = 0u32;
            while crate::upload::UPLOAD_IN_FLIGHT.load(portable_atomic::Ordering::SeqCst)
                && waited_ms < 120_000
            {
                Timer::after_millis(100).await;
                waited_ms += 100;
            }
            reset_now();
        }
    }
}

// ------------------------------------------------------------------
// Book upload server
// ------------------------------------------------------------------

const UPLOAD_PAGE: &str = concat!(
    r##"<!doctype html><html><head>"##,
    r##"<meta charset=utf-8>"##,
    r##"<meta name=viewport content="width=device-width,initial-scale=1">"##,
    r##"<title>Books · X4</title><style>"##,
    r##"body{font-family:Georgia,'Times New Roman',serif;margin:3em auto;"##,
    r##"max-width:26em;padding:0 1.2em;color:#1a1a1a;background:#fbfbf8}"##,
    r##"h2{font-size:.8em;font-weight:600;letter-spacing:.25em;"##,
    r##"text-transform:uppercase;margin:2.2em 0 .8em}"##,
    r##"h2:first-of-type{margin-top:.5em}"##,
    r##"h2:before{content:'— '}"##,
    r##"ul{list-style:none;margin:0;padding:0}"##,
    r##"li{display:flex;align-items:baseline;justify-content:space-between;"##,
    r##"gap:1em;padding:.45em 0;border-bottom:1px dotted #bbb}"##,
    r##"li i{color:#777}"##,
    r##"a.del{font-size:.7em;letter-spacing:.2em;text-transform:uppercase;"##,
    r##"color:#888;text-decoration:none;white-space:nowrap;cursor:pointer}"##,
    r##"a.del:hover{color:#1a1a1a}"##,
    r##"#drop{border:1px dashed #999;border-radius:3px;padding:2.2em 1em;"##,
    r##"text-align:center;color:#666;font-style:italic;cursor:pointer}"##,
    r##"#drop.over{border-color:#1a1a1a;color:#1a1a1a}"##,
    r##"progress{width:7em;height:.45em;accent-color:#1a1a1a}"##,
    r##"footer{margin-top:3em;text-align:center;font-style:italic;"##,
    r##"color:#777;font-size:.85em}"##,
    r##"</style></head><body>"##,
    r##"<h2>Books</h2><ul id=shelf><li><i>reading the card …</i></li></ul>"##,
    r##"<h2>Add books</h2>"##,
    r##"<div id=drop>drop EPUB files here — or click to choose</div>"##,
    r##"<input id=files type=file accept=.epub multiple hidden>"##,
    r##"<ul id=queue></ul>"##,
    r##"<footer>changes appear on the reader after it restarts ·"##,
    r##" press <b>done</b> there to finish</footer>"##,
    r##"<script>"##,
    r##"const shelf=document.getElementById('shelf'),"##,
    r##"queue=document.getElementById('queue'),"##,
    r##"drop=document.getElementById('drop'),"##,
    r##"input=document.getElementById('files');"##,
    r##"function row(label){const li=document.createElement('li');"##,
    r##"const span=document.createElement('span');span.textContent=label;"##,
    r##"li.appendChild(span);return li}"##,
    r##"async function load(){let text=null;"##,
    r##"for(let i=0;i<10&&text===null;i++){try{"##,
    r##"const r=await fetch('/list');if(r.ok)text=await r.text();}"##,
    r##"catch(e){}if(text===null)await new Promise(d=>setTimeout(d,800))}"##,
    r##"if(text===null){shelf.textContent='';"##,
    r##"shelf.appendChild(row('— the card did not answer —'));return}"##,
    r##"shelf.textContent='';"##,
    r##"const lines=text.split(String.fromCharCode(10)).filter(Boolean);"##,
    r##"if(!lines.length){shelf.appendChild(row('— nothing yet —'))}"##,
    r##"for(const line of lines){const[flag,open,label]=line.split('|');"##,
    r##"const li=row(label||open);"##,
    r##"const a=document.createElement('a');a.className='del';"##,
    r##"a.textContent='remove';a.onclick=async()=>{"##,
    r##"if(!confirm('Remove '+(label||open)+' from the card?'))return;"##,
    r##"const r=await fetch('/delete?name='+encodeURIComponent(open)+"##,
    r##"(flag==='R'?'&root=1':''),"##,
    r##"{method:'POST'});if(r.ok)li.remove()};li.appendChild(a);"##,
    r##"shelf.appendChild(li)}}"##,
    r##"function send(files){[...files].reduce((chain,f)=>chain.then(()=>new Promise(done=>{"##,
    r##"const li=row(f.name);const bar=document.createElement('progress');"##,
    r##"bar.max=1;bar.value=0;li.appendChild(bar);queue.appendChild(li);"##,
    r##"const xhr=new XMLHttpRequest();"##,
    r##"xhr.open('POST','/upload?name='+encodeURIComponent(f.name));"##,
    r##"xhr.upload.onprogress=e=>{if(e.lengthComputable)bar.value=e.loaded/e.total};"##,
    r##"xhr.onloadend=()=>{bar.remove();"##,
    r##"li.appendChild(document.createTextNode(xhr.status===200?' ✓':' — failed'));"##,
    r##"done()};xhr.send(f)})),Promise.resolve())}"##,
    r##"drop.onclick=()=>input.click();"##,
    r##"input.onchange=()=>send(input.files);"##,
    r##"drop.ondragover=e=>{e.preventDefault();drop.classList.add('over')};"##,
    r##"drop.ondragleave=()=>drop.classList.remove('over');"##,
    r##"drop.ondrop=e=>{e.preventDefault();drop.classList.remove('over');"##,
    r##"send(e.dataTransfer.files)};"##,
    r##"load();"##,
    r##"</script></body></html>"##,
);

/// Serves the shelf page, streams POSTed books to the display task,
/// lists the catalog snapshot, and deletes /BOOKS entries on request.
async fn upload_server(
    stack: Stack<'static>,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    request_buf: &'static mut [u8],
    catalog: &'static mut [u8],
    catalog_len: usize,
) -> ! {
    // Staging ping-pong buffers live in the loaned heap.
    let mut pool: heapless::Vec<&'static mut [u8], 2> = heapless::Vec::new();
    let _ = pool.push(alloc::vec![0u8; 4096].leak());
    let _ = pool.push(alloc::vec![0u8; 4096].leak());
    let mut session_started = false;

    loop {
        let mut socket = TcpSocket::new(stack, &mut *tcp_rx, &mut *tcp_tx);
        socket.set_timeout(Some(Duration::from_secs(30)));
        if socket.accept(80).await.is_err() {
            continue;
        }

        let mut filled = 0;
        let head = loop {
            if filled == request_buf.len() {
                break None;
            }
            let Ok(read) = socket.read(&mut request_buf[filled..]).await else {
                break None;
            };
            if read == 0 {
                break None;
            }
            filled += read;
            if let Some(head) = captive::parse_request_head(&request_buf[..filled]) {
                break Some((
                    head.method.len(),
                    head.path.len(),
                    head.content_length,
                    head.body_start,
                ));
            }
        };
        let Some((method_len, path_len, content_length, body_start)) = head else {
            socket.close();
            continue;
        };
        // Reborrow the pieces by index so the buffer stays usable for the
        // body bytes that arrived with the headers.
        let path_at = method_len + 1;
        let is_upload_post = request_buf
            .get(..method_len)
            .map(|m| m == b"POST")
            .unwrap_or(false)
            && request_buf
                .get(path_at..path_at + path_len)
                .map(|p| p.starts_with(b"/upload"))
                .unwrap_or(false);

        let path = request_buf.get(path_at..path_at + path_len).unwrap_or(b"/");
        let is_list = path.starts_with(b"/list");
        let is_delete = request_buf
            .get(..method_len)
            .map(|m| m == b"POST")
            .unwrap_or(false)
            && path.starts_with(b"/delete");

        if is_list {
            let listing =
                core::str::from_utf8(&catalog[..catalog_len.min(catalog.len())]).unwrap_or("");
            let _ = write_http_response(&mut socket, "200 OK", listing).await;
        } else if is_delete {
            let path_bytes = request_buf.get(path_at..path_at + path_len);
            let name = path_bytes
                .and_then(raw_query_name)
                .and_then(valid_short_name);
            let in_books = path_bytes
                .map(|p| !window_contains(p, b"root=1"))
                .unwrap_or(true);
            let ok = match name {
                Some(name) => {
                    if !session_started {
                        STORAGE_COMMANDS.send(StorageCommand::ReceiveUpload).await;
                        session_started = true;
                    }
                    UPLOAD_BEGINS
                        .send(UploadBegin {
                            name,
                            delete: true,
                            in_books,
                        })
                        .await;
                    UPLOAD_RESULTS.receive().await
                }
                None => false,
            };
            let _ = write_http_response(
                &mut socket,
                if ok { "200 OK" } else { "404 Not Found" },
                if ok { "deleted" } else { "failed" },
            )
            .await;
        } else if is_upload_post {
            let client_name = request_buf
                .get(path_at..path_at + path_len)
                .and_then(raw_query_name)
                .unwrap_or(b"book");
            let name = sanitized_name(client_name);

            if !session_started {
                STORAGE_COMMANDS.send(StorageCommand::ReceiveUpload).await;
                session_started = true;
            }
            let leftover_range = body_start..filled;
            let ok = stream_book(
                &mut socket,
                request_buf,
                leftover_range,
                content_length,
                name,
                &mut pool,
            )
            .await;
            let _ = write_http_response(
                &mut socket,
                if ok {
                    "200 OK"
                } else {
                    "507 Insufficient Storage"
                },
                if ok { "stored" } else { "failed" },
            )
            .await;
        } else {
            let _ = write_http_response(&mut socket, "200 OK", UPLOAD_PAGE).await;
        }
        socket.close();
        let _ = with_timeout(Duration::from_secs(2), socket.flush()).await;
    }
}

/// Streams one book body to the display task; true when the card write
/// succeeded end to end.
async fn stream_book(
    socket: &mut TcpSocket<'_>,
    request_buf: &[u8],
    leftover: core::ops::Range<usize>,
    content_length: usize,
    name: crate::upload::UploadName,
    pool: &mut heapless::Vec<&'static mut [u8], 2>,
) -> bool {
    esp_println::println!("upload: '{}' {} bytes", name, content_length);
    crate::upload::UPLOAD_IN_FLIGHT.store(true, portable_atomic::Ordering::SeqCst);
    UPLOAD_BEGINS
        .send(UploadBegin {
            name,
            delete: false,
            in_books: true,
        })
        .await;

    let mut leftover = &request_buf[leftover];
    if leftover.len() > content_length {
        leftover = &leftover[..content_length];
    }
    let mut remaining = content_length;
    let mut failed = false;
    while remaining > 0 && !failed {
        let buffer = match pool.pop() {
            Some(buffer) => buffer,
            None => UPLOAD_RETURNS.receive().await,
        };
        let mut len = 0;
        if !leftover.is_empty() {
            let take = leftover.len().min(buffer.len());
            buffer[..take].copy_from_slice(&leftover[..take]);
            leftover = &leftover[take..];
            len = take;
        }
        while len < buffer.len() && len < remaining {
            let window = buffer.len().min(remaining);
            match socket.read(&mut buffer[len..window]).await {
                Ok(0) | Err(_) => {
                    failed = true;
                    break;
                }
                Ok(read) => len += read,
            }
        }
        remaining -= len.min(remaining);
        UPLOAD_CHUNKS
            .send(UploadChunk {
                buffer: Some(buffer),
                len,
                last: remaining == 0 && !failed,
                abort: failed,
            })
            .await;
    }
    if content_length == 0 {
        // Nothing will flow; tell the writer to finish an empty file.
        UPLOAD_CHUNKS
            .send(UploadChunk {
                buffer: None,
                len: 0,
                last: true,
                abort: true,
            })
            .await;
    }
    // Refill the pool for the next file.
    let result = UPLOAD_RESULTS.receive().await;
    crate::upload::UPLOAD_IN_FLIGHT.store(false, portable_atomic::Ordering::SeqCst);
    while pool.len() < 2 {
        match UPLOAD_RETURNS.try_receive() {
            Ok(buffer) => {
                let _ = pool.push(buffer);
            }
            Err(_) => break,
        }
    }
    result && !failed
}

// ------------------------------------------------------------------
// Onboarding portal
// ------------------------------------------------------------------

const PORTAL_PAGE: &str = concat!(
    "<!doctype html><html><head>",
    "<meta name=viewport content=\"width=device-width,initial-scale=1\">",
    "<title>XTEINK X4</title>",
    "<style>body{font-family:Georgia,serif;margin:2.5em auto;max-width:22em;",
    "padding:0 1em;color:#222}h1{font-size:1.25em;letter-spacing:.08em}",
    "label{display:block;margin:1em 0 .2em}",
    "input{width:100%;font-size:1.05em;padding:.5em;border:1px solid #999;",
    "border-radius:4px;box-sizing:border-box}",
    "button{margin-top:1.2em;font-size:1.05em;padding:.6em 1.6em;",
    "border:1px solid #222;background:#222;color:#fff;border-radius:4px}",
    "</style></head><body><h1>XTEINK&nbsp;X4</h1>",
    "<p>Connect this reader to your Wi-Fi network.</p>",
    "<form method=post action=/save>",
    "<label>Network name</label><input name=ssid maxlength=32 required>",
    "<label>Password</label><input name=pass type=password maxlength=64>",
    "<button>Save</button></form></body></html>",
);

const SAVED_PAGE: &str = concat!(
    "<!doctype html><html><head>",
    "<meta name=viewport content=\"width=device-width,initial-scale=1\">",
    "<title>XTEINK X4</title>",
    "<style>body{font-family:Georgia,serif;margin:2.5em auto;max-width:22em;",
    "padding:0 1em;color:#222}h1{font-size:1.25em;letter-spacing:.08em}",
    "</style></head><body><h1>Saved</h1>",
    "<p>Back on the reader: press <i>done</i>, then run sync again to ",
    "connect to your network.</p></body></html>",
);

/// The onboarding hotspot: open AP, captive DHCP + DNS, and the
/// credential form on port 80. Never returns; the session ends with the
/// reset that `SyncCommand::Exit` triggers.
#[allow(clippy::too_many_arguments)]
async fn run_portal(
    spawner: Spawner,
    inited: &'static EspWifiController<'static>,
    wifi: WIFI,
    seed: u64,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    http_a: &'static mut [u8],
    http_b: &'static mut [u8],
) -> ! {
    let (device, mut controller) = match new_with_mode(inited, wifi, WifiApDevice) {
        Ok(parts) => parts,
        Err(err) => {
            esp_println::println!("portal: ap mode failed: {:?}", err);
            send_event(SyncEvent::Failed(SyncError::RadioInit));
            park_until_exit().await;
        }
    };
    let config = Configuration::AccessPoint(AccessPointConfiguration {
        ssid: PORTAL_SSID.try_into().unwrap_or_default(),
        auth_method: AuthMethod::None,
        ..Default::default()
    });
    if controller.set_configuration(&config).is_err() || controller.start_async().await.is_err() {
        esp_println::println!("portal: ap start failed");
        send_event(SyncEvent::Failed(SyncError::RadioInit));
        park_until_exit().await;
    }

    let portal = Ipv4Address::new(PORTAL_IP[0], PORTAL_IP[1], PORTAL_IP[2], PORTAL_IP[3]);
    let mut dns_servers = heapless::Vec::new();
    let _ = dns_servers.push(portal);
    let resources: &'static mut StackResources<6> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(StackResources::new()));
    let (stack, runner) = embassy_net::new(
        device,
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(portal, 24),
            gateway: Some(portal),
            dns_servers,
        }),
        resources,
        seed,
    );
    if spawner.spawn(ap_net_task(runner)).is_err() {
        send_event(SyncEvent::Failed(SyncError::RadioInit));
        park_until_exit().await;
    }

    esp_println::println!("portal: up at 192.168.4.1 as {}", PORTAL_SSID);
    send_event(SyncEvent::PortalUp);

    // Three servers share the task; Exit interrupts them with the reset.
    select(
        park_until_exit(),
        join3(
            dhcp_server(stack),
            dns_server(stack),
            credential_portal(stack, tcp_rx, tcp_tx, http_a, http_b),
        ),
    )
    .await;
    // park_until_exit resets and join3 never completes.
    unreachable!()
}

async fn dhcp_server(stack: Stack<'static>) -> ! {
    let rx_buf = alloc::vec![0u8; 1536].leak();
    let tx_buf = alloc::vec![0u8; 1536].leak();
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, rx_buf, &mut tx_meta, tx_buf);
    if socket.bind(67).is_err() {
        esp_println::println!("portal: dhcp bind failed");
        park_until_exit().await;
    }
    let mut server = captive::DhcpServer::new(PORTAL_IP);
    let mut packet = [0u8; 600];
    let mut reply = [0u8; captive::DHCP_REPLY_LEN];
    loop {
        let Ok((len, _meta)) = socket.recv_from(&mut packet).await else {
            continue;
        };
        if let Some(reply_len) = server.handle(&packet[..len], &mut reply) {
            let _ = socket
                .send_to(&reply[..reply_len], (IpAddress::v4(255, 255, 255, 255), 68))
                .await;
        }
    }
}

async fn dns_server(stack: Stack<'static>) -> ! {
    let rx_buf = alloc::vec![0u8; 1024].leak();
    let tx_buf = alloc::vec![0u8; 1024].leak();
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, rx_buf, &mut tx_meta, tx_buf);
    if socket.bind(53).is_err() {
        esp_println::println!("portal: dns bind failed");
        park_until_exit().await;
    }
    let mut query = [0u8; 300];
    let mut answer = [0u8; 360];
    loop {
        let Ok((len, meta)) = socket.recv_from(&mut query).await else {
            continue;
        };
        if let Some(answer_len) = captive::dns_answer(&query[..len], PORTAL_IP, &mut answer) {
            let _ = socket.send_to(&answer[..answer_len], meta).await;
        }
    }
}

async fn credential_portal(
    stack: Stack<'static>,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    request_buf: &'static mut [u8],
    _spare: &'static mut [u8],
) -> ! {
    loop {
        let mut socket = TcpSocket::new(stack, &mut *tcp_rx, &mut *tcp_tx);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if socket.accept(80).await.is_err() {
            continue;
        }

        let mut filled = 0;
        let saved = loop {
            if filled == request_buf.len() {
                break false;
            }
            let Ok(read) = socket.read(&mut request_buf[filled..]).await else {
                break false;
            };
            if read == 0 {
                break false;
            }
            filled += read;
            if let Some(request) = captive::parse_request(&request_buf[..filled]) {
                break handle_portal_request(&request).await;
            }
        };

        let body = if saved { SAVED_PAGE } else { PORTAL_PAGE };
        let _ = write_http_page(&mut socket, body).await;
        socket.close();
        let _ = with_timeout(Duration::from_secs(2), socket.flush()).await;
    }
}

/// Routes one parsed request; true means credentials were captured and
/// the success page should answer.
async fn handle_portal_request(request: &captive::HttpRequest<'_>) -> bool {
    if request.method != "POST" || request.path != "/save" {
        return false;
    }
    let mut ssid_buf = [0u8; 32];
    let mut pass_buf = [0u8; 64];
    let ssid = captive::form_value(request.body, "ssid", &mut ssid_buf).unwrap_or("");
    let password = captive::form_value(request.body, "pass", &mut pass_buf).unwrap_or("");
    let Some(credentials) = WifiCredentials::from_strs(ssid, password) else {
        return false;
    };
    esp_println::println!("portal: credentials captured for '{}'", credentials.ssid());
    STORAGE_COMMANDS
        .send(StorageCommand::StoreWifiCredentials(credentials))
        .await;
    send_event(SyncEvent::CredentialsSaved);
    true
}

fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Raw (undecoded) `name=` value from a path's query string.
fn raw_query_name(path: &[u8]) -> Option<&[u8]> {
    let query_at = path.iter().position(|byte| *byte == b'?')? + 1;
    path[query_at..]
        .split(|byte| *byte == b'&')
        .find_map(|pair| pair.strip_prefix(b"name="))
}

/// Accepts an existing 8.3 catalog open-name verbatim: short, printable
/// ASCII, no path separators. Deletion must not invent or mangle names.
fn valid_short_name(raw: &[u8]) -> Option<crate::upload::UploadName> {
    if raw.is_empty() || raw.len() > 12 {
        return None;
    }
    let mut name = crate::upload::UploadName::new();
    for byte in raw.iter().copied() {
        if !byte.is_ascii_graphic() || byte == b'/' || byte == b'\\' {
            return None;
        }
        let _ = name.push(byte as char);
    }
    Some(name)
}

async fn write_http_page(
    socket: &mut TcpSocket<'_>,
    body: &str,
) -> Result<(), embassy_net::tcp::Error> {
    write_http_response(socket, "200 OK", body).await
}

async fn write_http_response(
    socket: &mut TcpSocket<'_>,
    status: &str,
    body: &str,
) -> Result<(), embassy_net::tcp::Error> {
    let mut length = [0u8; 8];
    let mut at = length.len();
    let mut value = body.len();
    loop {
        at -= 1;
        length[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    write_all(socket, b"HTTP/1.1 ").await?;
    write_all(socket, status.as_bytes()).await?;
    write_all(
        socket,
        b"\r\ncache-control: no-store\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: ",
    )
    .await?;
    write_all(socket, &length[at..]).await?;
    write_all(socket, b"\r\nconnection: close\r\n\r\n").await?;
    write_all(socket, body.as_bytes()).await
}

async fn write_all(
    socket: &mut TcpSocket<'_>,
    mut data: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        if written == 0 {
            return Err(embassy_net::tcp::Error::ConnectionReset);
        }
        data = &data[written..];
    }
    Ok(())
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
        esp_println::println!(
            "wifi: heap used={} free={}",
            esp_alloc::HEAP.used(),
            esp_alloc::HEAP.free()
        );
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
