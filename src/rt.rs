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
/// runtime 支持的最大 task 数，同时决定 ready 队列和外部唤醒位图的容量。
const CAP: usize = 128;
/// 固定在堆上的异步任务对象；Pin 保证 Future 内跨 await 保存的数据地址不会移动。
type Task = Pin<Box<dyn Future<Output = ()>>>;

/// 收发线程独占的定长就绪队列；保存 task 编号而不是 Future 本身。
struct Ready {
    /// 容量为 CAP 的 task 编号数组，按 head 与 len 解释成环形 FIFO。
    ring: [usize; CAP],
    /// 当前队首在 ring 中的索引，出队后循环递增。
    head: usize,
    /// 当前已经排队、尚未出队的 task 数。
    len: usize,
    /// 按 task 编号记录是否已入队；避免重复 wake 挤占容量。
    queued: [bool; CAP],
}
impl Ready {
    /// 创建空的定长 FIFO 就绪队列，所有 task 初始均未入队。
    fn new() -> Self {
        Self {
            ring: [0; CAP],
            head: 0,
            len: 0,
            queued: [false; CAP],
        }
    }
    /// 把 task 编号加入队尾；若已经排队则忽略重复唤醒，容量由每 task 最多一项保证。
    fn push(&mut self, id: usize) {
        if !self.queued[id] {
            self.queued[id] = true;
            self.ring[(self.head + self.len) % CAP] = id;
            self.len += 1;
        }
    }
    /// 取出队首 task 编号并清除去重标志，让它在本次 poll 中仍可再次唤醒自己。
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
/// 外部线程唤醒使用的原子位图；外部只设置位，task 仍由所属线程 poll。
struct Foreign {
    /// 两个 64 位原子字，共表示最多 128 个 task；置位表示该 task 有外部唤醒待处理。
    bits: [AtomicU64; 2],
}
/// 全局 token 到弱 runtime 唤醒句柄及 task 编号的映射，避免延长 runtime 生命周期。
type Registry = HashMap<usize, (Weak<Foreign>, usize)>;
/// 惰性取得进程级 Waker 注册表，供跨线程唤醒定位所属 runtime。
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}
#[derive(Clone)]
/// 当前线程正在运行的 runtime 上下文，供本线程 Waker 直接访问 ready 队列。
struct Home {
    /// 本 runtime 的 token 区间起点；token−base 得到本地 task 编号。
    base: usize,
    /// 本 runtime 的就绪队列共享引用，只在所属线程中借用和修改。
    ready: Rc<RefCell<Ready>>,
}
// 只通过 TLS 访问 Rc 就绪队列，绝不把它的指针交给外部线程。
thread_local! { static HOME: RefCell<Option<Home>> = const { RefCell::new(None) }; }
/// 运行循环的 TLS 恢复守卫；退出时恢复进入前的上下文。
struct HomeGuard(
    /// 进入本次 run 之前的 TLS 上下文；None 表示当时没有活动 runtime。
    Option<Home>,
);
impl Drop for HomeGuard {
    /// 退出 run 时恢复进入前的 TLS runtime 上下文，避免留下已销毁 runtime 的引用。
    fn drop(&mut self) {
        HOME.with(|h| *h.borrow_mut() = self.0.take());
    }
}

/// 按唯一 token 唤醒 task；当前线程直接入 ready 队列，外部线程通过注册表设置原子位。
/// 旧 runtime 注销后找不到 token，旧 Waker 的唤醒会被忽略。
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
/// 复制只携带整数身份的 RawWaker；token 不拥有对象内存，因此不用增加引用计数。
unsafe fn clone_raw(p: *const ()) -> RawWaker {
    RawWaker::new(p, &VTABLE)
}
/// RawWaker 的唤醒回调：只把 data 当作整数 token，交给 wake 路由，不解引用。
unsafe fn wake_raw(p: *const ()) {
    wake(p.addr());
}
/// RawWaker 的释放回调；整数 token 不持有资源，所以无需释放内存。
unsafe fn drop_raw(_: *const ()) {}
static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_raw, wake_raw, wake_raw, drop_raw);
/// 把不会复用的整数 token 装入 RawWaker，绑定自定义 vtable 后构造标准 Waker。
fn waker(token: usize) -> Waker {
    let data = std::ptr::without_provenance(token);
    unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) }
}

