//! DPDK 的单线程安全封装：把 C 的裸 mbuf 指针转为 Rust 唯一所有者。
//! 生命周期顺序为 Mbuf → Pool → EAL；端口关闭时归还驱动持有的 RX/TX 缓冲区。
pub use arrayvec::ArrayVec;
use dpdk_sys::*;
use std::{
    ffi::{CStr, CString},
    ptr::NonNull,
    rc::Rc,
};
pub const BURST: usize = 32;

fn error() -> String {
    // SAFETY: w_error 返回 DPDK 管理的 NUL 结尾字符串，这里立即复制为 Rust String。
    unsafe { CStr::from_ptr(w_error()).to_string_lossy().into_owned() }
}
// 最后一个 Pool 释放后才清理 EAL，使 mbuf 的释放始终有有效的 DPDK 环境。
struct Eal;
impl Drop for Eal {
    fn drop(&mut self) {
        unsafe {
            w_eal_cleanup();
        }
    }
}
struct Pool {
    ptr: NonNull<w_pool>,
    _eal: Eal,
}
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            w_pool_free(self.ptr.as_ptr());
        }
    }
}

/// 包的唯一所有者；Rc 延长 Pool/EAL 的生命期，并让 Mbuf 自然成为 !Send/!Sync。
pub struct Mbuf {
    ptr: NonNull<w_mbuf>,
    _pool: Rc<Pool>,
}
impl Mbuf {
    pub fn data(&self) -> &[u8] {
        // SAFETY: 活着的唯一 mbuf；切片长度限制为首段 data_len，借用不能超过 self。
        unsafe {
            std::slice::from_raw_parts(w_data(self.ptr.as_ptr()), w_len(self.ptr.as_ptr()) as usize)
        }
    }
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
    fn drop(&mut self) {
        // SAFETY: 仅持有者会运行 Drop；成功 TX 后跳过此 Drop，由驱动负责回收。
        unsafe {
            w_free(self.ptr.as_ptr());
        }
    }
}

/// 单进程只打开一个端口、一对 RX/TX 队列；Rc 将所有操作限制在所属线程。
pub struct Port {
    pool: Rc<Pool>,
    pub mac: [u8; 6],
    pub initial_avail: u32,
    active: bool,
}
impl Port {
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
    pub fn alloc(&self, len: usize) -> Option<Mbuf> {
        let len = u16::try_from(len).ok()?;
        let ptr = NonNull::new(unsafe { w_alloc(self.pool.ptr.as_ptr(), len) })?;
        Some(Mbuf {
            ptr,
            _pool: self.pool.clone(),
        })
    }
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
    pub fn send(&mut self, packet: Mbuf) -> Result<u64, Mbuf> {
        let mut t1 = 0;
        if unsafe { w_tx(packet.ptr.as_ptr(), &mut t1) } == 1 {
            // mbuf 指针已经属于 PMD：跳过 Mbuf::drop，仅释放 Rust 侧的 Pool 引用。
            let mut packet = std::mem::ManuallyDrop::new(packet);
            unsafe {
                std::ptr::drop_in_place(&mut packet._pool);
            }
            Ok(t1)
        } else {
            Err(packet)
        }
    }
    pub fn maintenance(&mut self) {
        unsafe {
            w_maintenance();
        }
    }
    pub fn stats(&self) -> [u64; 6] {
        let mut v = [0; 6];
        unsafe {
            w_stats(v.as_mut_ptr());
        }
        v
    }
    pub fn finish(mut self) -> (u32, u32) {
        // 先停端口归还描述符里的 mbuf，再取池计数；调用者也应先释放其余 Mbuf。
        self.stop();
        (self.initial_avail, unsafe {
            w_pool_avail(self.pool.ptr.as_ptr())
        })
    }
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
    fn drop(&mut self) {
        self.stop();
    }
}
