//! Single-core DPDK -> smoltcp TCP -> rustls -> WebSocket -> typed BTCUSDT market.
//! Uses the repository's runtime for delivery to the application task.
#![forbid(unsafe_code)]
mod ws_device;
mod ws_protocol;
mod ws_stats;

use clap::{Parser, ValueEnum};
use dpdk::Port;
use metrics::{now, Clock};
use rustls::{pki_types::ServerName, ClientConfig, ClientConnection, RootCertStore};
use serde::Serialize;
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    socket::tcp,
    time::{Duration, Instant},
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr},
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs,
    io::{self, Cursor, Read, Write},
    net::{Ipv4Addr, ToSocketAddrs},
    rc::Rc,
    sync::Arc,
    time::SystemTime,
};
use ws_device::{DpdkDevice, Stamp, Trace};
use ws_protocol::{Codec, Control, Delivery, LayerStamp, Market, Output};

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
enum Stream {
    BookTicker,
    AggTrade,
}
impl Stream {
    fn path(self) -> &'static str {
        match self {
            Self::BookTicker => "/public/ws/btcusdt@bookTicker",
            Self::AggTrade => "/market/ws/btcusdt@aggTrade",
        }
    }
}
#[derive(Clone, Debug, Parser, Serialize)]
struct Options {
    #[arg(long)]
    bdf: String,
    #[arg(long)]
    src_ip: Ipv4Addr,
    #[arg(long, default_value = "10.202.0.1")]
    gateway: Ipv4Addr,
    #[arg(long, default_value_t = 20)]
    prefix: u8,
    #[arg(long, default_value_t = 2)]
    core: usize,
    #[arg(long, default_value_t = 3)]
    stats_core: u32,
    #[arg(long, value_enum, default_value = "book-ticker")]
    stream: Stream,
    #[arg(long, default_value_t = 180)]
    duration_sec: u64,
    #[arg(long, default_value_t = 5)]
    warmup_sec: u64,
    #[arg(long, default_value_t = 20)]
    connect_timeout_sec: u64,
    #[arg(long, default_value_t = 50000)]
    local_port: u16,
    #[arg(long, default_value = "fstream.binance.com")]
    host: String,
    /// Override remote IPv4 for reproducible routing; TLS still verifies the configured hostname.
    #[arg(long)]
    remote_ip: Option<Ipv4Addr>,
    #[arg(long, default_value_t = 443)]
    remote_port: u16,
    #[arg(long, default_value = "results/binance-ws.json")]
    output: String,
    #[arg(long)]
    csv: Option<String>,
}