/// 单个 task 的 timer 槽，接收超时与 sleep 复用它，不能同时注册两份等待。
struct TimerSlot {
    /// 绝对截止 ticks；u64::MAX 表示当前未注册 timer。
    deadline: Cell<u64>,
    /// 该槽所属 task 的 Waker，到期时用它把 task 放入就绪队列。
    waker: Waker,
}
/// 单线程 timer 表；每 task 一槽，并缓存最早 deadline 以减少无效扫描。
struct Timers {
    /// 按 task 编号索引的 timer 表，在 runtime 初始化时一次性创建。
    slots: Vec<TimerSlot>,
    /// 最早截止 ticks 的缓存；取消 timer 后可暂时偏早，因此可能多扫描一次。
    next: Cell<u64>,
}
impl Timers {
    /// 设置指定 task 的绝对截止时间，并更新最早 deadline 缓存。
    fn arm(&self, id: usize, deadline: u64) {
        self.slots[id].deadline.set(deadline);
        self.next.set(self.next.get().min(deadline));
    }
    /// 取消指定 task 的 timer；保留旧 next 只会多触发一次扫描，不会漏掉到期事件。
    fn disarm(&self, id: usize) {
        // 不重算 next：旧值只会让后续多扫描一次，不会错过真正的到期时间。
        self.slots[id].deadline.set(u64::MAX);
    }
    /// 若最早 deadline 已到，则扫描槽并唤醒到期 task，同时重算剩余最早截止时间。
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
    /// 分配给此 runtime 的唯一 token 区间起点，生命周期结束后也不复用。
    base: usize,
    /// 本线程就绪 task 的 FIFO，Waker 与 executor 通过它交接。
    ready: Rc<RefCell<Ready>>,
    /// 接收外部线程唤醒的原子位图；run 循环把置位项转入 ready。
    foreign: Arc<Foreign>,
    /// 每个 task 的 timer 槽及最早截止时间缓存。
    timers: Rc<Timers>,
    /// 按 task 编号保存的固定地址 Future；完成后对应项变为 None。
    tasks: Vec<Option<Task>>,
    /// 注入的单调 ticks 读取函数；由应用决定 ticks 的实际时间单位。
    clock: fn() -> u64,
}
impl Runtime {
    /// 创建最多 capacity 个 task 的 runtime，预建 timer/Waker，并分配全局唯一身份区间。
    /// clock 由调用方注入；本库不依赖 DPDK，也不创建收发线程。
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
    /// 从 FIFO 就绪队列取 task 并 poll，每次调用最多执行 CAP 次，返回完成的 task 数。
    /// 限制次数使持续 self-wake 的 task 也不会饿死收包、timer 或维护。
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
    /// 使用无额外维护回调的运行循环；reactor 负责检查外部事件并唤醒对应 task。
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
    /// 销毁 runtime 时注销所有 Waker token；之后旧 Waker 不会访问或唤醒新 runtime。
    fn drop(&mut self) {
        let mut reg = registry().lock().unwrap();
        for id in 0..self.timers.slots.len() {
            reg.remove(&(self.base + id));
        }
    }
}

/// 每个 task 独享一个 Handle；&mut 借用保证同一时刻最多存在一个等待中的 timer。
pub struct Handle {
    /// 当前 task 在 runtime 的 task/timer 表中的编号。
    id: usize,
    /// 所属 runtime 的 timer 表；此 Handle 只操作自己的 id 槽。
    timers: Rc<Timers>,
    /// 与所属 runtime 相同的时钟函数，供 sleep 和 deadline 检查使用。
    clock: fn() -> u64,
}
impl Handle {
    /// 调用注入的时钟读取当前 ticks；生产环境为 TSC，测试可替换为手动时钟。
    pub fn now(&self) -> u64 {
        (self.clock)()
    }
    /// 创建等待绝对 deadline 的 Future；timer 在首次 Pending 时注册，尚未在此处入表。
    pub fn sleep_until(&mut self, deadline: u64) -> Sleep<'_> {
        Sleep {
            handle: self,
            deadline,
            armed: false,
        }
    }
    /// 读取当前 ticks，创建等待指定相对时长的 Future；返回值仍在 poll 时判断到期。
    pub fn sleep(&mut self, ticks: u64) -> Sleep<'_> {
        self.sleep_until(self.now() + ticks)
    }
}
/// 到期返回原 deadline，调用者用它计算唤醒误差，而非把恢复时刻当作到期时刻。
pub struct Sleep<'a> {
    /// 当前 task 的独占 Handle 借用，防止同时建立多个占用同一槽的等待。
    handle: &'a mut Handle,
    /// 原始绝对截止 ticks，到期后作为 Ready 的返回值用于计算 sleep 误差。
    deadline: u64,
    /// 此 Future 是否曾把 deadline 注册进 timer 表，用于避免重复注册并在 Drop 时清理。
    armed: bool,
}
impl Future for Sleep<'_> {
    type Output = u64;
    /// 到期则撤销 timer 并返回原 deadline；未到期时只注册一次 timer，然后返回 Pending。
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
    /// Future 完成或被取消时清除已经注册的 timer，避免取消后的等待仍触发唤醒。
    fn drop(&mut self) {
        // 等待 reply 被提前完成或 future 被取消时，也要撤销原先的超时 timer。
        if self.armed {
            self.handle.timers.disarm(self.handle.id);
        }
    }
}

