//! A/B 共用计时与统计；保留 TSC 打点，只在统计端换算 ns 并写入 HDR 直方图。
use hdrhistogram::Histogram;
use rtrb::{Producer, RingBuffer};
use serde::Serialize;
use std::{
    arch::x86_64::{_mm_lfence, _rdtsc},
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    sync::{
        atomic::{compiler_fence, Ordering},
        mpsc::sync_channel,
    },
    thread::{self, JoinHandle},
};

/// 官方 intrinsic；CPU 与编译器屏障保持原来的 LFENCE/RDTSC/LFENCE 计时边界。
#[inline(always)]
pub fn now() -> u64 {
    compiler_fence(Ordering::SeqCst);
    // SAFETY: 项目目标是 x86_64，支持 TSC 和 SSE2；不访问指针。
    let t = unsafe {
        _mm_lfence();
        let t = _rdtsc();
        _mm_lfence();
        t
    };
    compiler_fence(Ordering::SeqCst);
    t
}

#[derive(Clone, Copy)]
/// 按 DPDK TSC 频率进行单位换算的轻量配置；不执行系统时间读取或自行校准。
pub struct Clock {
    /// TSC 每秒的 ticks 数，由 EAL 初始化后的 rte_get_tsc_hz() 提供。
    pub hz: u64,
}
impl Clock {
    /// 把微秒时长换算为 TSC ticks，整数除法向下取整，供 delay 和 timeout 使用。
    pub fn us(self, us: u64) -> u64 {
        ((us as u128 * self.hz as u128) / 1_000_000) as u64
    }
    /// 把 TSC 差值换算为整数纳秒，向下取整；不做按 10ns 的额外舍入。
    pub fn ns(self, ticks: u64) -> u64 {
        ((ticks as u128 * 1_000_000_000) / self.hz as u128) as u64
    }
}

/// HDR 的有效数字位数，用于配置分桶相对精度。
const SIGNIFICANT_FIGURES: u8 = 5;
/// 一次提交到 SPSC 的最大事件数；事件数不等于报文数。
const BATCH_SIZE: usize = 256;
/// SPSC 可容纳的满批数量，总容量为 BATCH_SIZE × QUEUE_BATCHES 条事件。
const QUEUE_BATCHES: usize = 64;

