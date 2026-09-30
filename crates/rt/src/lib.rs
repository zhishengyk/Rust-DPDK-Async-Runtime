//! Single-threaded poll-mode executor. Clock and reactor are supplied by the caller.
//! Tasks are allocated at spawn; no allocation, locking or atomic RMW on local wake.
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};
const CAP: usize = 128;
type Task = Pin<Box<dyn Future<Output = ()>>>;

struct Ready {
    ring: [usize; CAP],
    head: usize,
    len: usize,
    queued: [bool; CAP],
}
impl Ready {
    fn new() -> Self {
        Self {
            ring: [0; CAP],
            head: 0,
            len: 0,
            queued: [false; CAP],
        }
    }
    fn push(&mut self, id: usize) {
        if !self.queued[id] {
            self.queued[id] = true;
            self.ring[(self.head + self.len) % CAP] = id;
            self.len += 1;
        }
    }
    fn pop(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let id = self.ring[self.head];
        self.head = (self.head + 1) % CAP;
        self.len -= 1;
        self.queued[id] = false;
        Some(id)
    }
}
struct Foreign {
    bits: [AtomicU64; 2],
}
type Registry = HashMap<usize, (Weak<Foreign>, usize)>;
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}
#[derive(Clone)]
struct Home {
    base: usize,
    ready: Rc<RefCell<Ready>>,
}
thread_local! { static HOME: RefCell<Option<Home>> = const { RefCell::new(None) }; }
struct HomeGuard(Option<Home>);
impl Drop for HomeGuard {
    fn drop(&mut self) {
        HOME.with(|h| *h.borrow_mut() = self.0.take());
    }
}

fn wake(token: usize) {
    let local = HOME
        .try_with(|home| {
            let home = home.borrow();
            if let Some(h) = home
                .as_ref()
                .filter(|h| (h.base..h.base + CAP).contains(&token))
            {
                h.ready.borrow_mut().push(token - h.base);
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if !local {
        let reg = registry().lock().unwrap();
        if let Some((f, id)) = reg.get(&token) {
            if let Some(f) = f.upgrade() {
                f.bits[id / 64].fetch_or(1 << (id % 64), Ordering::Release);
            }
        }
    }
}
// SAFETY: data is an integer identity, never dereferenced. IDs are never reused.
// Local access is TLS-only; foreign access uses a synchronized registry and bitset.
// A stale Waker (including one that outlives Runtime) is therefore a harmless no-op.
unsafe fn clone_raw(p: *const ()) -> RawWaker {
    RawWaker::new(p, &VTABLE)
}
unsafe fn wake_raw(p: *const ()) {
    wake(p.addr());
}
unsafe fn drop_raw(_: *const ()) {}
static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_raw, wake_raw, wake_raw, drop_raw);
fn waker(token: usize) -> Waker {
    let data = std::ptr::without_provenance(token);
    unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) }
}

struct TimerSlot {
    deadline: Cell<u64>,
    waker: Waker,
}
struct Timers {
    slots: Vec<TimerSlot>,
    next: Cell<u64>,
}
impl Timers {
    fn arm(&self, id: usize, deadline: u64) {
        self.slots[id].deadline.set(deadline);
        self.next.set(self.next.get().min(deadline));
    }
    fn disarm(&self, id: usize) {
        self.slots[id].deadline.set(u64::MAX);
    }
    fn fire(&self, now: u64) {
        if now < self.next.get() {
            return;
        }
        let mut next = u64::MAX;
        for slot in &self.slots {
            let d = slot.deadline.get();
            if d != u64::MAX && d <= now {
                slot.deadline.set(u64::MAX);
                slot.waker.wake_by_ref();
            } else {
                next = next.min(d);
            }
        }
        self.next.set(next);
    }
}

