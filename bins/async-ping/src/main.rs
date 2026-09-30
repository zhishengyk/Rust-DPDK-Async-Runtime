#![forbid(unsafe_code)]
use metrics::now;
use ping_common::{Reply, Sample, Shared, Stamp};
use std::{cell::RefCell, rc::Rc};

async fn send(io: &RefCell<Shared>, sid: usize, seq: u16, deadline: Option<u64>) -> Option<Stamp> {
    io.borrow_mut().send(sid, seq, deadline)
}
async fn session(
    sid: usize,
    mut handle: rt::Handle,
    io: Rc<RefCell<Shared>>,
    slot: Rc<rt::Slot<Reply>>,
) {
    let phase = io.borrow().phase(sid);
    handle.sleep_until(phase).await;
    let mut seq = 0u16;
    let mut previous_deadline = None;
    loop {
        if now() >= io.borrow().end {
            break;
        }
        let stamp = send(&io, sid, seq, previous_deadline).await;
        let sample = if let Some(stamp) = stamp {
            match slot.recv(&mut handle, stamp.deadline).await {
                Ok(reply) => {
                    let t3 = now(); // First instruction after wait_reply resumes.
                    io.borrow_mut().accepted(sid);
                    Some(Sample { reply, stamp, t3 })
                }
                Err(_) => {
                    io.borrow_mut().timed_out(sid);
                    None
                }
            }
        } else {
            None
        };
        let delay = io.borrow().delay;
        let deadline = handle.sleep(delay).await; // Own the RX mbuf across this await.
        let resumed = now();
        {
            let mut io = io.borrow_mut();
            io.hist.sleep_error.record(resumed - deadline);
            if let Some(sample) = sample {
                io.record(sample);
            }
        }
        previous_deadline = Some(deadline);
        seq = seq.wrapping_add(1);
    }
}
fn run() -> Result<(), String> {
    let io = Rc::new(RefCell::new(Shared::open()?));
    let n = io.borrow().options.sessions as usize;
    let slots: Vec<_> = (0..n).map(|_| Rc::new(rt::Slot::default())).collect();
    let mut runtime = rt::Runtime::new(n, now);
    for (sid, slot) in slots.iter().enumerate() {
        let task_io = io.clone();
        let slot = slot.clone();
        runtime.spawn(move |handle| session(sid, handle, task_io, slot));
    }
    io.borrow_mut().start();
    runtime.run_with_maintenance(
        || {
            let mut io = io.borrow_mut();
            let (batch, t2) = io.port.receive();
            for packet in batch {
                if let Some((sid, reply)) = io.dispatch(packet, t2) {
                    if slots[sid].deliver(reply).is_err() {
                        io.counters.duplicate += 1;
                    }
                }
            }
        },
        || io.borrow_mut().maintenance(),
    );
    drop(runtime);
    drop(slots);
    Rc::try_unwrap(io).ok().unwrap().into_inner().finish("A")
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
