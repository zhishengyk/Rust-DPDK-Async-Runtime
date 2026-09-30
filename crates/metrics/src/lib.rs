use serde::Serialize;
use std::time::{Duration, Instant};

/// Ordered on both sides; the asm memory clobber also prevents compiler motion.
#[inline(always)]
pub fn now() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: x86_64 supports LFENCE/RDTSC; no pointers or memory are accessed.
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

/// Exact below 128 cycles; 64 sub-buckets per power of two above that.
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
    pub process: Histogram,
    pub end_to_end: Histogram,
    pub timer: Histogram,
    pub sleep_error: Histogram,
    pub send: Histogram,
    pub receive: Histogram,
}
impl Histograms {
    pub fn reply(&mut self, t0: u64, t1: u64, t2: u64, t3: u64) {
        self.send.record(t1 - t0);
        self.receive.record(t3 - t2);
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