pub struct Runtime {
    base: usize,
    ready: Rc<RefCell<Ready>>,
    foreign: Arc<Foreign>,
    timers: Rc<Timers>,
    tasks: Vec<Option<Task>>,
    clock: fn() -> u64,
}
impl Runtime {
    pub fn new(capacity: usize, clock: fn() -> u64) -> Self {
        assert!((1..=CAP).contains(&capacity));
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let base = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(CAP))
            .expect("waker IDs exhausted");
        let foreign = Arc::new(Foreign {
            bits: [AtomicU64::new(0), AtomicU64::new(0)],
        });
        let mut reg = registry().lock().unwrap();
        for id in 0..capacity {
            reg.insert(base + id, (Arc::downgrade(&foreign), id));
        }
        drop(reg);
        let slots = (0..capacity)
            .map(|id| TimerSlot {
                deadline: Cell::new(u64::MAX),
                waker: waker(base + id),
            })
            .collect();
        Self {
            base,
            ready: Rc::new(RefCell::new(Ready::new())),
            foreign,
            timers: Rc::new(Timers {
                slots,
                next: Cell::new(u64::MAX),
            }),
            tasks: Vec::with_capacity(capacity),
            clock,
        }
    }
    /// Fixed slots, spawned before run. Task IDs are not recycled within a runtime.
    pub fn spawn<F: Future<Output = ()> + 'static>(&mut self, make: impl FnOnce(Handle) -> F) {
        let id = self.tasks.len();
        assert!(id < self.timers.slots.len());
        let handle = Handle {
            id,
            timers: self.timers.clone(),
            clock: self.clock,
        };
        self.tasks.push(Some(Box::pin(make(handle))));
        self.ready.borrow_mut().push(id);
    }
    fn poll_ready(&mut self) -> usize {
        let mut completed = 0;
        // Bound each phase so self-waking tasks cannot starve I/O or timers.
        for _ in 0..CAP {
            let Some(id) = self.ready.borrow_mut().pop() else {
                break;
            };
            let Some(task) = self.tasks.get_mut(id).and_then(Option::as_mut) else {
                continue;
            };
            let w = &self.timers.slots[id].waker;
            if task.as_mut().poll(&mut Context::from_waker(w)).is_ready() {
                self.tasks[id] = None;
                self.timers.disarm(id);
                completed += 1;
            }
        }
        completed
    }
    pub fn run(&mut self, reactor: impl FnMut()) {
        self.run_with_maintenance(reactor, || {});
    }
    /// Run background work after both reply and timer wakeups have been polled.
    pub fn run_with_maintenance(
        &mut self,
        mut reactor: impl FnMut(),
        mut maintenance: impl FnMut(),
    ) {
        let prev = HOME.with(|h| {
            h.replace(Some(Home {
                base: self.base,
                ready: self.ready.clone(),
            }))
        });
        let _guard = HomeGuard(prev);
        let mut live = self.tasks.iter().filter(|f| f.is_some()).count();
        while live != 0 {
            reactor();
            // Deliver ready replies before checking unrelated timer/foreign wakes.
            live -= self.poll_ready();
            self.timers.fire((self.clock)());
            for (word, bits) in self.foreign.bits.iter().enumerate() {
                if bits.load(Ordering::Relaxed) != 0 {
                    let mut value = bits.swap(0, Ordering::Acquire);
                    while value != 0 {
                        let bit = value.trailing_zeros() as usize;
                        self.ready.borrow_mut().push(word * 64 + bit);
                        value &= value - 1;
                    }
                }
            }
            live -= self.poll_ready();
            maintenance();
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let mut reg = registry().lock().unwrap();
        for id in 0..self.timers.slots.len() {
            reg.remove(&(self.base + id));
        }
    }
}

