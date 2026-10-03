//! DPDK-backed smoltcp device, with bounded TCP byte-to-RX timestamp provenance.
//! No kernel socket is used by the TCP/TLS/WebSocket data path.
use dpdk::{Mbuf, Port, BURST};
use smoltcp::phy::{self, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, Ipv4Packet, TcpPacket};
use std::{cell::Cell, collections::VecDeque};

const MAX_SPANS: usize = 4096;

#[derive(Clone, Copy, Debug, Default)]
pub struct Stamp {
    pub first_rx: u64,
    pub last_rx: u64,
    // Completion of the last required packet is no earlier than this empty-poll start.
    pub ready_lower_bound: u64,
}
impl Stamp {
    pub fn merge(&mut self, other: Self) {
        if other.first_rx == 0 {
            return;
        }
        if self.first_rx == 0 {
            *self = other;
            return;
        }
        self.first_rx = self.first_rx.min(other.first_rx);
        self.last_rx = self.last_rx.max(other.last_rx);
        self.ready_lower_bound = self.ready_lower_bound.max(other.ready_lower_bound);
    }
}

#[derive(Clone, Copy)]
struct Span {
    start: u64,
    end: u64,
    stamp: Stamp,
}

pub struct Trace {
    pub peer_ip: [u8; 4],
    pub local_ip: [u8; 4],
    pub peer_port: u16,
    pub local_port: u16,
    base_seq: Option<u32>,
    consumed: u64,
    spans: Vec<Span>,
    pub payload_packets: u64,
    pub duplicate_bytes_packets: u64,
    pub error: Option<String>,
}
impl Trace {
    pub fn new(peer_ip: [u8; 4], local_ip: [u8; 4], peer_port: u16, local_port: u16) -> Self {
        Self {
            peer_ip,
            local_ip,
            peer_port,
            local_port,
            base_seq: None,
            consumed: 0,
            spans: Vec::with_capacity(MAX_SPANS),
            payload_packets: 0,
            duplicate_bytes_packets: 0,
            error: None,
        }
    }
    fn add(&mut self, seq: u32, len: usize, stamp: Stamp) {
        let Some(base) = self.base_seq else {
            self.error = Some("TCP payload arrived without observed peer SYN".into());
            return;
        };
        let relative = seq.wrapping_sub(base).wrapping_sub(self.consumed as u32) as i32 as i64;
        let start = self.consumed as i128 + relative as i128;
        let end = start + len as i128;
        if end <= self.consumed as i128 {
            self.duplicate_bytes_packets += 1;
            return;
        }
        if self.spans.len() == MAX_SPANS {
            self.error = Some("TCP timestamp provenance capacity exceeded".into());
            return;
        }
        self.spans.push(Span {
            start: start.max(self.consumed as i128) as u64,
            end: end as u64,
            stamp,
        });
        self.payload_packets += 1;
    }
    fn observe(&mut self, bytes: &[u8], stamp: Stamp) {
        if bytes.len() < 54 || bytes[12..14] != [0x08, 0x00] {
            return;
        }
        let ip = &bytes[14..];
        let ihl = (ip[0] as usize & 15) * 4;
        let total = u16::from_be_bytes([ip[2], ip[3]]) as usize;
        if ip[0] >> 4 != 4
            || ihl < 20
            || total > ip.len()
            || total < ihl + 20
            || ip[9] != 6
            || ip[6] & 0x3f != 0
            || ip[7] != 0
            || ip[12..16] != self.peer_ip
            || ip[16..20] != self.local_ip
        {
            return;
        }
        let tcp = &ip[ihl..total];
        if u16::from_be_bytes([tcp[0], tcp[1]]) != self.peer_port
            || u16::from_be_bytes([tcp[2], tcp[3]]) != self.local_port
        {
            return;
        }
        let header = (tcp[12] as usize >> 4) * 4;
        if header < 20 || header > tcp.len() {
            return;
        }
        // A rejected/corrupt first copy must not become the origin of a later valid retransmission.
        let Ok(ip_packet) = Ipv4Packet::new_checked(&ip[..total]) else {
            return;
        };
        let Ok(tcp_packet) = TcpPacket::new_checked(tcp) else {
            return;
        };
        if !ip_packet.verify_checksum()
            || !tcp_packet.verify_checksum(
                &IpAddress::Ipv4(self.peer_ip.into()),
                &IpAddress::Ipv4(self.local_ip.into()),
            )
        {
            return;
        }
        let seq = u32::from_be_bytes(tcp[4..8].try_into().unwrap());
        let syn = tcp[13] & 2 != 0;
        if syn && self.base_seq.is_none() {
            self.base_seq = Some(seq.wrapping_add(1));
        }
        if tcp.len() > header {
            self.add(seq.wrapping_add(u32::from(syn)), tcp.len() - header, stamp);
        }
    }
    /// Match actual ordered bytes drained from smoltcp, not simply the latest unrelated RX batch.
    /// First received copies win over retransmissions; gaps are fatal rather than fabricating time.
    pub fn take(&mut self, len: usize) -> Result<Stamp, String> {
        let end = self
            .consumed
            .checked_add(len as u64)
            .ok_or("TCP stream offset overflow")?;
        let mut position = self.consumed;
        let mut stamp = Stamp::default();
        while position < end {
            let span = self
                .spans
                .iter()
                .filter(|s| s.start <= position && s.end > position)
                .min_by_key(|s| s.stamp.last_rx)
                .ok_or("TCP bytes lack RX timestamp provenance")?;
            let mut next = span.end.min(end);
            for other in &self.spans {
                if other.start > position && other.start < next {
                    next = other.start;
                }
            }
            stamp.merge(span.stamp);
            position = next;
        }
        self.consumed = end;
        self.spans.retain(|s| s.end > end);
        Ok(stamp)
    }
}

