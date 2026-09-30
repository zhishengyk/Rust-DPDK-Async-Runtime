//! B：手写 busy-poll 状态机；收发与统计共用 A 的代码，用作 runtime 开销对照。
#![forbid(unsafe_code)]
use metrics::now;
use ping_common::{Sample, Shared, Stamp};

enum State {
    // 每个 session 最多一个在途 request；等待期间保存对应 T0/T1 和超时点。
    Waiting(Stamp),
    Sleeping {
        // 与 A 一样，收到 reply 后仍持有 mbuf，直到 delay 到期才统计并释放。
        sample: Option<Sample>,
        // 首发前仅用于错开相位，不计入 sleep/timer 误差；之后的间隔才计入。
        measured: bool,
    },
    Done,
}
struct Session {
    state: State,
    seq: u16,
    deadline: u64,
}
fn run() -> Result<(), String> {
    let mut io = Shared::open()?;
    let mut sessions: Vec<_> = (0..io.options.sessions)
        .map(|_| Session {
            state: State::Sleeping {
                sample: None,
                measured: false,
            },
            seq: 0,
            deadline: 0,
        })
        .collect();
    io.start();
    for (sid, s) in sessions.iter_mut().enumerate() {
        s.deadline = io.phase(sid);
    }
    let mut next = io.start; // 缓存最早 deadline，未到期时不扫描会话表。
    let mut live = sessions.len();
    while live > 0 {
        let (batch, t2) = io.port.receive();
        for packet in batch {
            if let Some((sid, reply)) = io.dispatch(packet, t2) {
                let s = &mut sessions[sid];
                if let State::Waiting(stamp) = s.state {
                    let t3 = now(); // T3：解析出的 reply 交到 Waiting 状态，先打点再处理。
                    io.accepted(sid);
                    s.state = State::Sleeping {
                        sample: Some(Sample { reply, stamp, t3 }),
                        measured: true,
                    };
                    s.deadline = now() + io.delay;
                    next = next.min(s.deadline);
                }
            }
        }
        let t = now();
        if t >= next {
            // 一次扫描同时处理 reply 超时和 sleep 到期，并重算最早 deadline。
            next = u64::MAX;
            for (sid, s) in sessions.iter_mut().enumerate() {
                if s.deadline <= t {
                    // 临时移出旧状态，取得 sample/mbuf 的唯一所有权；有效分支会写回新状态。
                    match std::mem::replace(&mut s.state, State::Done) {
                        State::Waiting(_) => {
                            // 超时也遵守相同 delay，不立即重发，以保持 A/B 会话行为一致。
                            io.timed_out(sid);
                            s.state = State::Sleeping {
                                sample: None,
                                measured: true,
                            };
                            s.deadline = now() + io.delay;
                        }
                        State::Sleeping { sample, measured } => {
                            if measured {
                                io.hist.sleep_error.record(now() - s.deadline);
                            }
                            if let Some(sample) = sample {
                                io.record(sample);
                            }
                            if now() >= io.end {
                                live -= 1;
                                s.deadline = u64::MAX;
                            } else {
                                if let Some(stamp) =
                                    io.send(sid, s.seq, measured.then_some(s.deadline))
                                {
                                    s.state = State::Waiting(stamp);
                                    s.deadline = stamp.deadline;
                                } else {
                                    s.state = State::Sleeping {
                                        sample: None,
                                        measured: true,
                                    };
                                    s.deadline = now() + io.delay;
                                }
                                s.seq = s.seq.wrapping_add(1);
                            }
                        }
                        State::Done => {}
                    }
                }
                next = next.min(s.deadline);
            }
        }
        // 与 A 相同，必要的驱动维护放在收包和会话处理之后。
        io.maintenance();
    }
    io.finish("B")
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
