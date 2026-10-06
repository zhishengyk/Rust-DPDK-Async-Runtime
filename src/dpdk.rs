//! DPDK 的单线程安全封装：把 C 的裸 mbuf 指针转为 Rust 唯一所有者。
//! 生命周期顺序为 Mbuf → Pool → EAL；端口关闭时归还驱动持有的 RX/TX 缓冲区。
pub use arrayvec::ArrayVec;
use dpdk_sys::*;
use std::{
    ffi::{CStr, CString},
    ptr::NonNull,
    rc::Rc,
};
/// 单次 RX burst 最多取得的 mbuf 数，也是栈上指针数组和 ArrayVec 的容量。
pub const BURST: usize = 32;

/// 统计线程单独绑定 CPU；不注册为 DPDK lcore，不访问 mbuf 或收发队列。
pub fn pin_thread(core: u32) {
    assert_eq!(
        unsafe { w_pin_thread(core) },
        0,
        "cannot pin statistics thread"
    );
}

/// 立即复制 DPDK 当前错误消息为 Rust String，避免把 C 字符串借用带出 FFI 边界。
fn error() -> String {
    // SAFETY: w_error 返回 DPDK 管理的 NUL 结尾字符串，这里立即复制为 Rust String。
    unsafe { CStr::from_ptr(w_error()).to_string_lossy().into_owned() }
}
/// 进程级 DPDK 初始化资源的析构守卫；由 Pool 持有以固定清理顺序。
struct Eal;
impl Drop for Eal {
    /// 最后一个池释放后清理进程级 EAL；此时所有依赖该池的 mbuf 都已归还。
    fn drop(&mut self) {
        unsafe {
            w_eal_cleanup();
        }
    }
}
/// DPDK mbuf 内存池所有者；由端口和活跃 mbuf 通过 Rc 共同延长生命周期。
struct Pool {
    /// 底层 rte_mempool 的非空不透明指针，仅由此包装负责最终销毁。
    ptr: NonNull<w_pool>,
    /// EAL 生命周期守卫，确保释放内存池之前 DPDK 环境始终有效。
    _eal: Eal,
}
impl Drop for Pool {
    /// 释放底层 mempool；字段中的 Eal 守卫随后析构，保持池先于 EAL 销毁。
    fn drop(&mut self) {
        unsafe {
            w_pool_free(self.ptr.as_ptr());
        }
    }
}

/// 包的唯一所有者；Rc 延长 Pool/EAL 的生命期，并让 Mbuf 自然成为 !Send/!Sync。
pub struct Mbuf {
    /// 此报文 rte_mbuf 的非空指针，指向报文元数据及关联的数据缓冲。
    ptr: NonNull<w_mbuf>,
    /// 所属池的共享引用；延长池生命周期，不复制报文，也不增加 C mbuf 的引用计数。
    _pool: Rc<Pool>,
}
impl Mbuf {
    /// 借用此 mbuf 首段的有效报文字节；返回切片不能比 mbuf 活得更久。
    pub fn data(&self) -> &[u8] {
        // SAFETY: 活着的唯一 mbuf；切片长度限制为首段 data_len，借用不能超过 self。
        unsafe {
            std::slice::from_raw_parts(w_data(self.ptr.as_ptr()), w_len(self.ptr.as_ptr()) as usize)
        }
    }
    /// 独占借用此 mbuf 首段的有效报文字节，供构造报文或原地改写 ARP 使用。
    pub fn data_mut(&mut self) -> &mut [u8] {
        // SAFETY: &mut self 排除并发别名；本 API 不暴露克隆 mbuf 或共享数据段的操作。
        unsafe {
            std::slice::from_raw_parts_mut(
                w_data(self.ptr.as_ptr()),
                w_len(self.ptr.as_ptr()) as usize,
            )
        }
    }
}
impl Drop for Mbuf {
    /// 将仍由 Rust 持有的 mbuf 归还原池；成功交给 TX 驱动的包会跳过此析构。
    fn drop(&mut self) {
        // SAFETY: 仅持有者会运行 Drop；成功 TX 后跳过此 Drop，由驱动负责回收。
        unsafe {
            w_free(self.ptr.as_ptr());
        }
    }
}