/// 一个指标的 HDR 分布及精确最大值，仅由统计线程维护。
struct Distribution {
    /// 存储各个纳秒区间的样本计数，5 位有效数字，禁止自动扩容。
    histogram: Histogram<u64>,
    /// 已记录的精确整数纳秒最大值，避免大值合并桶抬高 max。
    max: u64,
}
impl Distribution {
    /// 创建覆盖完整 u64 数值范围的固定 HDR 桶，避免统计过程中扩容。
    fn new() -> Self {
        // 预建覆盖 u64 范围的桶，禁止自动扩容；0..=262143ns 每个整数单独一个桶。
        Self {
            histogram: Histogram::new_with_max(u64::MAX, SIGNIFICANT_FIGURES).unwrap(),
            max: 0,
        }
    }
    /// 把一个整数纳秒样本写入 HDR，并单独维护不受桶合并影响的精确最大值。
    fn record(&mut self, ns: u64) {
        self.histogram.record(ns).unwrap();
        self.max = self.max.max(ns);
    }
    /// 读取计数和各分位数；桶上界不超过已记录最大值，空分布的各数值为 0。
    fn summary(&self) -> Summary {
        let quantile = |q| self.histogram.value_at_quantile(q).min(self.max);
        Summary {
            count: self.histogram.len(),
            p50: quantile(0.5),
            p90: quantile(0.9),
            p99: quantile(0.99),
            p999: quantile(0.999),
            p9999: quantile(0.9999),
            max: self.max,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
/// 一个指标的最终统计摘要；所有延迟字段单位为 ns，count 为样本数。
pub struct Summary {
    /// 进入这项分布的样本总数；超时请求不进入 process 和 end_to_end 分布。
    pub count: u64,
    /// 第 50 百分位，即中位数，单位 ns。
    pub p50: u64,
    /// 第 90 百分位，单位 ns。
    pub p90: u64,
    /// 第 99 百分位，单位 ns。
    pub p99: u64,
    /// 第 99.9 百分位，单位 ns。
    pub p999: u64,
    /// 第 99.99 百分位，单位 ns。
    pub p9999: u64,
    /// 所有输入样本中的最大整数纳秒值，独立于 HDR 桶上界。
    pub max: u64,
}

#[derive(Clone, Copy)]
#[repr(u64)]
pub enum Kind {
    Reply = 1,
    Sleep = 2,
    Timer = 3,
    Timeout = 4,
    Late = 5,
    AllocFailed = 6,
    TxFailed = 7,
}

/// 原始打点事件；没有发生的时刻填 0，不推算或伪造时间戳。
#[derive(Clone, Copy)]
struct Event {
    kind: Kind,
    id: u64,     // session << 32 | seq；T0 及每个 session 的顺序区分 seq 回绕。
    t: [u64; 6], // T0、T1、T2、T3、T4、T5，原始 TSC ticks。
}
impl Event {
    fn bytes(self) -> [u8; 64] {
        let mut bytes = [0; 64];
        let values = [
            self.kind as u64,
            self.id,
            self.t[0],
            self.t[1],
            self.t[2],
            self.t[3],
            self.t[4],
            self.t[5],
        ];
        for (chunk, value) in bytes.chunks_exact_mut(8).zip(values) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
}

/// 后台线程独占四组任务指标及发送、接收两段分布。
struct Histograms {
    /// 与前台相同的 TSC 频率，用于把所有差值统一换算为 ns。
    clock: Clock,
    /// process、end_to_end、timer、sleep_error、T1−T0、T3−T2。
    values: [Distribution; 6],
}
impl Histograms {
    /// 为后台统计线程创建直方图，共用一份 TSC 频率配置。
    fn new(clock: Clock) -> Self {
        Self {
            clock,
            values: std::array::from_fn(|_| Distribution::new()),
        }
    }
    /// 消费一条数值事件：先把该请求的发送/接收 ticks 相加，再换算并分别入桶。
    fn record(&mut self, event: Event) {
        let [t0, t1, t2, t3, t4, t5] = event.t;
        match event.kind {
            Kind::Reply => {
                let send = t1 - t0;
                let receive = t3 - t2;
                // 每个请求先按 ticks 相加，再换算 ns；不能把两个分位数相加。
                self.values[0].record(self.clock.ns(send + receive));
                self.values[1].record(self.clock.ns(t3 - t0));
                self.values[4].record(self.clock.ns(send));
                self.values[5].record(self.clock.ns(receive));
            }
            Kind::Timer => self.values[2].record(self.clock.ns(t0 - t4)),
            Kind::Sleep => self.values[3].record(self.clock.ns(t5 - t4)),
            _ => {}
        }
    }
    /// 生成按指标名排列的最终摘要，供前台在停止收发后序列化和打印。
    fn summaries(&self) -> BTreeMap<&'static str, Summary> {
        [
            "process",
            "end_to_end",
            "timer",
            "sleep_error",
            "t1_t0",
            "t3_t2",
        ]
        .into_iter()
        .zip(&self.values)
        .map(|(name, distribution)| (name, distribution.summary()))
        .collect()
    }
}

/// 收发线程本地的固定事件缓冲，用批量发布摊薄跨核同步成本。
struct Batch {
    /// 固定事件存储；只有前 len 项有效，其余是初始化值或上次批次的残留。
    events: [Event; BATCH_SIZE],
    /// 当前已缓存的有效事件数，也是下一条事件写入的位置。
    len: usize,
}
impl Batch {
    /// 初始化一个可重复使用的本地事件数组；len=0 表示还没有有效事件。
    fn new() -> Self {
        Self {
            events: [Event {
                kind: Kind::Timer,
                id: 0,
                t: [0; 6],
            }; BATCH_SIZE],
            len: 0,
        }
    }
}

/// 单核收发线程只追加数值；每 256 条交给统计线程，不跨线程传 mbuf。
/// 有界队列满时等待空间，不能静默丢样本。
pub struct Recorder {
    /// 当前收发线程独占的待发布事件缓冲。
    batch: Batch,
    /// SPSC 单生产者端，按批把数值发布给后台线程。
    sender: Producer<Event>,
    /// 后台统计线程的句柄；finish 通过 join 取回完整摘要。
    worker: JoinHandle<(BTreeMap<&'static str, Summary>, u64)>,
}
impl Recorder {
    /// 创建 SPSC 队列和统计线程，在线程中执行 worker_init（生产环境用于绑核）。
    /// 等待线程初始化直方图完成后才返回，避免把启动成本混进测量窗口。
    pub fn new(
        clock: Clock,
        trace: Option<File>,
        worker_init: impl FnOnce() + Send + 'static,
    ) -> Self {
        let (sender, mut receiver) = RingBuffer::<Event>::new(BATCH_SIZE * QUEUE_BATCHES);
        let (ready, started) = sync_channel(0);
        let worker = thread::Builder::new()
            .name("ping-statistics".into())
            .spawn(move || {
                worker_init(); // EAL 主线程已绑核；子线程必须改绑，避免继承收发核的 affinity。
                let mut histograms = Histograms::new(clock);
                let mut trace = trace.map(|file| BufWriter::with_capacity(1024 * 1024, file));
                let mut records = 0;
                if let Some(file) = &mut trace {
                    let mut header = [0; 64];
                    header[..8].copy_from_slice(b"DPDKTS01");
                    header[8..16].copy_from_slice(&clock.hz.to_le_bytes());
                    file.write_all(&header).expect("cannot write trace header");
                }
                ready.send(()).unwrap();
                loop {
                    // 先看发送端是否关闭，再读队列，避免退出检查与最后一批发布竞争而丢尾包。
                    let closed = receiver.is_abandoned();
                    match receiver.pop() {
                        Ok(event) => {
                            histograms.record(event);
                            if let Some(file) = &mut trace {
                                file.write_all(&event.bytes())
                                    .expect("cannot write raw trace");
                                records += 1;
                            }
                        }
                        Err(_) if closed => break,
                        Err(_) => std::hint::spin_loop(),
                    }
                }
                if let Some(mut file) = trace {
                    file.flush().expect("cannot flush raw trace");
                    file.get_ref().sync_all().expect("cannot sync raw trace");
                }
                (histograms.summaries(), records)
            })
            .expect("cannot start statistics thread");
        // 统计线程绑核、分配直方图完成后才允许开始测量。
        started
            .recv()
            .expect("statistics thread failed to initialize");
        Self {
            batch: Batch::new(),
            sender,
            worker,
        }
    }
    /// 向收发线程本地缓冲追加一条事件；凑满 BATCH_SIZE 条时同步调用 flush 发布。
    fn record(&mut self, event: Event) {
        self.batch.events[self.batch.len] = event;
        self.batch.len += 1;
        if self.batch.len == BATCH_SIZE {
            self.flush();
        }
    }
    /// 把有效事件一次性复制并发布到 SPSC，随后复用本地数组；不足一批也可以提交。
    /// 队列满时等待空间，保证统计完整，不静默丢样本。
    fn flush(&mut self) {
        if self.batch.len == 0 {
            return;
        }
        let events = &self.batch.events[..self.batch.len];
        // 一次复制、一批发布；复用本地数组，不逐批清零，不唤醒休眠的 OS 线程。
        if self.sender.push_entire_slice(events).is_err() {
            loop {
                assert!(!self.sender.is_abandoned(), "statistics thread stopped");
                if self.sender.push_entire_slice(events).is_ok() {
                    break;
                }
                std::hint::spin_loop();
            }
        }
        self.batch.len = 0;
    }
    /// 发布原始打点，换算、分桶和缓冲写盘都由统计线程完成。
    pub fn event(&mut self, kind: Kind, session: usize, seq: u16, t: [u64; 6]) {
        self.record(Event {
            kind,
            id: ((session as u64) << 32) | seq as u64,
            t,
        });
    }
    pub fn reply(&mut self, session: usize, seq: u16, t: [u64; 4]) {
        self.event(Kind::Reply, session, seq, [t[0], t[1], t[2], t[3], 0, 0]);
    }
    /// 提交 sleep deadline 和下一次 T0 的原始打点；这项不计入 process。
    pub fn timer(&mut self, session: usize, seq: u16, deadline: u64, t0: u64) {
        self.event(Kind::Timer, session, seq, [t0, 0, 0, 0, deadline, 0]);
    }
    /// 提交 sleep deadline 和恢复执行时刻的原始打点。
    pub fn sleep_error(&mut self, session: usize, seq: u16, deadline: u64, resumed: u64) {
        self.event(Kind::Sleep, session, seq, [0, 0, 0, 0, deadline, resumed]);
    }
    /// 提交尾批并关闭生产者，等待后台排空后返回摘要。
    pub fn finish(mut self) -> (BTreeMap<&'static str, Summary>, u64) {
        self.flush(); // 最后一批不足 256 条也必须提交。
        drop(self.sender); // receiver 排空后退出，再汇总；没有后台线程悬挂。
        self.worker.join().expect("statistics thread panicked")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// 验证统计线程收齐整批与不足一批的尾部，并保留各类事件的计数和最大值。
    fn worker_drains_full_and_partial_batches_without_losing_samples() {
        let path = std::env::temp_dir().join(format!("dpdk-trace-test-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut recorder = Recorder::new(Clock { hz: 1_000_000_000 }, Some(file), || {});
        let n = BATCH_SIZE * 2 + 7;
        for i in 0..n {
            recorder.reply(3, i as u16, [0, 7, 20, 33]);
            recorder.sleep_error(3, i as u16, 1000, 1000 + i as u64);
            if i % 2 == 0 {
                recorder.timer(3, i as u16, 1000, 1000 + i as u64);
            }
        }
        let (r, records) = recorder.finish();
        let raw = std::fs::read(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(&raw[..8], b"DPDKTS01");
        assert_eq!(records as usize, n * 2 + n.div_ceil(2));
        assert_eq!(raw.len(), 64 + records as usize * 64);
        let first: Vec<_> = raw[64..128]
            .chunks_exact(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(first, [1, 3 << 32, 0, 7, 20, 33, 0, 0]);
        assert_eq!(r.len(), 6);
        for metric in ["process", "end_to_end", "sleep_error", "t1_t0", "t3_t2"] {
            assert_eq!(r[metric].count, n as u64);
        }
        assert_eq!(r["timer"].count, n.div_ceil(2) as u64);
        assert_eq!(r["process"].p99, 20);
        assert_eq!(r["t1_t0"].p99, 7);
        assert_eq!(r["t3_t2"].p99, 13);
        assert_eq!(r["sleep_error"].max, n as u64 - 1);
    }

    #[test]
    /// 验证低延迟范围内的 1ns 桶、禁用自动扩容，以及极大数值的独立 max。
    fn nanosecond_bins_and_extreme_values() {
        const EXACT_NS_MAX: u64 = 262_143;
        let mut d = Distribution::new();
        assert!(!d.histogram.is_auto_resize());
        for ns in [0, 1, 80, 242, 713, EXACT_NS_MAX] {
            assert_eq!(d.histogram.equivalent_range(ns), 1);
            d.record(ns);
            assert_eq!(d.histogram.count_at(ns), 1);
        }
        for ns in [EXACT_NS_MAX + 1, 1_000_000_001, u64::MAX] {
            d.record(ns);
        }
        let summary = d.summary();
        assert_eq!(summary.count, 9);
        assert_eq!(summary.max, u64::MAX);
        // 大值按 HDR 桶近似，精确极值由独立的 max 保留。
        assert!(summary.p99 >= 1_000_000_001 && summary.p99 <= summary.max);
    }

    #[test]
    /// 验证 process 对每个请求先求发送加接收，再统计分位数；不能直接相加两组 p99。
    fn process_keeps_per_request_pairing() {
        let mut h = Histograms::new(Clock { hz: 1_000_000_000 });
        h.record(Event {
            kind: Kind::Reply,
            id: 0,
            t: [0, 10, 110, 200, 0, 0],
        });
        h.record(Event {
            kind: Kind::Reply,
            id: 0,
            t: [0, 90, 290, 300, 0, 0],
        });
        let r = h.summaries();
        assert_eq!(r["process"].p99, 100);
        assert_eq!(r["t1_t0"].p99, 90);
        assert_eq!(r["t3_t2"].p99, 90);
        assert_eq!(r["end_to_end"].max, 300);
        assert_eq!(r["timer"].count, 0);
        assert_eq!(r["timer"].p99, 0);
    }
}
