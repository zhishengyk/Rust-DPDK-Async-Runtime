#![forbid(unsafe_code)]
use metrics::now;
use ping_common::{Sample, Shared, Stamp};

enum State {
    Waiting(Stamp),
    Sleeping {
        sample: Option<Sample>,
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
    let mut next = io.start;
    let mut live = sessions.len();
    while live > 0 {
        let (batch, t2) = io.port.receive();
        for packet in batch {
            if let Some((sid, reply)) = io.dispatch(packet, t2) {
                let s = &mut sessions[sid];
                if let State::Waiting(stamp) = s.state {
                    let t3 = now(); // T3: parsed reply reaches its session state machine.
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
            next = u64::MAX;
            for (sid, s) in sessions.iter_mut().enumerate() {
                if s.deadline <= t {
                    match std::mem::replace(&mut s.state, State::Done) {
                        State::Waiting(_) => {
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
