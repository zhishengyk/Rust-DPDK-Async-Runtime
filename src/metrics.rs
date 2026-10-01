//! A/B 共用计时与统计；保留 TSC 打点，只在统计端换算 ns 并写入 HDR 直方图。
use hdrhistogram::Histogram;
use rtrb::{Producer, RingBuffer};
use serde::Serialize;
use std::{
    arch::x86_64::{_mm_lfence, _rdtsc},
    collections::BTreeMap,
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
pub struct Clock {
    /// EAL 初始化后由 DPDK 的 rte_get_tsc_hz() 提供。
    pub hz: u64,
}
impl Clock {
    pub fn us(self, us: u64) -> u64 {
        ((us as u128 * self.hz as u128) / 1_000_000) as u64
    }
    pub fn ns(self, ticks: u64) -> u64 {
        ((ticks as u128 * 1_000_000_000) / self.hz as u128) as u64
    }
}

pub const SIGNIFICANT_FIGURES: u8 = 5;
pub const EXACT_NS_MAX: u64 = 262_143;
pub const COLLECTION_MODE: &str = "background";
const BATCH_SIZE: usize = 256;
const QUEUE_BATCHES: usize = 64;

struct Distribution {
    histogram: Histogram<u64>,
    max: u64,
}
impl Distribution {
    fn new() -> Self {
        // 预建覆盖 u64 范围的桶，禁止自动扩容；0..=262143ns 每个整数单独一个桶。
        Self {
            histogram: Histogram::new_with_max(u64::MAX, SIGNIFICANT_FIGURES).unwrap(),
            max: 0,
        }
    }
    fn record(&mut self, ns: u64) {
        self.histogram.record(ns).unwrap();
        self.max = self.max.max(ns);
    }
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
pub struct Summary {
    pub count: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub p999: u64,
    pub p9999: u64,
    pub max: u64,
}

#[derive(Clone, Copy)]
enum Event {
    Reply {
        send: u64,
        receive: u64,
        end_to_end: u64,
    },
    Timer(u64),
    SleepError(u64),
}

struct Histograms {
    clock: Clock,
    // process、end_to_end、timer、sleep_error、send、receive。
    values: [Distribution; 6],
}
impl Histograms {
    fn new(clock: Clock) -> Self {
        Self {
            clock,
            values: std::array::from_fn(|_| Distribution::new()),
        }
    }
    fn record(&mut self, event: Event) {
        match event {
            Event::Reply {
                send,
                receive,
                end_to_end,
            } => {
                // 每个请求先按 ticks 相加，再换算 ns；不能把两个分位数相加。
                self.values[0].record(self.clock.ns(send + receive));
                self.values[1].record(self.clock.ns(end_to_end));
                self.values[4].record(self.clock.ns(send));
                self.values[5].record(self.clock.ns(receive));
            }
            Event::Timer(ticks) => self.values[2].record(self.clock.ns(ticks)),
            Event::SleepError(ticks) => self.values[3].record(self.clock.ns(ticks)),
        }
    }
    fn summaries(&self) -> BTreeMap<&'static str, Summary> {
        [
            "process",
            "end_to_end",
            "timer",
            "sleep_error",
            "send",
            "receive",
        ]
        .into_iter()
        .zip(&self.values)
        .map(|(name, distribution)| (name, distribution.summary()))
        .collect()
    }
}

struct Batch {
    events: [Event; BATCH_SIZE],
    len: usize,
}
impl Batch {
    fn new() -> Self {
        Self {
            events: [Event::Timer(0); BATCH_SIZE],
            len: 0,
        }
    }
}

/// 单核收发线程只追加数值；每 256 条交给统计线程，不跨线程传 mbuf。
/// 有界队列满时背压并计数，不能静默丢样本。正常运行应检查 backpressure_batches=0。
pub struct Recorder {
    batch: Batch,
    sender: Producer<Event>,
    worker: JoinHandle<BTreeMap<&'static str, Summary>>,
    backpressure_batches: u64,
}
impl Recorder {
    pub fn new(clock: Clock, worker_init: impl FnOnce() + Send + 'static) -> Self {
        let (sender, mut receiver) = RingBuffer::<Event>::new(BATCH_SIZE * QUEUE_BATCHES);
        let (ready, started) = sync_channel(0);
        let worker = thread::Builder::new()
            .name("ping-statistics".into())
            .spawn(move || {
                worker_init(); // EAL 主线程已绑核；子线程必须改绑，避免继承收发核的 affinity。
                let mut histograms = Histograms::new(clock);
                ready.send(()).unwrap();
                loop {
                    // 先看发送端是否关闭，再读队列，避免退出检查与最后一批发布竞争而丢尾包。
                    let closed = receiver.is_abandoned();
                    match receiver.pop() {
                        Ok(event) => histograms.record(event),
                        Err(_) if closed => break,
                        Err(_) => std::hint::spin_loop(),
                    }
                }
                histograms.summaries()
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
            backpressure_batches: 0,
        }
    }
    fn record(&mut self, event: Event) {
        self.batch.events[self.batch.len] = event;
        self.batch.len += 1;
        if self.batch.len == BATCH_SIZE {
            self.flush();
        }
    }
    fn flush(&mut self) {
        if self.batch.len == 0 {
            return;
        }
        let events = &self.batch.events[..self.batch.len];
        // 一次复制、一批发布；复用本地数组，不逐批清零，不唤醒休眠的 OS 线程。
        if self.sender.push_entire_slice(events).is_err() {
            self.backpressure_batches += 1;
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
    pub fn reply(&mut self, t0: u64, t1: u64, t2: u64, t3: u64) {
        self.record(Event::Reply {
            send: t1 - t0,
            receive: t3 - t2,
            end_to_end: t3 - t0,
        });
    }
    pub fn timer(&mut self, ticks: u64) {
        self.record(Event::Timer(ticks));
    }
    pub fn sleep_error(&mut self, ticks: u64) {
        self.record(Event::SleepError(ticks));
    }
    pub fn finish(mut self) -> (BTreeMap<&'static str, Summary>, u64) {
        self.flush(); // 最后一批不足 256 条也必须提交。
        drop(self.sender); // receiver 排空后退出，再汇总；没有后台线程悬挂。
        let summaries = self.worker.join().expect("statistics thread panicked");
        (summaries, self.backpressure_batches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_drains_full_and_partial_batches_without_losing_samples() {
        let mut recorder = Recorder::new(Clock { hz: 1_000_000_000 }, || {});
        let n = BATCH_SIZE * 2 + 7;
        for i in 0..n {
            recorder.reply(0, 7, 20, 33);
            recorder.sleep_error(i as u64);
            if i % 2 == 0 {
                recorder.timer(i as u64);
            }
        }
        let (r, _) = recorder.finish();
        for metric in ["send", "receive", "process", "end_to_end", "sleep_error"] {
            assert_eq!(r[metric].count, n as u64);
        }
        assert_eq!(r["timer"].count, n.div_ceil(2) as u64);
        assert_eq!(r["process"].p99, 20);
        assert_eq!(r["sleep_error"].max, n as u64 - 1);
    }

    #[test]
    fn nanosecond_bins_and_extreme_values() {
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
    fn process_keeps_per_request_pairing() {
        let mut h = Histograms::new(Clock { hz: 1_000_000_000 });
        h.record(Event::Reply {
            send: 10,
            receive: 90,
            end_to_end: 200,
        });
        h.record(Event::Reply {
            send: 90,
            receive: 10,
            end_to_end: 300,
        });
        let r = h.summaries();
        assert_eq!(r["process"].p99, 100);
        assert_eq!(r["send"].p99 + r["receive"].p99, 180);
        assert_eq!(r["end_to_end"].max, 300);
        assert_eq!(r["timer"].count, 0);
        assert_eq!(r["timer"].p99, 0);
    }
}
