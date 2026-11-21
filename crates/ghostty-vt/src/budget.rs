//! A shared byte budget. Used by frozen paint sources, their Ghostty allocator,
//! and the core's viewer queues; independent of the live terminal's allocator.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub struct Budget {
    limit: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
    denied: AtomicUsize,
}
impl Budget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self { limit, used: AtomicUsize::new(0), peak: AtomicUsize::new(0), denied: AtomicUsize::new(0) })
    }
    pub fn limit(&self) -> usize {
        self.limit
    }
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
    pub fn denied(&self) -> usize {
        self.denied.load(Ordering::Relaxed)
    }
    #[allow(deprecated)] // Keep compatibility with the upstream Rust toolchain.
    fn take(&self, bytes: usize) -> bool {
        match self.used.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes).filter(|n| *n <= self.limit)
        }) {
            Ok(was) => {
                self.peak.fetch_max(was + bytes, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.denied.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
    fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }
    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Option<Reservation> {
        self.take(bytes).then(|| Reservation { budget: self.clone(), bytes })
    }
}
pub struct Reservation {
    budget: Arc<Budget>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

/// Exact capacity accounting, including old+new storage during growth.
pub struct Bytes {
    bytes: Vec<u8>,
    reservation: Reservation,
}
impl Bytes {
    pub fn new(budget: &Arc<Budget>) -> Self {
        Self { bytes: Vec::new(), reservation: Reservation { budget: budget.clone(), bytes: 0 } }
    }
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}
impl std::io::Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let needed = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| std::io::Error::other("paint_limit"))?;
        if needed > self.bytes.capacity() {
            let capacity = needed.max(self.bytes.capacity().saturating_mul(2)).max(4096);
            let reservation =
                self.reservation.budget.reserve(capacity).ok_or_else(|| std::io::Error::other("paint_limit"))?;
            let mut next = Vec::new();
            next.try_reserve_exact(capacity).map_err(|_| std::io::Error::other("paint_limit"))?;
            next.extend_from_slice(&self.bytes);
            self.bytes = next;
            self.reservation = reservation;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Zig never calls these concurrently for one terminal. The budget itself is
// shared across independent terminals and queues and is thread-safe.
#[repr(C)]
pub(crate) struct Allocator {
    ctx: *const Budget,
    vtable: &'static Vtable,
    _budget: Arc<Budget>,
}
unsafe impl Send for Allocator {}
#[repr(C)]
struct Vtable {
    alloc: unsafe extern "C" fn(*const Budget, usize, u8, usize) -> *mut u8,
    resize: unsafe extern "C" fn(*const Budget, *mut u8, usize, u8, usize, usize) -> bool,
    remap: unsafe extern "C" fn(*const Budget, *mut u8, usize, u8, usize, usize) -> *mut u8,
    free: unsafe extern "C" fn(*const Budget, *mut u8, usize, u8, usize),
}
impl Allocator {
    pub(crate) fn new(budget: &Arc<Budget>) -> Box<Self> {
        Box::new(Self { ctx: Arc::as_ptr(budget), vtable: &VTABLE, _budget: budget.clone() })
    }
    pub(crate) fn budget(&self) -> &Arc<Budget> {
        &self._budget
    }
    pub(crate) fn as_ptr(&self) -> *const std::ffi::c_void {
        (self as *const Self).cast()
    }
}
fn layout(len: usize, alignment: u8) -> Option<std::alloc::Layout> {
    std::alloc::Layout::from_size_align(len.max(1), 1usize.checked_shl(alignment.into())?).ok()
}
unsafe extern "C" fn alloc(ctx: *const Budget, len: usize, alignment: u8, _: usize) -> *mut u8 {
    let Some(layout) = layout(len, alignment) else { return std::ptr::null_mut() };
    let budget = unsafe { &*ctx };
    if !budget.take(layout.size()) {
        return std::ptr::null_mut();
    }
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
        budget.release(layout.size());
    }
    ptr
}
unsafe extern "C" fn resize(_: *const Budget, _: *mut u8, old: usize, _: u8, new: usize, _: usize) -> bool {
    old == new
}
unsafe extern "C" fn remap(_: *const Budget, _: *mut u8, _: usize, _: u8, _: usize, _: usize) -> *mut u8 {
    std::ptr::null_mut()
}
unsafe extern "C" fn free(ctx: *const Budget, ptr: *mut u8, len: usize, alignment: u8, _: usize) {
    if let Some(layout) = layout(len, alignment) {
        unsafe {
            std::alloc::dealloc(ptr, layout);
            (&*ctx).release(layout.size());
        }
    }
}
static VTABLE: Vtable = Vtable { alloc, resize, remap, free };

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn growth_and_release_are_bounded_even_during_copy() {
        let budget = Budget::new(12_288);
        let mut bytes = Bytes::new(&budget);
        bytes.write_all(&vec![1; 4096]).unwrap();
        bytes.write_all(&[2]).unwrap();
        assert_eq!(budget.peak(), 12_288);
        assert_eq!(budget.used(), 8192);
        assert!(bytes.write_all(&vec![3; 8192]).is_err());
        drop(bytes);
        assert_eq!(budget.used(), 0);
    }
}
