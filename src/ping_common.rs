//! A/B 共用的会话 I/O、请求匹配和统计；差异留在 async 调度与手写状态机中。
//! T0 由客户端在决定发送时记录，T1/T2 在 C shim 的 TX/RX 返回处，T3 在回复交付时记录。
#![forbid(unsafe_code)]
use clap::Parser;
use dpdk::{Mbuf, Port};
use metrics::{now, Clock, Recorder};
use serde::Serialize;
use std::net::Ipv4Addr;
use wire::{Network, Packet, TxTemplate};

// A/B 共用的启动参数；解析和范围检查发生在启动阶段，不在逐包路径中。
#[derive(Parser, Debug, Serialize)]
#[command(args_override_self = true)]
pub struct Options {
    #[arg(long)]
    // 每个请求成功/超时/发送失败后的等待时长，单位微秒。
    pub delay_us: u64,
    #[arg(long)]
    // 允许发起新请求的持续时间，单位秒；结束后仍会完成在途请求和收尾。
    pub duration_sec: u64,
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(1..=128))]
    // 并发会话数，每个会话最多一个在途请求，范围 1～128。
    pub sessions: u16,
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(8..=1472))]
    // ICMP payload 字节数，包含保存 T0 的前 8 字节，不含 Ethernet/IP/ICMP 头。
    pub payload: u16,
    #[arg(long, default_value_t = 10000, value_parser = clap::value_parser!(u64).range(1..))]
    // 从 T1 起等待 reply 的超时时长，单位微秒。
    pub timeout_us: u64,
    #[arg(long)]
    // DPDK 网卡使用的本机 IPv4 地址，写入发送模板并用于接收过滤。
    pub src_ip: Ipv4Addr,
    #[arg(long, default_value = "10.202.8.15")]
    // 固定 ICMP 对端的 IPv4 地址。
    pub peer_ip: Ipv4Addr,
    #[arg(long, default_value = "06:ff:fd:b6:f0:cd", value_parser = parse_mac)]
    // 固定对端的 MAC 地址；已知后不需要实现 ARP 查询。
    pub peer_mac: [u8; 6],
    #[arg(long)]
    // 交给 DPDK 的网卡 PCI 地址，例如 0000:28:00.0。
    pub bdf: String,
    #[arg(long, default_value_t = 2)]
    // 收发线程/runtime 绑定的 Linux CPU 编号。
    pub core: usize,
    #[arg(long, default_value_t = 3)]
    // 后台统计线程绑定的 Linux CPU 编号，须与收发核不同。
    pub stats_core: u32,
    #[arg(long, default_value = "report.json")]
    // 最终 JSON 报告的文件路径，停止测量后才写入。
    pub output: String,
}
/// 把冒号分隔的六个十六进制字节解析成 MAC 地址；格式错误作为命令行解析错误返回。
fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let parts = s
        .split(':')
        .map(|s| u8::from_str_radix(s, 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    parts.try_into().map_err(|_| "MAC needs six octets".into())
}
#[derive(Default, Serialize)]
/// 一次运行的收发与异常计数；由收发线程独占修改，最终用于完整性对账。
pub struct Counters {
    /// 被 TX 驱动成功接纳的 ICMP request 数，不包含 ARP 应答。
    pub tx: u64,
    /// 已成功交给 session 的有效 reply 数，不包含迟到、重复或无关帧。
    pub rx: u64,
    /// 等待截止后仍未成功交付回复的请求数；迟到后也保留此计数。
    pub timeout: u64,
    /// 截止时间后收到并成功匹配的请求回复数，每个请求只计一次。
    pub late_reply: u64,
    /// ARP 应答未被 TX 驱动接纳的数量。
    pub arp_tx_failed: u64,
    /// ICMP request 已取得 mbuf，但 TX 驱动未接纳的次数。
    pub tx_failed: u64,
    /// 发送 request 时未能取得所需 mbuf 的次数。
    pub alloc_failed: u64,
}
#[derive(Clone, Copy)]
/// 一次成功提交的 request；内部时间使用 TSC ticks，由统计线程换算为 ns。
pub struct Stamp {
    /// 本次请求的 ICMP sequence，与回包字段比较。
    pub seq: u16,
    /// 客户端发送入口记录的绝对 TSC，同时写入 payload 作为请求身份。
    pub t0: u64,
    /// TX burst 返回后记录的绝对 TSC。
    pub t1: u64,
    /// 接收截止点，等于 t1 加 timeout ticks。
    pub deadline: u64,
    /// 本次请求是否已计过迟到回复，防止重复包重复增加 late_reply。
    pub late_seen: bool,
}
/// 接收缓冲区及整批收包的 T2；移动 Reply 就转移 mbuf 所有权，不复制包内容。
pub struct Reply {
    /// 收到的报文缓冲区唯一所有者；随 Reply 移动，不复制报文字节。
    pub mbuf: Mbuf,
    /// 取得这一批包的 RX burst 返回时刻，单位 TSC ticks，批内共用。
    pub t2: u64,
}
/// 保存四个打点及 RX mbuf，让客户端在 sleep 后再做统计和释放。
pub struct Sample {
    /// 收到的 mbuf 及 T2；保证报文在 session sleep 期间继续存活。
    pub reply: Reply,
    /// 与此回复对应的请求信息，提供 T0、T1 和唯一请求身份。
    pub stamp: Stamp,
    /// 回复交给 session 时记录的绝对 TSC；在成功计数和 sleep 之前保存。
    pub t3: u64,
}
#[derive(Serialize)]
/// 一条已超时请求的身份及迟到状态；即使 seq 回绕仍可通过 T0 区分。
pub struct Loss {
    /// 超时请求所属的会话编号。
    pub session: usize,
    /// 超时请求的 ICMP sequence。
    pub seq: u16,
    /// 超时请求 payload 中的原始发送 TSC，用来排除序号回绕造成的歧义。
    pub t0: u64,
    /// 最终 drain 结束前是否已经观察到这条请求的迟到回复。
    pub late: bool,
}
/// A/B 共用的单线程 I/O 状态；A 通过 `Rc<RefCell<_>>` 在 session 与 reactor 间共享。
pub struct Shared {
    /// 本次运行的固定配置，同时写入最终报告以便复现实验。
    pub options: Options,
    /// 当前线程独占的 DPDK 端口与内存池所有者。
    pub port: Port,
    /// 本机和对端的 IP/MAC，供构造模板、过滤 reply 和回答 ARP 使用。
    pub net: Network,
    /// DPDK 提供的 TSC 频率及时间单位转换方法。
    pub clock: Clock,
    /// 每个 session 一份可重复更新的发送字节模板；其中不持有 mbuf。
    pub templates: Vec<TxTemplate>,
    /// 按 session 索引的在途请求；None 表示当前没有等待交付的请求。
    /// seq 与 T0 一起匹配，防止 seq 回绕后接纳旧包。
    pub expected: Vec<Option<Stamp>>,
    /// 收发线程维护的成功、失败、迟到和协议处理计数。
    pub counters: Counters,
    /// 统计生产者，缓存 TSC 差值并批量提交到后台 SPSC 队列。
    pub metrics: Recorder,
    /// 超时请求列表；后续收到迟到包时更新对应条目的 late 标志。
    pub losses: Vec<Loss>,
    /// 测量窗口的绝对起始 TSC；open 后为 0，start 时设置。
    pub start: u64,
    /// start 前保存持续 ticks，start 后保存停止发起新请求的绝对 TSC。
    pub end: u64,
    /// 每轮请求后的 sleep 时长，已从微秒换算成 TSC ticks。
    pub delay: u64,
    /// 从成功 TX 到接收截止点的时长，单位 TSC ticks。
    pub timeout: u64,
    /// 下次允许执行 DPDK 驱动维护的绝对 TSC。
    next_maintenance: u64,
}
impl Shared {
    /// 解析参数并初始化端口、DPDK 时钟、统计线程、报文模板和请求表。
    /// 耗时初始化在测量开始前完成，调用 start 后才进入测试时间窗口。
    pub fn open() -> Result<Self, String> {
        let options = Options::parse();
        let port = Port::open(&options.bdf, options.core)?;
        let clock = Clock { hz: port.tsc_hz() };
        if options.stats_core as usize == options.core {
            return Err("stats-core must differ from core".into());
        }
        let stats_core = options.stats_core;
        let metrics = Recorder::new(clock, move || dpdk::pin_thread(stats_core));
        let net = Network {
            mac: port.mac,
            ip: options.src_ip.octets(),
            peer_mac: options.peer_mac,
            peer_ip: options.peer_ip.octets(),
        };
        let templates = (0..options.sessions as usize)
            .map(|sid| TxTemplate::new(net, sid, options.payload as usize))
            .collect();
        let delay = clock.us(options.delay_us);
        let timeout = clock.us(options.timeout_us);
        // open 时暂存持续 ticks；start 时加上起点，转换成绝对结束时间。
        let end = options
            .duration_sec
            .checked_mul(clock.hz)
            .ok_or("duration too large")?;
        let expected = vec![None; options.sessions as usize];
        Ok(Self {
            options,
            port,
            net,
            clock,
            templates,
            expected,
            counters: Counters::default(),
            metrics,
            losses: Vec::new(),
            start: 0,
            end,
            delay,
            timeout,
            next_maintenance: 0,
        })
    }
    /// 保存测量起点，将预存的持续 ticks 转为结束时间，并设置首次驱动维护时间；只调用一次。
    pub fn start(&mut self) {
        self.start = now();
        self.end += self.start;
        self.next_maintenance = self.start + self.clock.us(1000);
    }
    /// 计算指定 session 的首发绝对时刻，使各会话均匀错开在一个 delay 窗口内。
    pub fn phase(&self, sid: usize) -> u64 {
        // 只错开首发；后续都是收到 reply/超时后再等 delay，不追赶固定发送节拍。
        self.start + self.delay * sid as u64 / self.options.sessions as u64
    }
    /// A/B 使用同一发送路径；T1−T0 包含模板更新、mbuf 分配、复制与 TX 提交的耗时。
    #[inline]
    pub fn send(
        &mut self,
        sid: usize,
        seq: u16,
        sleep_deadline: Option<u64>,
        t0: u64,
    ) -> Option<Stamp> {
        let frame = self.templates[sid].emit(seq, t0);
        let Some(mut packet) = self.port.alloc(frame.len()) else {
            self.counters.alloc_failed += 1;
            return None;
        };
        packet.data_mut().copy_from_slice(frame);
        let t1 = match self.port.send(packet) {
            Ok(t1) => t1,
            Err(_) => {
                self.counters.tx_failed += 1;
                return None;
            }
        };
        let stamp = Stamp {
            seq,
            t0,
            t1,
            deadline: t1 + self.timeout, // 从 TX 提交返回时开始等待 reply。
            late_seen: false,
        };
        self.expected[sid] = Some(stamp);
        self.counters.tx += 1;
        if let Some(deadline) = sleep_deadline {
            // 段③：上次 sleep 到期 → 下一次 T0，包含恢复后统计/释放等工作，单独报告。
            self.metrics.timer(t0 - deadline);
        }
        Some(stamp)
    }
    /// T3 之前共用的解析、session 分拣和请求匹配；未交付的 mbuf 随局部变量 Drop 释放。
    pub fn dispatch(&mut self, mut mbuf: Mbuf, t2: u64) -> Option<(usize, Reply)> {
        match wire::classify(mbuf.data(), self.net, self.options.sessions as usize) {
            Packet::Arp => {
                // DPDK 接管的端口没有内核代答 ARP，复用收到的缓冲区原地生成应答。
                wire::arp_reply(mbuf.data_mut(), self.net);
                if self.port.send(mbuf).is_err() {
                    self.counters.arp_tx_failed += 1;
                }
            }
            Packet::Other => {}
            Packet::Reply { session, seq, t0 } => {
                if let Some(s) = self.expected[session].as_mut() {
                    if s.seq == seq && s.t0 == t0 {
                        // 用收到这一批包的 T2 判定是否超时，不把排队到 T3 的时间算作网络迟到。
                        if t2 <= s.deadline {
                            return Some((session, Reply { mbuf, t2 }));
                        }
                        if !s.late_seen {
                            s.late_seen = true;
                            self.counters.late_reply += 1;
                        }
                        return None;
                    }
                }
                // 已经结束的请求仍可能收到迟到包：补记 loss，但不再交给当前 session。
                if let Some(loss) = self
                    .losses
                    .iter_mut()
                    .rev()
                    .find(|l| l.session == session && l.seq == seq && l.t0 == t0 && !l.late)
                {
                    loss.late = true;
                    self.counters.late_reply += 1;
                }
            }
        }
        None
    }
    /// 标记一条匹配回复已交给 session：清除在途请求并增加成功 RX 计数；在 T3 之后调用。
    pub fn accepted(&mut self, sid: usize) {
        self.expected[sid] = None;
        self.counters.rx += 1;
    }
    /// 结束指定 session 的在途请求，增加 timeout 并保存可供迟到包对账的身份。
    /// 调用者必须保证 `expected[sid]` 中仍有本次请求；超时不生成成功延迟样本。
    pub fn timed_out(&mut self, sid: usize) {
        // 超时不进入成功 reply 的延迟分布；保留请求身份，结束时对账迟到/失踪情况。
        let stamp = self.expected[sid].take().unwrap();
        self.counters.timeout += 1;
        self.losses.push(Loss {
            session: sid,
            seq: stamp.seq,
            t0: stamp.t0,
            late: stamp.late_seen,
        });
    }
    /// 在 sleep 后把该请求的四个打点提交给统计器，再释放独占的 RX mbuf。
    pub fn record(&mut self, sample: Sample) {
        self.metrics
            .reply(sample.stamp.t0, sample.stamp.t1, sample.reply.t2, sample.t3);
        drop(sample.reply.mbuf);
    }
    /// 每约 1ms 调用一次 DPDK 驱动维护；调用者将它放在当前轮交付 reply 和 poll 之后。
    pub fn maintenance(&mut self) {
        let t = now();
        if t >= self.next_maintenance {
            // 每 1ms 服务一次 DPDK timer（含 ENA watchdog）；不是本项目的 sleep timer。
            // 调用方把它放在 reply 交付后，保留驱动维护而不阻塞当前回包恢复。
            self.port.maintenance();
            self.next_maintenance = t + self.clock.us(1000);
        }
    }
    /// 停止新请求后的最终收尾：额外收迟到包 50ms、关闭端口、检查 mbuf、排空统计并输出。
    /// label 标识 A/B；对账失败或输出文件写入失败返回错误。
    pub fn finish(mut self, label: &str) -> Result<(), String> {
        let measured_end = now();
        // 各 session 已结束最后一次等待和 sleep；额外收尾 50ms 补记迟到包，不计入运行时长。
        let drain_end = now() + self.clock.us(50_000);
        while now() < drain_end {
            let (batch, t2) = self.port.receive();
            for m in batch {
                self.dispatch(m, t2);
            }
            self.maintenance();
        }
        // 停止/关闭端口后，RX 描述符和未回收的 TX mbuf 才会归还内存池。
        let (initial, final_count) = self.port.finish();
        let missing = self.losses.iter().filter(|l| !l.late).count();
        // 停止收发后排空统计队列并等候最终分位数；写日志/JSON 不进入测量窗口。
        let latency = self.metrics.finish();
        // 每个成功提交的 request 必须落在成功或超时之一；迟到是超时的补充分类。
        let accounted = self.counters.tx == self.counters.rx + self.counters.timeout;
        let report = serde_json::json!({
            "client": label, "options": self.options,
            "elapsed_sec": (measured_end-self.start) as f64 / self.clock.hz as f64,
            "units": "ns",
            "latency": latency, "counters": self.counters, "losses": self.losses,
            "missing": missing, "accounted": accounted,
            "mempool": {"initial": initial, "final": final_count, "leak_free": initial == final_count}
        });
        println!(
            "client={label} sessions={} duration={}s delay={}us payload={} timeout={}us",
            self.options.sessions,
            self.options.duration_sec,
            self.options.delay_us,
            self.options.payload,
            self.options.timeout_us
        );
        println!("metric(ns)         p50        p90        p99      p99.9     p99.99        max      samples");
        for (name, h) in &latency {
            println!(
                "{name:14} {:10} {:10} {:10} {:10} {:10} {:10} {:12}",
                h.p50, h.p90, h.p99, h.p999, h.p9999, h.max, h.count
            );
        }
        println!("tx={} rx={} timeout={} late={} missing={} tx_failed={} alloc_failed={} accounted={accounted}",
            self.counters.tx,self.counters.rx,self.counters.timeout,self.counters.late_reply,missing,self.counters.tx_failed,self.counters.alloc_failed);
        println!(
            "mempool: {initial} -> {final_count}; report={}",
            self.options.output
        );
        std::fs::write(
            &self.options.output,
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .map_err(|e| e.to_string())?;
        if initial != final_count || !accounted {
            return Err("mbuf or packet accounting mismatch".into());
        }
        Ok(())
    }
}
