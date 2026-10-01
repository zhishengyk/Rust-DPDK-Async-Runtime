//! A：每个 session 是独立 async task，收包 reactor 通过 Slot + Waker 交付 reply。
#![forbid(unsafe_code)]
use metrics::now;
use ping_common::{Reply, Sample, Shared, Stamp};
use std::{cell::RefCell, rc::Rc};

async fn send(io: &RefCell<Shared>, sid: usize, seq: u16, deadline: Option<u64>) -> Option<Stamp> {
    // 发送当前是同步完成的 async 接口；首次 poll 即完成，不额外制造一次调度。
    io.borrow_mut().send(sid, seq, deadline)
}
async fn session(
    sid: usize,
    mut handle: rt::Handle,
    io: Rc<RefCell<Shared>>,
    slot: Rc<rt::Slot<Reply>>,
) {
    // 首发按 session 错开，与 B 相同，避免所有任务在启动时同时发包。
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
                    let t3 = now(); // T3：等待 reply 的 await 恢复后，先打点再做计数。
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
        // sample 持有 RX mbuf 跨越 sleep；这是任务要求，也验证 future 的所有权保持。
        let deadline = handle.sleep(delay).await;
        let resumed = now();
        {
            let mut io = io.borrow_mut();
            io.metrics.sleep_error(resumed - deadline);
            // 四个打点已保存，统计与 mbuf 释放延后到 sleep 后，不进入本次 T0 → T3。
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
            // 整批共用 rx_burst 返回时的 T2。先分发整批，再由 runtime poll 被唤醒的任务。
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
    // 先释放 task/slot 持有的 mbuf，再停端口、核对内存池，避免把存活所有者算作泄漏。
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
