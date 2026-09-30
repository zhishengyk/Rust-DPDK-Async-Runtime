//! 共享计时与直方图：热路径记录 TSC ticks，结束后统一换算为纳秒并计算分位数。
use serde::Serialize;
use std::time::{Duration, Instant};

/// 读取本地 CPU 的 TSC；前后 LFENCE 约束执行顺序，asm 的内存副作用约束编译器移动。
/// 与 C shim 使用相同序列，不能通过去掉 fence 来降低测得的延迟。
#[inline(always)]
pub fn now() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: 目标 x86_64 支持 LFENCE/RDTSC；此汇编不访问指针或内存。
    unsafe {
        std::arch::asm!("lfence", "rdtsc", "lfence", out("eax") lo, out("edx") hi,
            options(nostack, preserves_flags));
    }
    ((hi as u64) << 32) | lo as u64
}

#[derive(Clone, Copy)]
pub struct Clock {
    pub hz: u64,
}
impl Clock {
    pub fn calibrate() -> Self {
        // 用单调时钟校准 TSC 频率，仅启动时执行；TSC ticks 不等同于 CPU 当前主频周期。
        let start = Instant::now();
        let a = now();
        std::thread::sleep(Duration::from_millis(500));
        let b = now();
        Self {
            hz: ((b - a) as f64 / start.elapsed().as_secs_f64()).round() as u64,
        }
    }
    pub fn us(self, us: u64) -> u64 {
        ((us as u128 * self.hz as u128) / 1_000_000) as u64
    }
    pub fn ns(self, ticks: u64) -> u64 {
        ((ticks as u128 * 1_000_000_000) / self.hz as u128) as u64
    }
}

/// 小于 128 ticks 时精确记录；更大值按 2 的幂划区间，每区间 64 桶。
/// 分位数取桶上界，误差约不超过 1.6%；max 单独保存原始最大值，不受桶宽影响。
pub struct Histogram {
    bins: Box<[u64; 4096]>,
    count: u64,
    max: u64,
}
impl Default for Histogram {
    fn default() -> Self {
        Self {
            bins: Box::new([0; 4096]),
            count: 0,
            max: 0,
        }
    }
}
impl Histogram {
    fn index(v: u64) -> usize {
        if v < 128 {
            return v as usize;
        }
        let exponent = 63 - v.leading_zeros() as usize;
        exponent * 64 + ((v >> (exponent - 6)) as usize - 64)
    }
    fn upper(index: usize) -> u64 {
        if index < 128 {
            return index as u64;
        }
        let exponent = index / 64;
        (((index % 64 + 65) as u128) << (exponent - 6))
            .saturating_sub(1)
            .min(u64::MAX as u128) as u64
    }
    #[inline]
    pub fn record(&mut self, ticks: u64) {
        self.bins[Self::index(ticks)] += 1;
        self.count += 1;
        self.max = self.max.max(ticks);
    }
    fn percentile(&self, numerator: u64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        // 用十万分位表示 p99.99，避免热路径浮点计算或保存全部样本。
        let target = (self.count * numerator).div_ceil(100_000);
        let mut sum = 0;
        for (i, n) in self.bins.iter().enumerate() {
            sum += n;
            if sum >= target {
                return Self::upper(i).min(self.max);
            }
        }
        self.max
    }
    pub fn summary(&self, clock: Clock) -> Summary {
        Summary {
            count: self.count,
            p50: clock.ns(self.percentile(50_000)),
            p90: clock.ns(self.percentile(90_000)),
            p99: clock.ns(self.percentile(99_000)),
            p999: clock.ns(self.percentile(99_900)),
            p9999: clock.ns(self.percentile(99_990)),
            max: clock.ns(self.max),
        }
    }
}
#[derive(Serialize)]
pub struct Summary {
    pub count: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub p999: u64,
    pub p9999: u64,
    pub max: u64,
}
#[derive(Default)]
pub struct Histograms {
    /// 排名使用的进程内耗时：每个样本的发送段与接收段之和。
    pub process: Histogram,
    /// T3 − T0，包含本地收发、网络和对端处理，不含 reply 后的 sleep。
    pub end_to_end: Histogram,
    /// sleep deadline → 下一次 T0，单独报告，不进入 process。
    pub timer: Histogram,
    /// sleep deadline → sleep 恢复后的打点，比 timer 少了后续统计和下一轮发送前的工作。
    pub sleep_error: Histogram,
    pub send: Histogram,
    pub receive: Histogram,
}
impl Histograms {
    pub fn reply(&mut self, t0: u64, t1: u64, t2: u64, t3: u64) {
        self.send.record(t1 - t0);
        self.receive.record(t3 - t2);
        // 先逐样本相加再求分位数，不能用 send.p99 + receive.p99 替代 process.p99。
        self.process.record(t1 - t0 + t3 - t2);
        self.end_to_end.record(t3 - t0);
    }
    pub fn summaries(&self, clock: Clock) -> std::collections::BTreeMap<&'static str, Summary> {
        [
            ("process", &self.process),
            ("end_to_end", &self.end_to_end),
            ("timer", &self.timer),
            ("sleep_error", &self.sleep_error),
            ("send", &self.send),
            ("receive", &self.receive),
        ]
        .into_iter()
        .map(|(name, hist)| (name, hist.summary(clock)))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn histogram_bounds_and_quantiles() {
        for v in (0..20000).chain([u32::MAX as u64, u64::MAX]) {
            let upper = Histogram::upper(Histogram::index(v));
            assert!(upper >= v);
            assert!(upper as u128 <= v as u128 + v as u128 / 64 + 1);
        }
        let mut h = Histogram::default();
        for v in 1..=100 {
            h.record(v);
        }
        assert_eq!(h.percentile(50_000), 50);
        assert_eq!(h.percentile(99_000), 99);
    }
}
