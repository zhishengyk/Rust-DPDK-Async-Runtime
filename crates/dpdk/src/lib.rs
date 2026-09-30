pub use arrayvec::ArrayVec;
use dpdk_sys::*;
use std::{
    ffi::{CStr, CString},
    ptr::NonNull,
    rc::Rc,
};
pub const BURST: usize = 32;

fn error() -> String {
    unsafe { CStr::from_ptr(w_error()).to_string_lossy().into_owned() }
}
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

/// Unique owner of a packet; Rc keeps its pool/EAL alive, and makes it !Send/!Sync.
pub struct Mbuf {
    ptr: NonNull<w_mbuf>,
    _pool: Rc<Pool>,
}
impl Mbuf {
    pub fn data(&self) -> &[u8] {
        // SAFETY: unique live mbuf, bounded by its first segment's data_len.
        unsafe {
            std::slice::from_raw_parts(w_data(self.ptr.as_ptr()), w_len(self.ptr.as_ptr()) as usize)
        }
    }
    pub fn data_mut(&mut self) -> &mut [u8] {
        // SAFETY: no shared templates/refcounts exposed by this API.
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
        unsafe {
            w_free(self.ptr.as_ptr());
        }
    }
}

/// One process, one port, one RX/TX queue, all used on the calling thread.
pub struct Port {
    pool: Rc<Pool>,
    pub mac: [u8; 6],
    pub initial_avail: u32,
    active: bool,
}
impl Port {
    pub fn open(bdf: &str, core: usize) -> Result<Self, String> {
        // EAL is process-global. Reject a second initialization, including after drop.
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
        let mut ptrs = [std::ptr::null_mut(); BURST];
        let mut t2 = 0;
        let n = unsafe { w_rx(ptrs.as_mut_ptr(), BURST as u16, &mut t2) };
        let mut packets = ArrayVec::new();
        for ptr in &ptrs[..n as usize] {
            packets.push(Mbuf {
                ptr: NonNull::new(*ptr).unwrap(),
                _pool: self.pool.clone(),
            });
        }
        (packets, t2)
    }
    /// PMD takes ownership only on success. Failure returns the original owner.
    pub fn send(&mut self, packet: Mbuf) -> Result<u64, Mbuf> {
        let mut t1 = 0;
        if unsafe { w_tx(packet.ptr.as_ptr(), &mut t1) } == 1 {
            // Drop only the Rc field; the PMD now owns the mbuf pointer.
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
        self.stop();
        (self.initial_avail, unsafe {
            w_pool_avail(self.pool.ptr.as_ptr())
        })
    }
    fn stop(&mut self) {
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