/// 单进程只打开一个端口、一对 RX/TX 队列；Rc 将所有操作限制在所属线程。
pub struct Port {
    /// 端口使用的 mbuf 池；RX 描述符和 TX 分配共享这份池资源。
    pool: Rc<Pool>,
    /// 启动时读取的本机网卡 MAC，用于填充报文模板和 ARP 应答。
    pub mac: [u8; 6],
    /// 配置 RX 描述符之前的空闲 mbuf 数，作为关闭端口后的泄漏检查基线。
    pub initial_avail: u32,
    /// 端口是否仍需停止/关闭；关闭后置 false，防止 finish 与 Drop 重复操作。
    active: bool,
}
impl Port {
    /// Port 创建成功意味着 EAL 已初始化，DPDK 已确定本机 TSC 频率。
    pub fn tsc_hz(&self) -> u64 {
        unsafe { w_tsc_hz() }
    }
    /// 只初始化一次 EAL，绑定收发核和指定 PCI 网卡，创建池并启动单端口单队列。
    /// 配置 RX 描述符前保存可用 mbuf 基线，供结束时检查泄漏。
    pub fn open(bdf: &str, core: usize) -> Result<Self, String> {
        // EAL 是进程级状态，只初始化一次；这项约束是安全封装前提，不在逐包路径上。
        static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return Err("EAL already initialized".into());
        }
        let args: Vec<CString> = [
            "ping".into(),
            "-l".into(),
            core.to_string(),
            "--main-lcore".into(),
            core.to_string(),
            "--in-memory".into(),
            "--iova-mode=pa".into(),
            "-a".into(),
            bdf.into(),
            "--log-level=lib.eal:warning".into(),
        ]
        .into_iter()
        .map(|s: String| CString::new(s).unwrap())
        .collect();
        let mut argv: Vec<_> = args.iter().map(|s| s.as_ptr() as *mut _).collect();
        // SAFETY: C 字符串及 argv 在初始化调用期间存活；之后由 Pool/Eal 守卫管理资源。
        unsafe {
            if w_eal_init(argv.len() as i32, argv.as_mut_ptr()) < 0 {
                return Err(error());
            }
            let eal = Eal;
            let ptr = NonNull::new(w_pool_create()).ok_or_else(error)?;
            let pool = Rc::new(Pool { ptr, _eal: eal });
            let mut port = Self {
                initial_avail: w_pool_avail(ptr.as_ptr()),
                pool,
                mac: [0; 6],
                active: true,
            };
            let rc = w_port_start(ptr.as_ptr(), port.mac.as_mut_ptr());
            if rc < 0 {
                return Err(format!("port start: {rc}"));
            }
            Ok(port)
        }
    }
    /// 从本端口的池取得一个 mbuf，并在已有数据空间内预留 len 字节。
    /// 成功返回独占所有者，长度不支持、池耗尽或空间不足时返回 None。
    pub fn alloc(&self, len: usize) -> Option<Mbuf> {
        let len = u16::try_from(len).ok()?;
        let ptr = NonNull::new(unsafe { w_alloc(self.pool.ptr.as_ptr(), len) })?;
        Some(Mbuf {
            ptr,
            _pool: self.pool.clone(),
        })
    }
    /// 一次最多接收 BURST 个包；返回 Rust mbuf 所有者及整批共用的 T2。
    /// T2 在 C 的 RX burst 返回后记录，随后进行的 Rust 包装计入接收段。
    pub fn receive(&mut self) -> (ArrayVec<Mbuf, BURST>, u64) {
        // 固定数组接收至多 BURST 个指针，ArrayVec 包装它们而不在热路径上分配堆内存。
        let mut ptrs = [std::ptr::null_mut(); BURST];
        let mut t2 = 0;
        let n = unsafe { w_rx(ptrs.as_mut_ptr(), BURST as u16, &mut t2) };
        // T2 已在 C 内记录，包含下面的包装、解析和调度耗时；同一批包不得重新打点。
        let mut packets = ArrayVec::new();
        for ptr in &ptrs[..n as usize] {
            packets.push(Mbuf {
                ptr: NonNull::new(*ptr).unwrap(),
                _pool: self.pool.clone(),
            });
        }
        (packets, t2)
    }
    /// 驱动接纳成功才转移 mbuf；失败返回原所有者，由调用者重试或 Drop 释放。
    pub fn send(&mut self, packet: Mbuf) -> Result<u64, (Mbuf, u64)> {
        let mut t1 = 0;
        if unsafe { w_tx(packet.ptr.as_ptr(), &mut t1) } == 1 {
            // mbuf 指针已经属于 PMD：跳过 Mbuf::drop，仅释放 Rust 侧的 Pool 引用。
            let mut packet = std::mem::ManuallyDrop::new(packet);
            unsafe {
                std::ptr::drop_in_place(&mut packet._pool);
            }
            Ok(t1)
        } else {
            Err((packet, t1))
        }
    }
    /// 调用 C 适配层驱动 DPDK 定时维护；执行频率由共用 I/O 层控制。
    pub fn maintenance(&mut self) {
        unsafe {
            w_maintenance();
        }
    }
    /// 消费端口所有者并先停止/关闭端口，返回池的初始和最终可用数量用于对账。
    pub fn finish(mut self) -> (u32, u32) {
        // 先停端口归还描述符里的 mbuf，再取池计数；调用者也应先释放其余 Mbuf。
        self.stop();
        (self.initial_avail, unsafe {
            w_pool_avail(self.pool.ptr.as_ptr())
        })
    }
    /// 若端口仍运行就停止并关闭，使驱动归还 RX/TX 缓冲；active 防止重复关闭。
    fn stop(&mut self) {
        // finish 和 Drop 共用此入口，确保关闭只发生一次。
        if self.active {
            unsafe {
                w_port_stop();
            }
            self.active = false;
        }
    }
}
impl Drop for Port {
    /// 端口离开作用域时执行一次 stop，随后字段析构释放池引用。
    fn drop(&mut self) {
        self.stop();
    }
}
