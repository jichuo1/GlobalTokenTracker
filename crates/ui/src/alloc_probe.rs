//! TEMP memory probe — counts live/peak bytes handed out by the Rust allocator.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

pub struct Counting;
pub static LIVE: AtomicUsize = AtomicUsize::new(0);
pub static PEAK: AtomicUsize = AtomicUsize::new(0);
pub static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
            ALLOCS.fetch_add(1, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let now = LIVE.fetch_add(new - l.size(), Relaxed) + (new - l.size());
                PEAK.fetch_max(now, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

pub fn spawn_reporter() {
    std::thread::spawn(|| {
        let t0 = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            eprintln!(
                "[mem] t={:>3}s rust_live={:>7.1}MB peak={:>7.1}MB allocs={}",
                t0.elapsed().as_secs(),
                LIVE.load(Relaxed) as f64 / 1048576.0,
                PEAK.load(Relaxed) as f64 / 1048576.0,
                ALLOCS.load(Relaxed)
            );
        }
    });
}
