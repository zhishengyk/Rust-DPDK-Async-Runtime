//! Background statistics, so histogram insertion and file I/O do not run in the reactor.
use crate::ws_protocol::{Delivery, Market};
use hdrhistogram::Histogram;
use metrics::Clock;
use rtrb::{Producer, RingBuffer};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    sync::mpsc::sync_channel,
    thread::{self, JoinHandle},
};

pub const NAMES: [&str; 13] = [
    "rx_last_to_json",
    "rx_last_to_app",
    "completion_to_json_upper_bound",
    "completion_to_app_upper_bound",
    "rx_first_to_json",
    "rx_first_to_app",
    "rx_to_tcp_record",
    "tls",
    "websocket",
    "json",
    "dispatch",
    "tcp_record_to_app",
    "completion_observation_window",
];
const BATCH: usize = 128;
#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub ticks: [u64; 13],
    pub market: Market,
}
impl Sample {
    pub fn from_delivery(d: Delivery, app: u64) -> Result<Self, String> {
        let r = d.layer.rx;
        let pairs = [
            (d.json_ready, r.last_rx),
            (app, r.last_rx),
            (d.json_ready, r.ready_lower_bound),
            (app, r.ready_lower_bound),
            (d.json_ready, r.first_rx),
            (app, r.first_rx),
            (d.layer.tcp_ready, r.last_rx),
            (d.layer.tls_ready, d.layer.tcp_ready),
            (d.ws_ready, d.layer.tls_ready),
            (d.json_ready, d.ws_ready),
            (app, d.json_ready),
            (app, d.layer.tcp_ready),
            (r.last_rx, r.ready_lower_bound),
        ];
        let mut ticks = [0; 13];
        for (i, (end, start)) in pairs.into_iter().enumerate() {
            ticks[i] = end
                .checked_sub(start)
                .ok_or_else(|| format!("Non-monotonic timestamp for {}", NAMES[i]))?;
        }
        Ok(Self {
            ticks,
            market: d.market,
        })
    }
}
#[derive(Serialize)]
pub struct Summary {
    pub count: u64,
    pub mean: f64,
    pub p50: u64,
    pub p90: u64,
    pub p95: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
}
struct Distribution {
    histogram: Histogram<u64>,
    sum: u128,
    max: u64,
}
impl Distribution {
    fn new() -> Self {
        Self {
            histogram: Histogram::new_with_max(1_000_000_000_000, 4).unwrap(),
            sum: 0,
            max: 0,
        }
    }
    fn record(&mut self, n: u64) -> Result<(), String> {
        self.histogram.record(n).map_err(|e| e.to_string())?;
        self.sum += n as u128;
        self.max = self.max.max(n);
        Ok(())
    }
    fn summary(&self) -> Summary {
        let n = self.histogram.len();
        let q = |p| self.histogram.value_at_quantile(p).min(self.max);
        Summary {
            count: n,
            mean: if n == 0 {
                0.0
            } else {
                self.sum as f64 / n as f64
            },
            p50: q(0.5),
            p90: q(0.9),
            p95: q(0.95),
            p99: q(0.99),
            p999: q(0.999),
            max: self.max,
        }
    }
}
type Report = BTreeMap<&'static str, Summary>;
pub struct Recorder {
    sender: Producer<Sample>,
    batch: [Sample; BATCH],
    len: usize,
    worker: JoinHandle<Result<Report, String>>,
    pub backpressure_batches: u64,
}
impl Recorder {
    pub fn new(clock: Clock, core: u32, csv: Option<String>) -> Result<Self, String> {
        let (sender, mut receiver) = RingBuffer::<Sample>::new(16_384);
        let (ready, started) = sync_channel(0);
        let worker = thread::Builder::new()
            .name("ws-statistics".into())
            .spawn(move || {
                dpdk::pin_thread(core);
                let mut hist: [Distribution; 13] = std::array::from_fn(|_| Distribution::new());
                let mut output = if let Some(path) = csv {
                    Some(BufWriter::new(
                        File::create(path).map_err(|e| e.to_string())?,
                    ))
                } else {
                    None
                };
                if let Some(w) = &mut output {
                    writeln!(w, "event_ms,transaction_ms,id,{}", NAMES.join(","))
                        .map_err(|e| e.to_string())?;
                }
                ready.send(()).map_err(|e| e.to_string())?;
                loop {
                    let closed = receiver.is_abandoned();
                    match receiver.pop() {
                        Ok(sample) => {
                            if let Some(w) = &mut output {
                                write!(
                                    w,
                                    "{},{},{}",
                                    sample.market.event_ms,
                                    sample.market.transaction_ms,
                                    sample.market.id
                                )
                                .map_err(|e| e.to_string())?;
                            }
                            for (i, t) in sample.ticks.into_iter().enumerate() {
                                let ns = clock.ns(t);
                                hist[i].record(ns)?;
                                if let Some(w) = &mut output {
                                    write!(w, ",{ns}").map_err(|e| e.to_string())?;
                                }
                            }
                            if let Some(w) = &mut output {
                                writeln!(w).map_err(|e| e.to_string())?;
                            }
                        }
                        Err(_) if closed => break,
                        Err(_) => std::hint::spin_loop(),
                    }
                }
                if let Some(w) = &mut output {
                    w.flush().map_err(|e| e.to_string())?;
                }
                Ok(NAMES
                    .into_iter()
                    .zip(hist.iter().map(Distribution::summary))
                    .collect())
            })
            .map_err(|e| e.to_string())?;
        if started.recv().is_err() {
            return Err(worker
                .join()
                .unwrap()
                .err()
                .unwrap_or_else(|| "Statistics initialization failed".into()));
        }
        Ok(Self {
            sender,
            batch: [Sample::default(); BATCH],
            len: 0,
            worker,
            backpressure_batches: 0,
        })
    }
    pub fn record(&mut self, s: Sample) -> Result<(), String> {
        self.batch[self.len] = s;
        self.len += 1;
        if self.len == BATCH {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), String> {
        if self.len == 0 {
            return Ok(());
        }
        let samples = &self.batch[..self.len];
        if self.sender.push_entire_slice(samples).is_err() {
            self.backpressure_batches += 1;
            loop {
                if self.sender.is_abandoned() {
                    return Err("Statistics consumer failed".into());
                }
                if self.sender.push_entire_slice(samples).is_ok() {
                    break;
                }
                std::hint::spin_loop();
            }
        }
        self.len = 0;
        Ok(())
    }
    pub fn finish(mut self) -> Result<(Report, u64), String> {
        let flush = self.flush();
        drop(self.sender);
        let report = self
            .worker
            .join()
            .map_err(|_| "Statistics worker panicked")??;
        flush?;
        Ok((report, self.backpressure_batches))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ws_device::Stamp, ws_protocol::LayerStamp};
    #[test]
    fn local_processing_parts_add_up_for_the_same_message_and_bounds_hold() {
        let d = Delivery {
            market: Market::default(),
            layer: LayerStamp {
                rx: Stamp {
                    first_rx: 10,
                    last_rx: 20,
                    ready_lower_bound: 18,
                },
                tcp_ready: 30,
                tls_ready: 40,
            },
            ws_ready: 45,
            json_ready: 50,
        };
        let s = Sample::from_delivery(d, 60).unwrap();
        assert_eq!(s.ticks[1], s.ticks[6..=10].iter().sum::<u64>());
        assert_eq!(s.ticks[3] - s.ticks[1], s.ticks[12]);
        assert_eq!(s.ticks[5], 50);
        assert!(Sample::from_delivery(d, 49).is_err());
    }
}
