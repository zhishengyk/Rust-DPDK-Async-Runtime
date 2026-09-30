#![forbid(unsafe_code)]
use clap::Parser;
use dpdk::{Mbuf, Port};
use metrics::{now, Clock, Histograms};
use serde::Serialize;
use std::net::Ipv4Addr;
use wire::{Network, Packet, TxTemplate};

#[derive(Parser, Debug, Serialize)]
#[command(args_override_self = true)]
pub struct Options {
    #[arg(long)]
    pub delay_us: u64,
    #[arg(long)]
    pub duration_sec: u64,
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(1..=128))]
    pub sessions: u16,
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(8..=1472))]
    pub payload: u16,
    #[arg(long, default_value_t = 10000, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout_us: u64,
    #[arg(long)]
    pub src_ip: Ipv4Addr,
    #[arg(long, default_value = "10.202.8.15")]
    pub peer_ip: Ipv4Addr,
    #[arg(long, default_value = "06:ff:fd:b6:f0:cd", value_parser = parse_mac)]
    pub peer_mac: [u8; 6],
    #[arg(long)]
    pub bdf: String,
    #[arg(long, default_value_t = 2)]
    pub core: usize,
    #[arg(long, default_value = "report.json")]
    pub output: String,
}
fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let parts = s
        .split(':')
        .map(|s| u8::from_str_radix(s, 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    parts.try_into().map_err(|_| "MAC needs six octets".into())
}
#[derive(Default, Serialize)]
pub struct Counters {
    pub tx: u64,
    pub rx: u64,
    pub timeout: u64,
    pub late_reply: u64,
    pub duplicate: u64,
    pub foreign: u64,
    pub arp_replied: u64,
    pub arp_tx_failed: u64,
    pub tx_failed: u64,
    pub alloc_failed: u64,
}
#[derive(Clone, Copy)]
pub struct Stamp {
    pub seq: u16,
    pub t0: u64,
    pub t1: u64,
    pub deadline: u64,
    pub late_seen: bool,
}
pub struct Reply {
    pub mbuf: Mbuf,
    pub t2: u64,
}
pub struct Sample {
    pub reply: Reply,
    pub stamp: Stamp,
    pub t3: u64,
}
#[derive(Serialize)]
pub struct Loss {
    pub session: usize,
    pub seq: u16,
    pub t0: u64,
    pub late: bool,
}
pub struct Shared {
    pub options: Options,
    pub port: Port,
    pub net: Network,
    pub clock: Clock,
    pub templates: Vec<TxTemplate>,
    pub expected: Vec<Option<Stamp>>,
    pub counters: Counters,
    pub hist: Histograms,
    pub losses: Vec<Loss>,
    pub start: u64,
    pub end: u64,
    pub delay: u64,
    pub timeout: u64,
    next_maintenance: u64,
}
impl Shared {
    pub fn open() -> Result<Self, String> {
        let options = Options::parse();
        let port = Port::open(&options.bdf, options.core)?;
        let clock = Clock::calibrate();
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
            hist: Histograms::default(),
            losses: Vec::new(),
            start: 0,
            end,
            delay,
            timeout,
            next_maintenance: 0,
        })
    }
    pub fn start(&mut self) {
        self.start = now();
        self.end += self.start;
        self.next_maintenance = self.start + self.clock.us(1000);
    }
    pub fn phase(&self, sid: usize) -> u64 {
        self.start + self.delay * sid as u64 / self.options.sessions as u64
    }
    /// Both clients call exactly this send path; T0 precedes alloc/copy/patch/TX.
    #[inline]
    pub fn send(&mut self, sid: usize, seq: u16, sleep_deadline: Option<u64>) -> Option<Stamp> {
        let t0 = now();
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
            deadline: t1 + self.timeout,
            late_seen: false,
        };
        self.expected[sid] = Some(stamp);
        self.counters.tx += 1;
        if let Some(deadline) = sleep_deadline {
            self.hist.timer.record(t0 - deadline);
        }
        Some(stamp)
    }
    /// Common parsing, demultiplexing and stale-reply validation, before T3.
    pub fn dispatch(&mut self, mut mbuf: Mbuf, t2: u64) -> Option<(usize, Reply)> {
        match wire::classify(mbuf.data(), self.net, self.options.sessions as usize) {
            Packet::Arp => {
                wire::arp_reply(mbuf.data_mut(), self.net);
                if self.port.send(mbuf).is_ok() {
                    self.counters.arp_replied += 1;
                } else {
                    self.counters.arp_tx_failed += 1;
                }
            }
            Packet::Other => self.counters.foreign += 1,
            Packet::Reply { session, seq, t0 } => {
                if let Some(s) = self.expected[session].as_mut() {
                    if s.seq == seq && s.t0 == t0 {
                        if t2 <= s.deadline {
                            return Some((session, Reply { mbuf, t2 }));
                        }
                        if !s.late_seen {
                            s.late_seen = true;
                            self.counters.late_reply += 1;
                        } else {
                            self.counters.duplicate += 1;
                        }
                        return None;
                    }
                }
                if let Some(loss) = self
                    .losses
                    .iter_mut()
                    .rev()
                    .find(|l| l.session == session && l.seq == seq && l.t0 == t0 && !l.late)
                {
                    loss.late = true;
                    self.counters.late_reply += 1;
                } else {
                    self.counters.duplicate += 1;
                }
            }
        }
        None
    }
    pub fn accepted(&mut self, sid: usize) {
        self.expected[sid] = None;
        self.counters.rx += 1;
    }
    pub fn timed_out(&mut self, sid: usize) {
        let stamp = self.expected[sid].take().unwrap();
        self.counters.timeout += 1;
        self.losses.push(Loss {
            session: sid,
            seq: stamp.seq,
            t0: stamp.t0,
            late: stamp.late_seen,
        });
    }
    pub fn record(&mut self, sample: Sample) {
        self.hist
            .reply(sample.stamp.t0, sample.stamp.t1, sample.reply.t2, sample.t3);
        drop(sample.reply.mbuf);
    }
    pub fn maintenance(&mut self) {
        let t = now();
        if t >= self.next_maintenance {
            self.port.maintenance();
            self.next_maintenance = t + self.clock.us(1000);
        }
    }
    pub fn finish(mut self, label: &str) -> Result<(), String> {
        let measured_end = now();
        // Tasks have finished their last timeout and sleep. Account late packets too.
        let drain_end = now() + self.clock.us(50_000);
        while now() < drain_end {
            let (batch, t2) = self.port.receive();
            for m in batch {
                self.dispatch(m, t2);
            }
            self.maintenance();
        }
        let nic = self.port.stats();
        let (initial, final_count) = self.port.finish(); // Stop/close returns RX descriptors and pending TX.
        let missing = self.losses.iter().filter(|l| !l.late).count();
        let latency = self.hist.summaries(self.clock);
        let accounted = self.counters.tx == self.counters.rx + self.counters.timeout;
        let report = serde_json::json!({
            "client": label, "options": self.options, "tsc_hz": self.clock.hz,
            "elapsed_sec": (measured_end-self.start) as f64 / self.clock.hz as f64,
            "units": "ns", "histogram_relative_error_max": 0.015625,
            "latency": latency, "counters": self.counters, "losses": self.losses,
            "missing": missing, "accounted": accounted,
            "mempool": {"initial": initial, "final": final_count, "leak_free": initial == final_count},
            "nic": {"rx": nic[0], "tx": nic[1], "missed": nic[2], "rx_errors": nic[3], "tx_errors": nic[4], "rx_nombuf": nic[5]}
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
