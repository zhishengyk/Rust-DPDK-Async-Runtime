//! 单线程、主动轮询的 executor；时钟和收包 reactor 由调用者提供，不依赖 DPDK。
//! 一轮执行顺序：收包并唤醒 → poll 就绪任务 → 检查 timer/跨线程唤醒 → 再 poll → 维护。
//! task 仅在 spawn 时分配；本线程 wake 只入就绪队列，不分配、不加锁、不做原子读改写。
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

// 定长 FIFO：每个 task 最多入队一次，因此 CAP 个任务不会撑满后再溢出。
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
        // poll 前解除去重标记，允许任务在自己的 poll 中再次 wake 自己。
        self.queued[id] = false;
        Some(id)
    }
}
// Waker 可以跨线程使用，但外部线程只能置位；Future 始终在所属线程上 poll。
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
// 只通过 TLS 访问 Rc 就绪队列，绝不把它的指针交给外部线程。
thread_local! { static HOME: RefCell<Option<Home>> = const { RefCell::new(None) }; }
struct HomeGuard(Option<Home>);
impl Drop for HomeGuard {
    fn drop(&mut self) {
        HOME.with(|h| *h.borrow_mut() = self.0.take());
    }
}

fn wake(token: usize) {
    // 常见路径：当前 runtime 的任务直接入队，同一轮就能恢复执行。
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
        // 外部/其他 runtime 的唤醒经注册表转交；Weak 不延长 runtime 的生命期。
        let reg = registry().lock().unwrap();
        if let Some((f, id)) = reg.get(&token) {
            if let Some(f) = f.upgrade() {
                f.bits[id / 64].fetch_or(1 << (id % 64), Ordering::Release);
            }
        }
    }
}
// SAFETY: data 只是永不复用的整数身份，不是可解引用的对象指针。
// 本线程走 TLS，其他线程走锁和原子位图；runtime 销毁后查不到身份，旧 Waker 无效。
// clone/drop 无须管理引用计数，因为这个整数身份本身不拥有内存。
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
    // u64::MAX 表示未注册；每个 task 固定占用一个槽。
    deadline: Cell<u64>,
    waker: Waker,
}
struct Timers {
    slots: Vec<TimerSlot>,
    // 缓存最早 deadline；尚未到期时无须扫描所有槽。
    next: Cell<u64>,
}
impl Timers {
    fn arm(&self, id: usize, deadline: u64) {
        self.slots[id].deadline.set(deadline);
        self.next.set(self.next.get().min(deadline));
    }
    fn disarm(&self, id: usize) {
        // 不重算 next：旧值只会让后续多扫描一次，不会错过真正的到期时间。
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

/// 最多 128 个任务的单线程运行时；所有任务在 run 之前创建。
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
        // 每个 runtime 独占一段身份，避免旧 Waker 误唤醒新 runtime 的同号任务。
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
    /// 为任务分配固定槽并首次入队；Pin<Box<_>> 保证跨 await 保存的数据地址稳定。
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
        // 每阶段限制 poll 次数，避免不断自唤醒的任务饿死收包、timer 和维护。
        for _ in 0..CAP {
            let Some(id) = self.ready.borrow_mut().pop() else {
                break;
            };
            let Some(task) = self.tasks.get_mut(id).and_then(Option::as_mut) else {
                // 已完成任务可能仍有旧的 wake 留在队列中。
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
    /// 先交付 reply，再处理 timer，最后做维护；维护仍在当前线程执行。
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
            // 回包已到手就立即恢复 task，不在 T2 → T3 之间插入 timer 扫描或网卡维护。
            live -= self.poll_ready();
            self.timers.fire((self.clock)());
            // Acquire 与外部 wake 的 Release 配对，接收它在唤醒前发布的数据。
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

/// 每个 task 独享一个 Handle；&mut 借用保证同一时刻最多存在一个等待中的 timer。
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
/// 到期返回原 deadline，调用者用它计算唤醒误差，而非把恢复时刻当作到期时刻。
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
        // 等待 reply 被提前完成或 future 被取消时，也要撤销原先的超时 timer。
        if self.armed {
            self.handle.timers.disarm(self.handle.id);
        }
    }
}

/// 单线程、单消费者的交接槽；槽已满时拒绝新值，保留第一个 reply 及其 mbuf。
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
        // 先放数据再唤醒；wake 只入队，不会在 deliver 内重入 poll。
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
        // reply 优先：即使本次 poll 已过 deadline，已被 reactor 接纳的回包仍交给任务。
        if let Some(value) = self.slot.value.take() {
            return Poll::Ready(Ok(value));
        }
        if Pin::new(&mut self.timer).poll(cx).is_ready() {
            return Poll::Ready(Err(Timeout));
        }
        self.slot.waker.set(Some(cx.waker().clone()));
        // 槽与 reactor 在同一线程，两步间不会并发投递，所以无需锁或二次检查。
        Poll::Pending
    }
}
impl<T> Drop for Receive<'_, T> {
    fn drop(&mut self) {
        // 完成或取消等待时移除订阅；timer 字段随后 Drop，自动取消超时。
        self.slot.waker.take();
    }
}

#[cfg(test)]
mod tests {
    // 用手动推进的时钟验证调度与取消语义，不依赖真实时间或网卡。
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
