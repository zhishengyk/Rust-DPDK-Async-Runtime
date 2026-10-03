//! Bounded streaming WebSocket codec. Fragmentation and interleaved controls are supported.
//! No compression is negotiated. JSON borrows strings from the existing frame buffer.
use crate::ws_device::Stamp;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::VecDeque;

pub const MAX_MESSAGE: usize = 65_536;
const BUFFER_CAP: usize = MAX_MESSAGE * 2 + 32;

#[derive(Clone, Copy, Debug, Default)]
pub struct LayerStamp {
    pub rx: Stamp,
    pub tcp_ready: u64,
    pub tls_ready: u64,
}
impl LayerStamp {
    pub fn merge(&mut self, other: Self) {
        self.rx.merge(other.rx);
        self.tcp_ready = self.tcp_ready.max(other.tcp_ready);
        self.tls_ready = self.tls_ready.max(other.tls_ready);
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Market {
    pub event_ms: u64,
    pub transaction_ms: u64,
    pub id: u64,
    pub bid_or_price: f64,
    pub bid_qty_or_qty: f64,
    pub ask: f64,
    pub ask_qty: f64,
    pub is_trade: bool,
}
// Use separate schemas: bookTicker.a is a string, aggTrade.a is a numeric ID.
#[derive(Deserialize)]
struct Book<'a> {
    #[serde(borrow)]
    e: &'a str,
    #[serde(borrow)]
    s: &'a str,
    #[serde(rename = "E")]
    event_ms: u64,
    #[serde(rename = "T")]
    transaction_ms: u64,
    u: u64,
    #[serde(borrow)]
    b: &'a str,
    #[serde(rename = "B", borrow)]
    bq: &'a str,
    #[serde(borrow)]
    a: &'a str,
    #[serde(rename = "A", borrow)]
    aq: &'a str,
}
#[derive(Deserialize)]
struct Trade<'a> {
    #[serde(borrow)]
    e: &'a str,
    #[serde(borrow)]
    s: &'a str,
    #[serde(rename = "E")]
    event_ms: u64,
    #[serde(rename = "T")]
    transaction_ms: u64,
    a: u64,
    #[serde(borrow)]
    p: &'a str,
    #[serde(borrow)]
    q: &'a str,
}
fn number(s: &str) -> Result<f64, String> {
    let n: f64 = s.parse().map_err(|_| "Invalid market numeric field")?;
    if !n.is_finite() {
        return Err("Non-finite market numeric field".into());
    }
    Ok(n)
}
fn market(bytes: &[u8], trade: bool) -> Result<Market, String> {
    if trade {
        let t: Trade<'_> = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if t.e != "aggTrade" || t.s != "BTCUSDT" {
            return Err("Unexpected market stream or symbol".into());
        }
        Ok(Market {
            event_ms: t.event_ms,
            transaction_ms: t.transaction_ms,
            id: t.a,
            bid_or_price: number(t.p)?,
            bid_qty_or_qty: number(t.q)?,
            is_trade: true,
            ..Market::default()
        })
    } else {
        let b: Book<'_> = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if b.e != "bookTicker" || b.s != "BTCUSDT" {
            return Err("Unexpected market stream or symbol".into());
        }
        Ok(Market {
            event_ms: b.event_ms,
            transaction_ms: b.transaction_ms,
            id: b.u,
            bid_or_price: number(b.b)?,
            bid_qty_or_qty: number(b.bq)?,
            ask: number(b.a)?,
            ask_qty: number(b.aq)?,
            is_trade: false,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Delivery {
    pub market: Market,
    pub layer: LayerStamp,
    pub ws_ready: u64,
    pub json_ready: u64,
}
#[derive(Clone, Copy)]
pub struct Control {
    pub bytes: [u8; 131],
    pub len: usize,
}
pub enum Output {
    Market(Delivery),
    Pong(Control),
    Ignored,
    Close,
}
pub fn control(opcode: u8, payload: &[u8]) -> Result<Control, String> {
    if payload.len() > 125 {
        return Err("Control frame larger than 125 bytes".into());
    }
    let mut c = Control {
        bytes: [0; 131],
        len: payload.len() + 6,
    };
    c.bytes[0] = 0x80 | opcode;
    c.bytes[1] = 0x80 | payload.len() as u8;
    getrandom::getrandom(&mut c.bytes[2..6]).map_err(|e| e.to_string())?;
    for (i, b) in payload.iter().enumerate() {
        c.bytes[6 + i] = b ^ c.bytes[2 + i % 4];
    }
    Ok(c)
}

pub fn upgrade_request(host: &str, path: &str) -> Result<(String, String), String> {
    if host.contains(['\r', '\n']) || path.contains(['\r', '\n']) {
        return Err("Invalid HTTP target".into());
    }
    let mut nonce = [0; 16];
    getrandom::getrandom(&mut nonce).map_err(|e| e.to_string())?;
    let key = STANDARD.encode(nonce);
    let accept = STANDARD.encode(Sha1::digest(format!(
        "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
    )));
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n");
    Ok((request, accept))
}
pub fn validate_upgrade(header: &[u8], accept: &str) -> Result<(), String> {
    let header = std::str::from_utf8(header).map_err(|_| "HTTP upgrade header is not UTF-8")?;
    let mut lines = header.split("\r\n");
    if !lines.next().unwrap_or("").starts_with("HTTP/1.1 101 ") {
        return Err(format!("WebSocket upgrade refused: {header}"));
    }
    let mut valid_accept = false;
    let mut valid_upgrade = false;
    let mut valid_connection = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("sec-websocket-accept") {
                valid_accept = value == accept;
            }
            if name.eq_ignore_ascii_case("upgrade") {
                valid_upgrade = value.eq_ignore_ascii_case("websocket");
            }
            if name.eq_ignore_ascii_case("connection") {
                valid_connection = value
                    .split(',')
                    .any(|v| v.trim().eq_ignore_ascii_case("upgrade"));
            }
            if name.eq_ignore_ascii_case("sec-websocket-extensions") && !value.is_empty() {
                return Err("Server negotiated an unrequested WebSocket extension".into());
            }
        }
    }
    if !(valid_accept && valid_upgrade && valid_connection) {
        return Err("Invalid HTTP WebSocket upgrade response".into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Block {
    end: u64,
    layer: LayerStamp,
}
#[derive(Default, Serialize)]
pub struct JsonBytes {
    pub messages: u64,
    pub total_bytes: u64,
    pub min_bytes: usize,
    pub max_bytes: usize,
}
impl JsonBytes {
    fn record(&mut self, bytes: usize) {
        self.min_bytes = if self.messages == 0 {
            bytes
        } else {
            self.min_bytes.min(bytes)
        };
        self.max_bytes = self.max_bytes.max(bytes);
        self.messages += 1;
        self.total_bytes += bytes as u64;
    }
}
pub struct Codec {
    bytes: Vec<u8>,
    start: usize,
    absolute_start: u64,
    blocks: VecDeque<Block>,
    fragment: Vec<u8>,
    fragment_layer: LayerStamp,
    fragmented: bool,
    trade: bool,
    pub json_bytes: JsonBytes,
}
impl Codec {
    pub fn new(trade: bool) -> Self {
        Self {
            bytes: Vec::with_capacity(BUFFER_CAP),
            start: 0,
            absolute_start: 0,
            blocks: VecDeque::with_capacity(512),
            fragment: Vec::with_capacity(MAX_MESSAGE),
            fragment_layer: LayerStamp::default(),
            fragmented: false,
            trade,
            json_bytes: JsonBytes::default(),
        }
    }
    pub fn push(&mut self, bytes: &[u8], layer: LayerStamp) -> Result<(), String> {
        if self.bytes.len() + bytes.len() > BUFFER_CAP && self.start > 0 {
            let remaining = self.bytes.len() - self.start;
            self.bytes.copy_within(self.start.., 0);
            self.bytes.truncate(remaining);
            self.start = 0;
        }
        if self.bytes.len() + bytes.len() > BUFFER_CAP || self.blocks.len() == 512 {
            return Err("WebSocket buffer or timestamp provenance capacity exceeded".into());
        }
        self.bytes.extend_from_slice(bytes);
        self.blocks.push_back(Block {
            end: self.absolute_start + (self.bytes.len() - self.start) as u64,
            layer,
        });
        Ok(())
    }
    fn consume(&mut self, len: usize) -> LayerStamp {
        let end = self.absolute_start + len as u64;
        let mut layer = LayerStamp::default();
        for block in &self.blocks {
            layer.merge(block.layer);
            if block.end >= end {
                break;
            }
        }
        self.start += len;
        self.absolute_start = end;
        while self.blocks.front().is_some_and(|b| b.end <= end) {
            self.blocks.pop_front();
        }
        if self.start == self.bytes.len() {
            self.bytes.clear();
            self.start = 0;
        }
        layer
    }
    pub fn next(&mut self) -> Result<Option<Output>, String> {
        let b = &self.bytes[self.start..];
        if b.len() < 2 {
            return Ok(None);
        }
        if b[0] & 0x70 != 0 || b[1] & 0x80 != 0 {
            return Err("Compressed, reserved, or masked server frame".into());
        }
        let fin = b[0] & 0x80 != 0;
        let opcode = b[0] & 15;
        let (header, len) = match b[1] & 127 {
            126 => {
                if b.len() < 4 {
                    return Ok(None);
                }
                let len = u16::from_be_bytes(b[2..4].try_into().unwrap()) as usize;
                if len < 126 {
                    return Err("Non-canonical WebSocket length".into());
                }
                (4, len)
            }
            127 => {
                if b.len() < 10 {
                    return Ok(None);
                }
                let n = u64::from_be_bytes(b[2..10].try_into().unwrap());
                if n < 65536 || n > MAX_MESSAGE as u64 {
                    return Err("Invalid or oversized WebSocket length".into());
                }
                (10, n as usize)
            }
            n => (2, n as usize),
        };
        if len > MAX_MESSAGE || opcode >= 8 && (!fin || len > 125) {
            return Err("Oversized message or invalid control frame".into());
        }
        if b.len() < header + len {
            return Ok(None);
        }
        let ws_ready = metrics::now();
        let payload = &b[header..header + len];
        // Preserve bytes while JSON borrows them; move only the fixed-size Market afterwards.
        let parsed = match opcode {
            1 if !self.fragmented && fin => Some(market(payload, self.trade)?),
            1 if !self.fragmented => {
                self.fragment.clear();
                self.fragment.extend_from_slice(payload);
                self.fragmented = true;
                None
            }
            0 if self.fragmented => {
                if self.fragment.len() + len > MAX_MESSAGE {
                    return Err("Fragmented message exceeds capacity".into());
                }
                self.fragment.extend_from_slice(payload);
                if fin {
                    self.fragmented = false;
                    Some(market(&self.fragment, self.trade)?)
                } else {
                    None
                }
            }
            8 => {
                if len == 1 {
                    return Err("Invalid close frame payload".into());
                }
                self.consume(header + len);
                return Ok(Some(Output::Close));
            }
            9 => {
                let pong = control(10, payload)?;
                self.consume(header + len);
                return Ok(Some(Output::Pong(pong)));
            }
            10 => {
                self.consume(header + len);
                return Ok(Some(Output::Ignored));
            }
            _ => return Err("Unexpected WebSocket data opcode or fragmentation sequence".into()),
        };
        let json_ready = metrics::now();
        if parsed.is_some() {
            // Count the full reassembled JSON once, after the parse timestamp.
            // This includes fields skipped by serde, but excludes WS/TLS headers.
            self.json_bytes.record(if opcode == 0 {
                self.fragment.len()
            } else {
                len
            });
        }
        let layer = self.consume(header + len);
        if opcode == 1 && !fin {
            self.fragment_layer = layer;
        }
        let layer = if opcode == 0 {
            self.fragment_layer.merge(layer);
            self.fragment_layer
        } else {
            layer
        };
        match parsed {
            Some(market) => Ok(Some(Output::Market(Delivery {
                market,
                layer,
                ws_ready,
                json_ready,
            }))),
            None => Ok(Some(Output::Ignored)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const JSON: &[u8] = br#"{"e":"bookTicker","s":"BTCUSDT","E":100,"T":99,"u":1,"b":"123.4","B":"2","a":"123.5","A":"3"}"#;
    fn layer(t: u64) -> LayerStamp {
        LayerStamp {
            rx: Stamp {
                first_rx: t,
                last_rx: t,
                ready_lower_bound: t - 1,
            },
            tcp_ready: t + 1,
            tls_ready: t + 2,
        }
    }
    fn frame(op: u8, fin: bool, p: &[u8]) -> Vec<u8> {
        let mut b = vec![op | if fin { 128 } else { 0 }, p.len() as u8];
        b.extend_from_slice(p);
        b
    }
    #[test]
    fn split_frame_and_coalesced_messages_preserve_byte_provenance() {
        let mut c = Codec::new(false);
        let f = frame(1, true, JSON);
        c.push(&f[..15], layer(10)).unwrap();
        assert!(c.next().unwrap().is_none());
        c.push(&f[15..], layer(20)).unwrap();
        let Output::Market(d) = c.next().unwrap().unwrap() else {
            panic!()
        };
        assert_eq!((d.layer.rx.first_rx, d.layer.rx.last_rx), (10, 20));
        assert_eq!(d.market.ask, 123.5);
        assert_eq!(c.json_bytes.messages, 1);
        assert_eq!(c.json_bytes.total_bytes, JSON.len() as u64);
        let both = [f.as_slice(), f.as_slice()].concat();
        c.push(&both, layer(30)).unwrap();
        assert!(matches!(c.next().unwrap(), Some(Output::Market(_))));
        assert!(matches!(c.next().unwrap(), Some(Output::Market(_))));
        assert!(c.next().unwrap().is_none());
        assert_eq!(c.json_bytes.messages, 3);
        assert_eq!(c.json_bytes.total_bytes, (JSON.len() * 3) as u64);
    }
    #[test]
    fn fragmented_text_accepts_interleaved_ping_and_masks_pong() {
        let mut c = Codec::new(false);
        c.push(&frame(1, false, &JSON[..20]), layer(10)).unwrap();
        c.next().unwrap();
        c.push(&frame(9, true, b"test"), layer(15)).unwrap();
        let Output::Pong(p) = c.next().unwrap().unwrap() else {
            panic!()
        };
        assert_eq!(p.bytes[0], 138);
        assert_eq!(p.bytes[1], 132);
        for i in 0..4 {
            assert_eq!(p.bytes[6 + i] ^ p.bytes[2 + i % 4], b"test"[i]);
        }
        c.push(&frame(0, true, &JSON[20..]), layer(20)).unwrap();
        let Output::Market(d) = c.next().unwrap().unwrap() else {
            panic!()
        };
        assert_eq!((d.layer.rx.first_rx, d.layer.rx.last_rx), (10, 20));
        assert_eq!(c.json_bytes.messages, 1);
        assert_eq!(c.json_bytes.min_bytes, JSON.len());
        assert_eq!(c.json_bytes.max_bytes, JSON.len());
    }
    #[test]
    fn reject_unrequested_extensions_and_invalid_handshake_or_frame() {
        assert!(validate_upgrade(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Accept: expected\r\n\r\n","expected").is_ok());
        assert!(validate_upgrade(b"HTTP/1.1 200 OK\r\n\r\n", "expected").is_err());
        let mut c = Codec::new(false);
        c.push(&[0x81, 0xff], layer(10)).unwrap();
        assert!(c.next().is_err());
    }
}
