#![forbid(unsafe_code)]

pub const BASE_ID: u16 = 0x4000;
#[derive(Clone, Copy)]
pub struct Network {
    pub mac: [u8; 6],
    pub ip: [u8; 4],
    pub peer_mac: [u8; 6],
    pub peer_ip: [u8; 4],
}
fn word(p: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([p[at], p[at + 1]])
}
fn sum(bytes: &[u8]) -> u32 {
    bytes
        .chunks(2)
        .map(|p| ((p[0] as u32) << 8) | p.get(1).copied().unwrap_or(0) as u32)
        .sum()
}
fn finish(mut s: u32) -> u16 {
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}
pub fn checksum(bytes: &[u8]) -> u16 {
    finish(sum(bytes))
}

pub struct TxTemplate {
    frame: Vec<u8>,
    base: u32,
}
impl TxTemplate {
    pub fn new(net: Network, session: usize, payload: usize) -> Self {
        assert!((8..=1472).contains(&payload));
        let mut p = vec![0; 42 + payload];
        p[0..6].copy_from_slice(&net.peer_mac);
        p[6..12].copy_from_slice(&net.mac);
        p[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        p[14] = 0x45;
        p[16..18].copy_from_slice(&((28 + payload) as u16).to_be_bytes());
        p[20] = 0x40; // Don't fragment.
        p[22] = 64;
        p[23] = 1;
        p[26..30].copy_from_slice(&net.ip);
        p[30..34].copy_from_slice(&net.peer_ip);
        let ip_checksum = checksum(&p[14..34]);
        p[24..26].copy_from_slice(&ip_checksum.to_be_bytes());
        p[34] = 8;
        p[38..40].copy_from_slice(&(BASE_ID + session as u16).to_be_bytes());
        p[50..].fill(0xa5);
        let base = sum(&p[34..]);
        Self { frame: p, base }
    }
    /// Update only the changing words; no full-payload checksum on transmit.
    pub fn emit(&mut self, seq: u16, t0: u64) -> &[u8] {
        self.frame[40..42].copy_from_slice(&seq.to_be_bytes());
        self.frame[42..50].copy_from_slice(&t0.to_le_bytes());
        let c = finish(self.base + seq as u32 + sum(&self.frame[42..50]));
        self.frame[36..38].copy_from_slice(&c.to_be_bytes());
        &self.frame
    }
}

#[derive(Debug, PartialEq)]
pub enum Packet {
    Reply { session: usize, seq: u16, t0: u64 },
    Arp,
    Other,
}
pub fn classify(p: &[u8], net: Network, sessions: usize) -> Packet {
    if p.len() < 42 {
        return Packet::Other;
    }
    if word(p, 12) == 0x0806
        && word(p, 14) == 1
        && word(p, 16) == 0x0800
        && p[18..20] == [6, 4]
        && word(p, 20) == 1
        && p[38..42] == net.ip
    {
        return Packet::Arp;
    }
    if p.len() < 50
        || word(p, 12) != 0x0800
        || p[14] != 0x45
        || p[23] != 1
        || word(p, 20) & 0x3fff != 0
        || p[26..30] != net.peer_ip
        || p[30..34] != net.ip
        || p[34..36] != [0, 0]
    {
        return Packet::Other;
    }
    let len = word(p, 16) as usize;
    if len < 36 || len + 14 > p.len() {
        return Packet::Other;
    }
    let Some(session) = word(p, 38).checked_sub(BASE_ID) else {
        return Packet::Other;
    };
    if session as usize >= sessions {
        return Packet::Other;
    }
    Packet::Reply {
        session: session as usize,
        seq: word(p, 40),
        t0: u64::from_le_bytes(p[42..50].try_into().unwrap()),
    }
}
pub fn arp_reply(p: &mut [u8], net: Network) {
    let peer_mac: [u8; 6] = p[22..28].try_into().unwrap();
    let peer_ip: [u8; 4] = p[28..32].try_into().unwrap();
    p[0..6].copy_from_slice(&peer_mac);
    p[6..12].copy_from_slice(&net.mac);
    p[20..22].copy_from_slice(&2u16.to_be_bytes());
    p[22..28].copy_from_slice(&net.mac);
    p[28..32].copy_from_slice(&net.ip);
    p[32..38].copy_from_slice(&peer_mac);
    p[38..42].copy_from_slice(&peer_ip);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn net() -> Network {
        Network {
            mac: [1; 6],
            ip: [10, 0, 0, 1],
            peer_mac: [2; 6],
            peer_ip: [10, 0, 0, 2],
        }
    }
    #[test]
    fn incremental_matches_full_checksum() {
        for payload in [8, 63, 64, 1472] {
            let mut tpl = TxTemplate::new(net(), 63, payload);
            for seq in [0, 1, 32768, 65535] {
                for tsc in [0, 1, 0x123456789abcdef0, u64::MAX] {
                    let p = tpl.emit(seq, tsc);
                    assert_eq!(checksum(&p[14..34]), 0);
                    assert_eq!(checksum(&p[34..]), 0);
                }
            }
        }
    }
    #[test]
    fn parse_truncation_fragments_and_arp() {
        let n = net();
        let mut tpl = TxTemplate::new(n, 2, 64);
        let mut p = tpl.emit(65535, 1234).to_vec();
        p[26..30].copy_from_slice(&n.peer_ip);
        p[30..34].copy_from_slice(&n.ip);
        p[34] = 0;
        assert_eq!(
            classify(&p, n, 64),
            Packet::Reply {
                session: 2,
                seq: 65535,
                t0: 1234
            }
        );
        for len in 0..p.len() {
            assert_eq!(classify(&p[..len], n, 64), Packet::Other);
        }
        p[20] |= 0x20;
        assert_eq!(classify(&p, n, 64), Packet::Other);
        let mut arp = [0u8; 42];
        arp[12..22].copy_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
        arp[22..28].copy_from_slice(&n.peer_mac);
        arp[28..32].copy_from_slice(&n.peer_ip);
        arp[38..42].copy_from_slice(&n.ip);
        assert_eq!(classify(&arp, n, 64), Packet::Arp);
        arp_reply(&mut arp, n);
        assert_eq!(&arp[20..22], &[0, 2]);
        assert_eq!(&arp[32..38], &n.peer_mac);
        assert_eq!(&arp[38..42], &n.peer_ip);
    }
}
