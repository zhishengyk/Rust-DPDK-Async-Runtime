// 诊断程序：链接项目实际 metrics::now / Clock / Recorder，不访问网卡。
use metrics::{now, Clock, Recorder};
use serde_json::{json, Value};

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}
fn probe(name: &str, read: fn() -> u64, clock: Clock, varied: bool) -> Value {
    const N: usize = 1_000_000;
    let mut stamps = vec![0u64; N + 1];
    // 先触碰内存，再连续读取；采集期间不做换算、入桶或终端输出。
    for s in &mut stamps {
        *s = std::hint::black_box(1);
    }
    for _ in 0..10_000 {
        std::hint::black_box(read());
    }
    let mut work = 1u64;
    for (i, s) in stamps.iter_mut().enumerate() {
        if varied {
            for _ in 0..i % 64 {
                work = std::hint::black_box(work.wrapping_mul(6364136223846793005).wrapping_add(1));
            }
        }
        *s = read();
    }
    std::hint::black_box(work);
    let deltas: Vec<u64> = stamps.windows(2).map(|w| w[1] - w[0]).collect();
    let mut values = deltas.clone();
    values.sort_unstable();
    values.dedup();
    let g = deltas.iter().fold(0, |g, &x| gcd(g, x));
    json!({"mode": name, "samples": N, "delta_gcd_ticks": g,
        "delta_gcd_ns": clock.ns(g), "minimum_delta_ticks": deltas.iter().min(),
        "first_16_stamps_ticks": &stamps[..16], "first_16_deltas_ticks": &deltas[..16],
        "first_16_deltas_ns": deltas[..16].iter().map(|&x| clock.ns(x)).collect::<Vec<_>>(),
        "smallest_distinct_deltas_ticks": &values[..values.len().min(12)],
        "deltas_not_multiple_of_26": deltas.iter().filter(|x| **x % 26 != 0).count(),
        "converted_ns_not_multiple_of_10": deltas.iter().filter(|&&x| clock.ns(x) % 10 != 0).count()})
}
// 原版的计时汇编，用来区分 intrinsic 更换与已有时钟读数步进。
fn legacy_now() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        std::arch::asm!("lfence", "rdtsc", "lfence", out("eax") lo, out("edx") hi,
            options(nostack, preserves_flags));
    }
    ((hi as u64) << 32) | lo as u64
}
fn raw_rdtsc() -> u64 {
    unsafe { std::arch::x86_64::_rdtsc() }
}
fn main() {
    // 频率取自本轮 DPDK 实测 JSON；不启动 EAL 或占用端口。
    let clock = Clock { hz: 2_600_000_000 };
    let raw = probe("raw_rdtsc", raw_rdtsc, clock, false);
    let fenced = probe("project_metrics_now", now, clock, false);
    let varied = probe("project_now_varied_work", now, clock, true);
    let legacy = probe("legacy_asm_varied_work", legacy_now, clock, true);
    // 人工构造尾数 1..9 的 ns，经实际换算/后台入桶/分位数/JSON 链路验证。
    let synthetic: Vec<_> = (701u64..=709)
        .map(|ns| {
            let ticks = (ns * clock.hz).div_ceil(1_000_000_000);
            let mut recorder = Recorder::new(clock, || {});
            recorder.reply(0, ticks, ticks, ticks);
            let (summary, _) = recorder.finish();
            assert_eq!(summary["process"].p99, ns);
            json!({"input_ticks": ticks, "expected_ns": ns,
            "process": summary["process"]})
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"tsc_hz": clock.hz,
        "measurements": [raw, fenced, varied, legacy], "synthetic_non_round_ns": synthetic}))
        .unwrap()
    );
}
