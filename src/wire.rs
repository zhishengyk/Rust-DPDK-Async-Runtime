//! 固定格式的 Ethernet/IPv4/ICMP 编解码及 ARP 应答，供 A/B 共用。
//! 仅实现实验需要的无 IP options、无分片报文；应用标识由 ICMP id、seq 和 payload 内的 T0 组成。
#![forbid(unsafe_code)]

// ICMP identifier = BASE_ID + session，回包时可直接映射到会话槽。
pub const BASE_ID: u16 = 0x4000;
#[derive(Clone, Copy)]
/// 固定链路两端的二层/三层地址，用于发送模板和接收过滤。
pub struct Network {
    /// 本机 DPDK 端口的 6 字节 MAC。
    pub mac: [u8; 6],
    /// 本机 IPv4 地址的 4 个网络顺序字节。
    pub ip: [u8; 4],
    /// 固定对端 MAC，直接作为请求帧的目的地址。
    pub peer_mac: [u8; 6],
    /// 固定对端 IPv4，用于请求目的地址和回包源地址过滤。
    pub peer_ip: [u8; 4],
}
/// 按网络字节序读取偏移 at 处的 16 位字段；调用者先保证两个字节都在切片内。
fn word(p: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([p[at], p[at + 1]])
}
/// 按网络字节序累加 16 位字，奇数长度的最后一个字节低位补零；暂不折叠进位。
fn sum(bytes: &[u8]) -> u32 {
    // Internet checksum 按网络字节序累加 16 位字；奇数字节数时末尾补零。
    bytes
        .chunks(2)
        .map(|p| ((p[0] as u32) << 8) | p.get(1).copied().unwrap_or(0) as u32)
        .sum()
}
/// 反复折叠 checksum 累加值的高 16 位进位，再按位取反得到最终校验和。
fn finish(mut s: u32) -> u16 {
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}
/// 计算完整字节序列的 Internet checksum，供启动模板构造和校验测试使用。
pub fn checksum(bytes: &[u8]) -> u16 {
    finish(sum(bytes))
}

/// 每个 session 一份可复用字节模板；它是普通 Vec，不持有 DPDK mbuf。
pub struct TxTemplate {
    /// 预分配的完整报文字节，含各层头和 payload；逐包只改变化字段。
    frame: Vec<u8>,
    /// ICMP 固定字节的未折叠 checksum 累加和，不包含变化的 seq/T0。
    base: u32,
}
impl TxTemplate {
    /// 在启动时创建指定 session 的完整帧模板，固定 MAC/IP/id/padding 并缓存 ICMP 常量和。
    /// payload 包含保存 T0 的 8 字节；IP checksum 只在此处计算一次。
    pub fn new(net: Network, session: usize, payload: usize) -> Self {
        assert!((8..=1472).contains(&payload));
        // 帧布局：Ethernet 14B + IPv4 20B + ICMP 8B + payload；payload 前 8B 存 T0。
        let mut p = vec![0; 42 + payload];
        p[0..6].copy_from_slice(&net.peer_mac);
        p[6..12].copy_from_slice(&net.mac);
        p[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        p[14] = 0x45;
        p[16..18].copy_from_slice(&((28 + payload) as u16).to_be_bytes());
        p[20] = 0x40; // DF：不允许 IP 分片。
        p[22] = 64;
        p[23] = 1;
        p[26..30].copy_from_slice(&net.ip);
        p[30..34].copy_from_slice(&net.peer_ip);
        let ip_checksum = checksum(&p[14..34]);
        p[24..26].copy_from_slice(&ip_checksum.to_be_bytes());
        p[34] = 8;
        p[38..40].copy_from_slice(&(BASE_ID + session as u16).to_be_bytes());
        p[50..].fill(0xa5);
        // 此时 checksum、seq、T0 均为零，缓存所有不变 ICMP 字节的累加和。
        let base = sum(&p[34..]);
        Self { frame: p, base }
    }
    /// 每包只更新 seq/T0，并在固定部分的和上加新值，避免重新扫描整个 payload。
    pub fn emit(&mut self, seq: u16, t0: u64) -> &[u8] {
        self.frame[40..42].copy_from_slice(&seq.to_be_bytes());
        self.frame[42..50].copy_from_slice(&t0.to_le_bytes()); // 应用 payload 自定小端，解析时对应还原。
        let c = finish(self.base + seq as u32 + sum(&self.frame[42..50]));
        self.frame[36..38].copy_from_slice(&c.to_be_bytes());
        &self.frame
    }
}

#[derive(Debug, PartialEq)]
/// 报文分类结果；只保存解析出的身份字段，不持有或复制 mbuf。
pub enum Packet {
    /// 格式和地址符合要求的 Echo Reply，仍须与当前请求核对。
    Reply {
        /// ICMP identifier 减 BASE_ID 后得到的会话索引。
        session: usize,
        /// ICMP sequence，按网络字节序读取。
        seq: u16,
        /// payload 前 8 字节中的原始 T0，按本协议约定的小端还原。
        t0: u64,
    },
    /// 请求查询本机 IPv4 对应 MAC 的 ARP request，需要原地生成应答。
    Arp,
    /// 无关、长度不足或本实验不支持的报文，交给上层计数并释放。
    Other,
}
/// 只解析本实验支持的 ARP request 或固定 IPv4/ICMP Echo Reply；其他帧返回 Other。
/// 返回的 session/seq/T0 只是报文身份，在途请求匹配和截止时间检查由 Shared::dispatch 完成。
pub fn classify(p: &[u8], net: Network, sessions: usize) -> Packet {
    // 先验证长度再按固定偏移读取；这些检查保证切片访问安全，也排除无关帧。
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
    // 只接纳指定对端发往本机的 IPv4 ICMP Echo Reply（type=0, code=0）。
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
/// 调用前已由 classify 确认为发给本机的 ARP request；直接在原缓冲区内改成 reply。
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
    /// 生成测试专用的固定本机/对端地址，不访问真实网卡。
    fn net() -> Network {
        Network {
            mac: [1; 6],
            ip: [10, 0, 0, 1],
            peer_mac: [2; 6],
            peer_ip: [10, 0, 0, 2],
        }
    }
    #[test]
    /// 验证不同 payload 长度、序号和 T0 下，模板增量计算与完整 checksum 校验一致。
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
    /// 验证正常回复身份解析、截断/分片拒绝，以及 ARP 应答地址与操作码的改写。
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