/// Unique per task: &mut borrowing enforces one pending timer per task.
pub struct Handle {
    id: usize,
    timers: Rc<Timers>,
    clock: fn() -> u64,
}
impl Handle {
    pub fn now(&self) -> u64 {
        (self.clock)()
    }
    pub fn sleep_until(&mut self, deadline: u64) -> Sleep<'_> {
        Sleep {
            handle: self,
            deadline,
            armed: false,
        }
    }
    pub fn sleep(&mut self, ticks: u64) -> Sleep<'_> {
        self.sleep_until(self.now() + ticks)
    }
}
pub struct Sleep<'a> {
    handle: &'a mut Handle,
    deadline: u64,
    armed: bool,
}
impl Future for Sleep<'_> {
    type Output = u64;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u64> {
        if self.handle.now() >= self.deadline {
            self.handle.timers.disarm(self.handle.id);
            Poll::Ready(self.deadline)
        } else {
            if !self.armed {
                self.handle.timers.arm(self.handle.id, self.deadline);
                self.armed = true;
            }
            Poll::Pending
        }
    }
}
impl Drop for Sleep<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.handle.timers.disarm(self.handle.id);
        }
    }
}

/// Single-consumer rendezvous slot. A full slot rejects duplicates without replacing its mbuf.
pub struct Slot<T> {
    value: Cell<Option<T>>,
    waker: Cell<Option<Waker>>,
}
impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            value: Cell::new(None),
            waker: Cell::new(None),
        }
    }
}
impl<T> Slot<T> {
    pub fn deliver(&self, value: T) -> Result<(), T> {
        if let Some(previous) = self.value.take() {
            self.value.set(Some(previous));
            return Err(value);
        }
        self.value.set(Some(value));
        if let Some(w) = self.waker.take() {
            w.wake();
        }
        Ok(())
    }
    pub fn recv<'a>(&'a self, handle: &'a mut Handle, deadline: u64) -> Receive<'a, T> {
        Receive {
            slot: self,
            timer: handle.sleep_until(deadline),
        }
    }
}
#[derive(Debug, PartialEq)]
pub struct Timeout;
pub struct Receive<'a, T> {
    slot: &'a Slot<T>,
    timer: Sleep<'a>,
}
impl<T> Future for Receive<'_, T> {
    type Output = Result<T, Timeout>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(value) = self.slot.value.take() {
            return Poll::Ready(Ok(value));
        }
        if Pin::new(&mut self.timer).poll(cx).is_ready() {
            return Poll::Ready(Err(Timeout));
        }
        self.slot.waker.set(Some(cx.waker().clone()));
        Poll::Pending
    }
}
impl<T> Drop for Receive<'_, T> {
    fn drop(&mut self) {
        self.slot.waker.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    thread_local! { static TIME: Cell<u64> = const { Cell::new(0) }; }
    fn now() -> u64 {
        TIME.with(Cell::get)
    }
    #[test]
    fn timers_reply_timeout_and_cancel() {
        TIME.with(|t| t.set(0));
        let slot = Rc::new(Slot::default());
        let receiver = slot.clone();
        let done = Rc::new(Cell::new(false));
        let end = done.clone();
        let mut rt = Runtime::new(1, now);
        rt.spawn(move |mut h| async move {
            assert_eq!(receiver.recv(&mut h, 20).await, Ok(7));
            let mut sleep = h.sleep(100);
            std::future::poll_fn(|cx| {
                assert!(Pin::new(&mut sleep).poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(sleep);
            h.sleep_until(12).await;
            assert!(h.now() >= 12);
            assert_eq!(receiver.recv(&mut h, 15).await, Err(Timeout));
            end.set(true);
        });
        rt.run(|| {
            TIME.with(|t| t.set(t.get() + 1));
            if now() == 5 {
                slot.deliver(7).unwrap();
            }
        });
        assert!(done.get());
    }
    #[test]
    fn full_slot_preserves_first_value_and_same_turn_delivery() {
        TIME.with(|t| t.set(0));
        let slot = Rc::new(Slot::default());
        slot.deliver(1).unwrap();
        assert_eq!(slot.deliver(2), Err(2));
        let receiver = slot.clone();
        let observed = Rc::new(Cell::new(0));
        let end = observed.clone();
        let mut rt = Runtime::new(1, now);
        rt.spawn(move |mut h| async move {
            assert_eq!(receiver.recv(&mut h, 10).await, Ok(1));
            assert_eq!(receiver.recv(&mut h, 10).await, Ok(3));
            end.set(now());
        });
        rt.run(|| {
            TIME.with(|t| t.set(t.get() + 1));
            if now() == 2 {
                slot.deliver(3).unwrap();
            }
        });
        assert_eq!(observed.get(), 2);
    }
    #[test]
    fn reply_runs_before_clock_check_and_maintenance() {
        thread_local! { static READS: Cell<usize> = const { Cell::new(0) }; }
        fn clock() -> u64 {
            READS.with(|n| n.set(n.get() + 1));
            now()
        }
        TIME.with(|t| t.set(0));
        READS.with(|n| n.set(0));
        let slot = Rc::new(Slot::default());
        let receiver = slot.clone();
        let done = Rc::new(Cell::new(false));
        let end = done.clone();
        let mut rt = Runtime::new(1, clock);
        rt.spawn(move |mut h| async move {
            let reads_at_delivery = receiver.recv(&mut h, 10).await.unwrap();
            assert_eq!(READS.with(Cell::get), reads_at_delivery);
            end.set(true);
        });
        rt.run_with_maintenance(
            || {
                TIME.with(|t| t.set(t.get() + 1));
                if now() == 2 {
                    slot.deliver(READS.with(Cell::get)).unwrap();
                }
            },
            || {
                if now() == 2 {
                    assert!(done.get());
                }
            },
        );
        assert!(done.get());
    }
    #[test]
    fn self_wakes_do_not_starve_reactor_timers_or_maintenance() {
        TIME.with(|t| t.set(0));
        let fired_at = Rc::new(Cell::new(0));
        let end = fired_at.clone();
        let mut rt = Runtime::new(2, now);
        let mut polls = 0;
        rt.spawn(move |_| {
            std::future::poll_fn(move |cx| {
                polls += 1;
                assert!(polls < CAP * 8, "reactor was starved");
                if now() >= 3 {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
        });
        rt.spawn(move |mut h| async move {
            h.sleep_until(2).await;
            end.set(now());
        });
        let mut maintenance_turns = 0;
        rt.run_with_maintenance(
            || TIME.with(|t| t.set(t.get() + 1)),
            || maintenance_turns += 1,
        );
        assert_eq!(fired_at.get(), 2);
        assert_eq!(maintenance_turns, 3);
    }
    #[test]
    fn dedup_self_wake_and_stale_identity() {
        let saved = Rc::new(RefCell::new(None));
        let w = saved.clone();
        let mut polls = 0;
        let mut rt = Runtime::new(1, now);
        rt.spawn(move |_| {
            std::future::poll_fn(move |cx| {
                polls += 1;
                *w.borrow_mut() = Some(cx.waker().clone());
                if polls == 2 {
                    return Poll::Ready(());
                }
                for _ in 0..1000 {
                    cx.waker().wake_by_ref();
                }
                Poll::Pending
            })
        });
        rt.run(|| {});
        drop(rt);
        let stale = saved.borrow_mut().take().unwrap();
        std::thread::spawn(move || {
            stale.wake_by_ref();
            drop(stale.clone());
        })
        .join()
        .unwrap();
    }
    #[test]
    fn foreign_wake_reaches_home_thread() {
        let (tx, rx) = std::sync::mpsc::channel::<Waker>();
        let complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = complete.clone();
        let thread = std::thread::spawn(move || {
            let w = rx.recv().unwrap();
            signal.store(true, Ordering::Release);
            w.wake();
        });
        let mut rt = Runtime::new(1, now);
        let mut sent = false;
        rt.spawn(move |_| {
            std::future::poll_fn(move |cx| {
                if complete.load(Ordering::Acquire) {
                    return Poll::Ready(());
                }
                if !sent {
                    tx.send(cx.waker().clone()).unwrap();
                    sent = true;
                }
                Poll::Pending
            })
        });
        rt.run(|| {});
        thread.join().unwrap();
    }
}