/// 单线程、单消费者的交接槽；槽已满时拒绝新值，保留第一个 reply 及其 mbuf。
pub struct Slot<T> {
    /// 待交付值的唯一所有权；None 表示尚未到达或已经被接收 Future 取走。
    value: Cell<Option<T>>,
    /// 当前等待该槽的 task 的唤醒订阅；deliver 取出并消费，避免重复保留。
    waker: Cell<Option<Waker>>,
}
impl<T> Default for Slot<T> {
    /// 创建既没有 reply、也没有等候 Waker 的空槽。
    fn default() -> Self {
        Self {
            value: Cell::new(None),
            waker: Cell::new(None),
        }
    }
}
impl<T> Slot<T> {
    /// 把值放入空槽并唤醒等待 task；wake 只入就绪队列，不在此处执行 task。
    /// 槽已满时保留旧值并返回 Err(value)，让调用者继续持有新值的所有权。
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
    /// 借用槽和 task 的 Handle，创建附带接收 deadline 的 Future；不在此处阻塞线程。
    pub fn recv<'a>(&'a self, handle: &'a mut Handle, deadline: u64) -> Receive<'a, T> {
        Receive {
            slot: self,
            timer: handle.sleep_until(deadline),
        }
    }
}
#[derive(Debug, PartialEq)]
/// 接收截止时间已到但槽内仍没有 reply 时返回的零大小错误标记。
pub struct Timeout;
/// 等待槽中出现一个值或超时的 Future；持有 Handle 的独占借用以限制并行等待。
pub struct Receive<'a, T> {
    /// 等待接收的单消费者槽，不复制其中的值。
    slot: &'a Slot<T>,
    /// 与接收等待绑定的超时 Future；取消接收也会撤销该 timer。
    timer: Sleep<'a>,
}
impl<T> Future for Receive<'_, T> {
    type Output = Result<T, Timeout>;
    /// 优先从槽取出已接纳的 reply；否则检查超时，并保存当前 task 的 Waker 后返回 Pending。
    /// 先检查 reply，避免任务排队导致 T3 变晚时把按时到达的包误算为超时。
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
    /// 取消或完成接收等待时移除槽中订阅的 Waker；timer 字段随后撤销超时注册。
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
    /// 读取当前测试线程的手动时钟，避免单元测试依赖真实时间。
    fn now() -> u64 {
        TIME.with(Cell::get)
    }
    #[test]
    /// 验证 sleep 到期、reply 成功、接收超时和取消接收后的 timer 清理。
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
    /// 验证槽满时保留第一份值并退回新值，以及同轮交付后 task 能立即恢复。
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
    /// 验证 reply 唤醒的 task 在 runtime 额外读取时钟和调用维护闭包之前先执行。
    fn reply_runs_before_clock_check_and_maintenance() {
        thread_local! { static READS: Cell<usize> = const { Cell::new(0) }; }
        /// 读取手动时钟并累计读取次数，用来检验 reply 与 timer 检查的执行顺序。
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
    /// 验证持续自唤醒的任务受单轮 poll 上限约束，不阻塞 reactor、timer 和维护。
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
    /// 验证重复 self-wake 只排队一次，以及 runtime 销毁后旧 Waker 仍可安全调用。
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
    /// 验证外部线程发布数据并唤醒后，task 在所属 runtime 线程重新 poll 并看到数据。
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
