//! B：手写 busy-poll 状态机；收发与统计共用 A 的代码，用作 runtime 开销对照。
#![forbid(unsafe_code)]
use metrics::now;
use ping_common::{Sample, Shared, Stamp};

/// 裸循环中一个 session 的当前阶段；用枚举保存该阶段需要持有的数据。
enum State {
    /// 等待 reply；唯一字段保存本次成功 TX 的序号、时间戳和超时点。
    Waiting(Stamp),
    /// 首发错峰或回复/超时后的间隔等待；成功回复的 mbuf 在此阶段继续存活。
    Sleeping {
        /// 上一条成功请求的时间戳和 RX mbuf；首发、超时或发送失败时为 None。
        /// 与 A 一样持有 mbuf 到 delay 到期，再统计并释放。
        sample: Option<Sample>,
        /// 是否来自真实请求后的 sleep；首发错峰为 false，不记入 timer/sleep_error。
        measured: bool,
    },
    /// 会话已停止；移出旧状态处理时也暂用此值占位。
    Done,
}
/// B 的会话状态表项；索引与共用 I/O 中的 session 编号一致。
struct Session {
    /// 当前等待阶段及其独占的请求或回复数据。
    state: State,
    /// 下一次发送使用的 16 位 ICMP 序号；递增溢出时回绕。
    seq: u16,
    /// 当前阶段的绝对 TSC 截止时间；Waiting 是超时点，Sleeping 是唤醒点，Done 用 MAX。
    deadline: u64,
}
/// 运行 B 的单线程 busy-poll 状态机：先交付整批回复，再处理超时和 sleep 到期。
/// 使用与 A 相同的 I/O、delay、计时和统计；不经过 executor、Waker 或 async task。
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
                                io.metrics.sleep_error(now() - s.deadline);
                            }
                            if let Some(sample) = sample {
                                io.record(sample);
                            }
                            if now() >= io.end {
                                live -= 1;
                                s.deadline = u64::MAX;
                            } else {
                                let t0 = now(); // T0：已判定该 session 应当发送。
                                if let Some(stamp) =
                                    io.send(sid, s.seq, measured.then_some(s.deadline), t0)
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
/// B 的进程入口；运行失败时打印原因并以非零退出码结束。
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