pub struct DpdkDevice {
    pub port: Port,
    pending: VecDeque<(Mbuf, Stamp)>,
    pub trace: Trace,
    pub tx_failed: Cell<u64>,
    last_empty_before: u64,
    pub bursts: u64,
    pub packets: u64,
}
impl DpdkDevice {
    pub fn new(port: Port, trace: Trace) -> Self {
        Self {
            port,
            trace,
            pending: VecDeque::with_capacity(BURST),
            tx_failed: Cell::new(0),
            last_empty_before: metrics::now(),
            bursts: 0,
            packets: 0,
        }
    }
    pub fn refill(&mut self) {
        if !self.pending.is_empty() {
            return;
        }
        let before = metrics::now();
        let (packets, t2) = self.port.receive();
        if packets.is_empty() {
            self.last_empty_before = before;
            return;
        }
        self.bursts += 1;
        self.packets += packets.len() as u64;
        let stamp = Stamp {
            first_rx: t2,
            last_rx: t2,
            ready_lower_bound: self.last_empty_before,
        };
        self.pending.extend(packets.into_iter().map(|p| (p, stamp)));
    }
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
    pub fn release_pending(&mut self) {
        self.pending.clear();
    }
}

pub struct Rx<'a> {
    packet: Mbuf,
    stamp: Stamp,
    trace: &'a mut Trace,
}
impl phy::RxToken for Rx<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        self.trace.observe(self.packet.data(), self.stamp);
        f(self.packet.data())
    }
}
pub struct Tx<'a> {
    port: &'a mut Port,
    failed: &'a Cell<u64>,
}
impl phy::TxToken for Tx<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut packet = self
            .port
            .alloc(len)
            .expect("DPDK TX pool exhausted or frame too large");
        let result = f(packet.data_mut());
        if self.port.send(packet).is_err() {
            self.failed.set(self.failed.get() + 1);
        }
        result
    }
}
impl phy::Device for DpdkDevice {
    type RxToken<'a>
        = Rx<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = Tx<'a>
    where
        Self: 'a;
    fn receive(&mut self, _: Instant) -> Option<(Rx<'_>, Tx<'_>)> {
        let (packet, stamp) = self.pending.pop_front()?;
        Some((
            Rx {
                packet,
                stamp,
                trace: &mut self.trace,
            },
            Tx {
                port: &mut self.port,
                failed: &self.tx_failed,
            },
        ))
    }
    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        Some(Tx {
            port: &mut self.port,
            failed: &self.tx_failed,
        })
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(BURST);
        caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn trace() -> Trace {
        let mut t = Trace::new([1; 4], [2; 4], 443, 50000);
        t.base_seq = Some(u32::MAX - 4);
        t
    }
    fn stamp(t: u64) -> Stamp {
        Stamp {
            first_rx: t,
            last_rx: t,
            ready_lower_bound: t - 1,
        }
    }
    #[test]
    fn split_coalesced_out_of_order_and_wrapping_bytes_keep_provenance() {
        let mut t = trace();
        t.add(5, 10, stamp(10)); // offset 10, delivered before offset 0
        t.add(u32::MAX - 4, 10, stamp(20));
        let a = t.take(15).unwrap();
        assert_eq!((a.first_rx, a.last_rx, a.ready_lower_bound), (10, 20, 19));
        let b = t.take(5).unwrap();
        assert_eq!((b.first_rx, b.last_rx), (10, 10));
        assert!(t.spans.is_empty());
    }
    #[test]
    fn retransmission_does_not_replace_first_arrival_and_holes_fail() {
        let mut t = trace();
        t.add(u32::MAX - 4, 10, stamp(10));
        t.add(u32::MAX - 4, 10, stamp(30));
        assert_eq!(t.take(10).unwrap().last_rx, 10);
        assert!(t.take(1).is_err());
    }
    #[test]
    fn corrupt_packet_is_not_used_as_valid_retransmissions_origin() {
        use smoltcp::wire::{IpProtocol, TcpSeqNumber};
        let mut t = trace();
        let mut frame = vec![0u8; 55];
        frame[12..14].copy_from_slice(&[8, 0]);
        {
            let mut ip = Ipv4Packet::new_unchecked(&mut frame[14..]);
            ip.set_version(4);
            ip.set_header_len(20);
            ip.set_total_len(41);
            ip.set_hop_limit(64);
            ip.set_next_header(IpProtocol::Tcp);
            ip.set_src_addr(t.peer_ip.into());
            ip.set_dst_addr(t.local_ip.into());
            ip.fill_checksum();
        }
        {
            let mut tcp = TcpPacket::new_unchecked(&mut frame[34..]);
            tcp.set_src_port(t.peer_port);
            tcp.set_dst_port(t.local_port);
            tcp.set_header_len(20);
            tcp.set_seq_number(TcpSeqNumber((u32::MAX - 4) as i32));
            tcp.payload_mut()[0] = 42;
            tcp.fill_checksum(
                &IpAddress::Ipv4(t.peer_ip.into()),
                &IpAddress::Ipv4(t.local_ip.into()),
            );
        }
        frame[54] ^= 1;
        t.observe(&frame, stamp(10));
        assert!(t.spans.is_empty());
        frame[54] ^= 1;
        t.observe(&frame, stamp(20));
        assert_eq!(t.take(1).unwrap().first_rx, 20);
    }
}
