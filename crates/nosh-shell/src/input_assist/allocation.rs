//! A worker-only fallback where RLIMIT_AS is advisory (notably Darwin).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static INSTALLED: AtomicBool = AtomicBool::new(false);
static REMAINING: AtomicUsize = AtomicUsize::new(usize::MAX);
const ALLOWANCE: usize = 128 * 1024 * 1024;
const RECYCLE_AT: usize = 64 * 1024 * 1024;

/// Install as the host's global allocator on platforms without an enforced
/// address-space limit. Until the explicit worker entry enables its budget,
/// allocations go straight to System and are not counted or limited.
pub struct WorkerAllocator;

impl WorkerAllocator {
    fn reserve(size: usize, align: usize) -> bool {
        if !INSTALLED.load(Ordering::Relaxed) {
            INSTALLED.store(true, Ordering::Relaxed);
        }
        if REMAINING.load(Ordering::Relaxed) == usize::MAX {
            return true;
        }
        let Some(charge) = size.checked_add(align).and_then(|n| n.checked_add(64)) else {
            return false;
        };
        REMAINING
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(charge)
            })
            .is_ok()
    }
}

// SAFETY: allocation/deallocation use System with the caller's exact layouts.
// Budget exhaustion returns null, as required by GlobalAlloc. No allocation,
// logging, panics, or locks are used while updating the budget.
unsafe impl GlobalAlloc for WorkerAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if Self::reserve(layout.size(), layout.align()) {
            unsafe { System.alloc(layout) }
        } else {
            std::ptr::null_mut()
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if Self::reserve(layout.size(), layout.align()) {
            unsafe { System.alloc_zeroed(layout) }
        } else {
            std::ptr::null_mut()
        }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if Self::reserve(size, layout.align()) {
            unsafe { System.realloc(pointer, layout, size) }
        } else {
            std::ptr::null_mut()
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // Deliberately do not refund frees: allocator arena retention and
        // fragmentation cannot turn bounded live allocations into unbounded RSS.
        unsafe { System.dealloc(pointer, layout) };
    }
}

pub(super) fn enable() -> bool {
    if !INSTALLED.load(Ordering::Relaxed) {
        return false;
    }
    let _ = REMAINING.compare_exchange(usize::MAX, ALLOWANCE, Ordering::Relaxed, Ordering::Relaxed);
    true
}

pub(super) fn recycle() -> bool {
    REMAINING.load(Ordering::Relaxed) < RECYCLE_AT
}

#[cfg(test)]
pub(super) fn remaining() -> usize {
    REMAINING.load(Ordering::Relaxed)
}