const TCP_BUFFER: usize = 262_144;
const TLS_RECORD_MAX: usize = 18_432 + 5;
const PENDING: usize = 256;
struct FixedWriter<'a>(&'a mut Vec<u8>);
impl Write for FixedWriter<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.0.len() + b.len() > self.0.capacity() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "TLS output buffer full",
            ));
        }
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Client {
    options: Options,
    remote_ip: Ipv4Addr,
    clock: Clock,
    begin: u64,
    iface: Interface,
    device: DpdkDevice,
    sockets: SocketSet<'static>,
    tcp: SocketHandle,
    tls: ClientConnection,
    record: Vec<u8>,
    record_stamp: Stamp,
    record_tcp_ready: u64,
    tcp_scratch: Box<[u8; 16_384]>,
    plain_scratch: Box<[u8; 65_536]>,
    tls_output: Vec<u8>,
    tls_output_start: usize,
    header: Vec<u8>,
    accept: String,
    codec: Codec,
    controls: VecDeque<Control>,
    pending: VecDeque<Delivery>,
    recorder: Option<ws_stats::Recorder>,
    established: Option<u64>,
    done: bool,
    error: Option<String>,
    messages_seen: u64,
    samples: u64,
    last_market: Market,
    peak_pending: usize,
    next_maintenance: u64,
    next_ping: u64,
    tls_version: String,
    cipher_suite: String,
    rx_hardware_timestamp_supported: bool,
}
impl Client {
    fn open(options: Options) -> Result<Self, String> {
        if options.stats_core as usize == options.core
            || options.prefix > 32
            || options.duration_sec == 0
        {
            return Err("Invalid CPU, IPv4 prefix, or duration configuration".into());
        }
        let remote_ip = if let Some(ip) = options.remote_ip {
            ip
        } else {
            (options.host.as_str(), options.remote_port)
                .to_socket_addrs()
                .map_err(|e| e.to_string())?
                .find_map(|a| {
                    if let std::net::SocketAddr::V4(v) = a {
                        Some(*v.ip())
                    } else {
                        None
                    }
                })
                .ok_or("No IPv4 address resolved")?
        };
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut tls = ClientConnection::new(
            Arc::new(config),
            ServerName::try_from(options.host.clone()).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        tls.set_buffer_limit(Some(TCP_BUFFER));
        let (request, accept) = ws_protocol::upgrade_request(&options.host, options.stream.path())?;
        tls.writer()
            .write_all(request.as_bytes())
            .map_err(|e| e.to_string())?;
        let port = Port::open(&options.bdf, options.core)?;
        let clock = Clock { hz: port.tsc_hz() };
        let rx_hardware_timestamp_supported = port.rx_timestamp_supported();
        let recorder = ws_stats::Recorder::new(clock, options.stats_core, options.csv.clone())?;
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(port.mac)));
        let trace = Trace::new(
            remote_ip.octets(),
            options.src_ip.octets(),
            options.remote_port,
            options.local_port,
        );
        let mut device = DpdkDevice::new(port, trace);
        let mut iface = Interface::new(config, &mut device, Instant::from_micros(0));
        iface.update_ip_addrs(|ips| {
            ips.push(IpCidr::new(IpAddress::Ipv4(options.src_ip), options.prefix))
                .unwrap()
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(options.gateway)
            .map_err(|e| e.to_string())?;
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
            tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
        );
        socket.set_nagle_enabled(false);
        socket.set_ack_delay(Some(Duration::from_millis(0)));
        socket.set_timeout(Some(Duration::from_secs(30)));
        socket.set_keep_alive(Some(Duration::from_secs(10)));
        socket
            .connect(
                iface.context(),
                (IpAddress::Ipv4(remote_ip), options.remote_port),
                (IpAddress::Ipv4(options.src_ip), options.local_port),
            )
            .map_err(|e| e.to_string())?;
        let mut sockets = SocketSet::new(vec![]);
        let tcp = sockets.add(socket);
        let begin = now();
        eprintln!(
            "DPDK TCP: {}:{} -> {}:{}; core={}; hardware_rx_timestamp={}",
            options.src_ip,
            options.local_port,
            remote_ip,
            options.remote_port,
            options.core,
            rx_hardware_timestamp_supported
        );
        Ok(Self {
            codec: Codec::new(matches!(options.stream, Stream::AggTrade)),
            options,
            remote_ip,
            clock,
            begin,
            iface,
            device,
            sockets,
            tcp,
            tls,
            record: Vec::with_capacity(TLS_RECORD_MAX),
            record_stamp: Stamp::default(),
            record_tcp_ready: 0,
            tcp_scratch: Box::new([0; 16_384]),
            plain_scratch: Box::new([0; 65_536]),
            tls_output: Vec::with_capacity(TCP_BUFFER),
            tls_output_start: 0,
            header: Vec::with_capacity(16_384),
            accept,
            controls: VecDeque::with_capacity(64),
            pending: VecDeque::with_capacity(PENDING),
            recorder: Some(recorder),
            established: None,
            done: false,
            error: None,
            messages_seen: 0,
            samples: 0,
            last_market: Market::default(),
            peak_pending: 0,
            next_maintenance: begin,
            next_ping: u64::MAX,
            tls_version: String::new(),
            cipher_suite: String::new(),
            rx_hardware_timestamp_supported,
        })
    }
    fn timestamp(&self, t: u64) -> Instant {
        Instant::from_micros((self.clock.ns(t - self.begin) / 1000) as i64)
    }
    fn deadline(&self) -> u64 {
        self.established.map_or(
            self.begin + self.clock.us(self.options.connect_timeout_sec * 1_000_000),
            |t| {
                t + self
                    .clock
                    .us((self.options.warmup_sec + self.options.duration_sec) * 1_000_000)
            },
        )
    }
    fn flush_tls(&mut self) -> Result<(), String> {
        if !self.sockets.get::<tcp::Socket>(self.tcp).may_send() {
            return Ok(());
        }
        if self.tls_output_start > 0 && self.tls_output_start == self.tls_output.len() {
            self.tls_output.clear();
            self.tls_output_start = 0;
        }
        while self.tls.wants_write()
            && self.tls_output.capacity() - self.tls_output.len() > TLS_RECORD_MAX
        {
            match self.tls.write_tls(&mut FixedWriter(&mut self.tls_output)) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.to_string()),
            }
        }
        if self.tls_output_start < self.tls_output.len()
            && self.sockets.get::<tcp::Socket>(self.tcp).can_send()
        {
            let sent = self
                .sockets
                .get_mut::<tcp::Socket>(self.tcp)
                .send_slice(&self.tls_output[self.tls_output_start..])
                .map_err(|e| e.to_string())?;
            self.tls_output_start += sent;
        }
        Ok(())
    }
    fn plain(&mut self, len: usize, layer: LayerStamp) -> Result<(), String> {
        if self.established.is_none() {
            if self.header.len() + len > self.header.capacity() {
                return Err("HTTP upgrade header exceeded 16 KiB".into());
            }
            self.header.extend_from_slice(&self.plain_scratch[..len]);
            let Some(end) = self
                .header
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| p + 4)
            else {
                return Ok(());
            };
            ws_protocol::validate_upgrade(&self.header[..end], &self.accept)?;
            self.tls_version = format!("{:?}", self.tls.protocol_version());
            self.cipher_suite = format!("{:?}", self.tls.negotiated_cipher_suite());
            self.established = Some(now());
            self.next_ping = now() + self.clock.us(20_000_000);
            eprintln!(
                "TLS and WebSocket established: {:?}; warming up {} seconds",
                self.tls.protocol_version(),
                self.options.warmup_sec
            );
            if end < self.header.len() {
                self.codec.push(&self.header[end..], layer)?;
            }
            self.header.clear();
        } else {
            self.codec.push(&self.plain_scratch[..len], layer)?;
        }
        while let Some(frame) = self.codec.next()? {
            match frame {
                Output::Market(delivery) => {
                    self.messages_seen += 1;
                    if self.pending.len() == PENDING {
                        return Err("Application delivery queue full".into());
                    }
                    self.pending.push_back(delivery);
                    self.peak_pending = self.peak_pending.max(self.pending.len());
                }
                Output::Pong(control) => {
                    if self.controls.len() == 64 {
                        return Err("WebSocket control queue full".into());
                    }
                    self.controls.push_back(control);
                }
                Output::Close => return Err("Server closed WebSocket during benchmark".into()),
                Output::Ignored => {}
            }
        }
        Ok(())
    }
    fn complete_record(&mut self) -> Result<(), String> {
        let tcp_ready = now().max(self.record_tcp_ready);
        let mut cursor = Cursor::new(self.record.as_slice());
        while (cursor.position() as usize) < self.record.len() {
            if self.tls.read_tls(&mut cursor).map_err(|e| e.to_string())? == 0 {
                return Err("TLS record input made no progress".into());
            }
            self.tls.process_new_packets().map_err(|e| e.to_string())?;
        }
        let tls_ready = now();
        let layer = LayerStamp {
            rx: self.record_stamp,
            tcp_ready,
            tls_ready,
        };
        loop {
            match self.tls.reader().read(self.plain_scratch.as_mut_slice()) {
                Ok(0) => break,
                Ok(n) => self.plain(n, layer)?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.to_string()),
            }
        }
        self.record.clear();
        self.record_stamp = Stamp::default();
        self.record_tcp_ready = 0;
        Ok(())
    }
    fn receive_tcp(&mut self) -> Result<(), String> {
        while self.sockets.get::<tcp::Socket>(self.tcp).can_recv() {
            let n = self
                .sockets
                .get_mut::<tcp::Socket>(self.tcp)
                .recv_slice(self.tcp_scratch.as_mut_slice())
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            let tcp_ready = now();
            let mut offset = 0;
            while offset < n {
                let target = if self.record.len() < 5 {
                    5
                } else {
                    let body = u16::from_be_bytes([self.record[3], self.record[4]]) as usize;
                    if body + 5 > TLS_RECORD_MAX {
                        return Err("TLS ciphertext record exceeds RFC bound".into());
                    }
                    body + 5
                };
                let take = (target - self.record.len()).min(n - offset);
                let stamp = self.device.trace.take(take)?;
                self.record_stamp.merge(stamp);
                self.record_tcp_ready = self.record_tcp_ready.max(tcp_ready);
                self.record
                    .extend_from_slice(&self.tcp_scratch[offset..offset + take]);
                offset += take;
                if self.record.len() >= 5 {
                    let target = 5 + u16::from_be_bytes([self.record[3], self.record[4]]) as usize;
                    if self.record.len() == target {
                        self.complete_record()?;
                    }
                }
            }
        }
        Ok(())
    }
    fn step(&mut self) -> Result<(), String> {
        let t = now();
        if t >= self.deadline() {
            if self.established.is_none() {
                return Err(format!(
                    "TCP/TLS/WS connection timed out; TCP state={:?}; NIC={:?}",
                    self.sockets.get::<tcp::Socket>(self.tcp).state(),
                    self.device.port.stats()
                ));
            }
            self.done = true;
            return Ok(());
        }
        self.device.refill();
        if self.device.has_pending() {
            self.iface
                .poll_ingress_single(self.timestamp(t), &mut self.device, &mut self.sockets);
            if let Some(e) = &self.device.trace.error {
                return Err(e.clone());
            }
            self.receive_tcp()?;
        }
        while let Some(c) = self.controls.pop_front() {
            self.tls
                .writer()
                .write_all(&c.bytes[..c.len])
                .map_err(|e| e.to_string())?;
        }
        if t >= self.next_ping {
            let c = ws_protocol::control(9, b"latency")?;
            self.tls
                .writer()
                .write_all(&c.bytes[..c.len])
                .map_err(|e| e.to_string())?;
            self.next_ping = t + self.clock.us(20_000_000);
        }
        self.flush_tls()?;
        self.iface
            .poll_egress(self.timestamp(t), &mut self.device, &mut self.sockets);
        if self.device.tx_failed.get() > 0 {
            return Err("DPDK TX submission failed".into());
        }
        if t >= self.next_maintenance {
            self.device.port.maintenance();
            self.next_maintenance = t + self.clock.us(1000);
        }
        if !self.sockets.get::<tcp::Socket>(self.tcp).is_active() {
            return Err("TCP connection closed during benchmark".into());
        }
        Ok(())
    }
    fn accept(&mut self, d: Delivery, app: u64) -> Result<(), String> {
        self.last_market = d.market;
        let Some(start) = self.established else {
            return Ok(());
        };
        if app < start + self.clock.us(self.options.warmup_sec * 1_000_000)
            || app >= self.deadline()
        {
            return Ok(());
        }
        self.recorder
            .as_mut()
            .unwrap()
            .record(ws_stats::Sample::from_delivery(d, app)?)?;
        self.samples += 1;
        Ok(())
    }
    fn shutdown(&mut self) {
        if self.established.is_some() {
            if let Ok(c) = ws_protocol::control(8, &[3, 232]) {
                let _ = self.tls.writer().write_all(&c.bytes[..c.len]);
            }
            self.tls.send_close_notify();
            let _ = self.flush_tls();
        }
        self.sockets.get_mut::<tcp::Socket>(self.tcp).close();
        let end = now() + self.clock.us(50_000);
        while now() < end {
            self.device.refill();
            self.iface
                .poll(self.timestamp(now()), &mut self.device, &mut self.sockets);
            self.device.port.maintenance();
            if !self.sockets.get::<tcp::Socket>(self.tcp).is_active() {
                break;
            }
        }
        self.device.release_pending();
    }
    fn finish(mut self, start_wall: String) -> Result<(), String> {
        self.shutdown();
        let (latency, backpressure_batches) = self.recorder.take().unwrap().finish()?;
        let nic = self.device.port.stats();
        let tx_failed = self.device.tx_failed.get();
        let packets = self.device.packets;
        let bursts = self.device.bursts;
        let tcp_payload_packets = self.device.trace.payload_packets;
        let trace_error = self.device.trace.error.take();
        let (initial, final_count) = self.device.port.finish();
        let success = self.error.is_none()
            && trace_error.is_none()
            && initial == final_count
            && self.samples > 0
            && tx_failed == 0;
        let report = serde_json::json!({
            "success":success,"start_wall":start_wall,"options":self.options,"remote_ip":self.remote_ip,
            "tsc_hz":self.clock.hz,"units":"ns","messages_seen":self.messages_seen,"samples":self.samples,
            "json_payload_bytes":self.codec.json_bytes,
            "json_payload_bytes_scope":"All successfully parsed market messages, including warmup; complete JSON text including skipped fields, excluding WebSocket/TLS headers",
            "error":self.error,"trace_error":trace_error,"latency":latency,"last_market":self.last_market,
            "tls_version":self.tls_version,"cipher_suite":self.cipher_suite,
            "nic":nic,"tx_failed":tx_failed,"rx_packets":packets,"rx_bursts":bursts,"tcp_payload_packets":tcp_payload_packets,
            "application_peak_pending":self.peak_pending,
            "statistics":{"library":"hdrhistogram 7.6.0","significant_figures":4,"collection":"background SPSC batches", "backpressure_batches":backpressure_batches},
            "mempool":{"initial":initial,"final":final_count,"leak_free":initial==final_count},
            "timing":{"rx_start":"C shim after rte_eth_rx_burst returns; shared per burst", "end":"typed JSON parsed / application task resumed; separately reported",
                "hardware_rx_timestamp_supported":self.rx_hardware_timestamp_supported,"dma_completion_exactly_measured":false,
                "completion_bounds":"For the last required packet: CPU-observable CQ readiness is bounded by start of previous empty RX poll and non-empty RX return. Lower latency bound = rx_last_to_*; upper bound = completion_to_*_upper_bound. This brackets observable descriptor readiness, not exact DMA completion.",
                "provenance":"Observed TCP sequence spans -> complete TLS record -> WebSocket frame/message; first arrivals retained across retransmission; all coalesced frames share contributing TLS record provenance; fragmented messages merge their contributing records.",
                "first_packet":"rx_first_to_* additionally includes waiting for remaining TCP/TLS/WebSocket fragments", "data_path":"DPDK ENA -> smoltcp -> rustls -> bounded WebSocket codec -> borrowed serde JSON -> existing rt::Slot and executor", "kernel_tcp_socket_used":false }
        });
        fs::write(
            &self.options.output,
            serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
        if !success {
            return Err("Benchmark failed; diagnostic report was saved".into());
        }
        Ok(())
    }
}

async fn application(mut handle: rt::Handle, io: Rc<RefCell<Client>>, slot: Rc<rt::Slot<()>>) {
    loop {
        let deadline = io.borrow().deadline();
        let received = slot.recv(&mut handle, deadline).await;
        if received.is_err() {
            let mut state = io.borrow_mut();
            if now() >= state.deadline() {
                state.done = state.established.is_some();
                if !state.done {
                    state.error = Some("Connection/upgrade deadline expired".into());
                }
                break;
            }
        }
        loop {
            let Some(d) = io.borrow_mut().pending.pop_front() else {
                break;
            };
            let app = now(); // Application owns the typed market immediately after queue transfer.
            let accepted = io.borrow_mut().accept(d, app);
            if let Err(e) = accepted {
                io.borrow_mut().error = Some(e);
                break;
            }
        }
        let state = io.borrow();
        if state.done || state.error.is_some() {
            break;
        }
    }
}
fn run() -> Result<(), String> {
    let options = Options::parse();
    if let Some(parent) = std::path::Path::new(&options.output).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let start_wall = format!("{:?}", SystemTime::now());
    let client = Rc::new(RefCell::new(Client::open(options)?));
    let slot = Rc::new(rt::Slot::default());
    let mut runtime = rt::Runtime::new(1, now);
    let task_io = client.clone();
    let task_slot = slot.clone();
    runtime.spawn(move |handle| application(handle, task_io, task_slot));
    runtime.run(|| {
        let mut state = client.borrow_mut();
        if state.error.is_none() && !state.done {
            if let Err(e) = state.step() {
                state.error = Some(e);
            }
        }
        if !state.pending.is_empty() || state.error.is_some() || state.done {
            let _ = slot.deliver(());
        }
    });
    drop(runtime);
    drop(slot);
    Rc::try_unwrap(client)
        .map_err(|_| "Application still owns client")?
        .into_inner()
        .finish(start_wall)
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
