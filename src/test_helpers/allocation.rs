//! Actual Rust allocation high-water instrumentation for synchronous production paths.
//!
//! Measurements are local to the calling thread, so unrelated parallel tests cannot
//! add noise. Warm up external libraries and construct inputs before measuring; keep
//! the operation synchronous and do not move allocations between threads. The peak
//! is net live Rust heap growth above entry, not RSS or C++ toolkit allocations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Default)]
struct Measurement {
    active: bool,
    live: i128,
    peak: i128,
    largest: usize,
}

thread_local! {
    static MEASUREMENT: Cell<Measurement> = const { Cell::new(Measurement {
        active: false, live: 0, peak: 0, largest: 0,
    }) };
}

struct MeasuringAllocator;

#[global_allocator]
static ALLOCATOR: MeasuringAllocator = MeasuringAllocator;

fn record(allocated: usize, released: usize) {
    // Const TLS initialization and Cell access allocate nothing. During TLS
    // destruction try_with fails harmlessly instead of panicking in an allocator.
    let _ = MEASUREMENT.try_with(|slot| {
        let mut value = slot.get();
        if value.active {
            value.live += allocated as i128 - released as i128;
            value.peak = value.peak.max(value.live);
            value.largest = value.largest.max(allocated);
            slot.set(value);
        }
    });
}

// SAFETY: every allocation operation forwards the original pointer/layout to
// System and returns its unchanged result. Accounting neither dereferences nor
// retains allocation pointers, changes layouts, or allocates recursively.
unsafe impl GlobalAlloc for MeasuringAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies the valid nonzero layout required by System.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record(layout.size(), 0);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies the valid nonzero layout required by System.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record(layout.size(), 0);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record(0, layout.size());
        // SAFETY: pointer and layout are passed unchanged from GlobalAlloc's caller.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: pointer, layout and new_size are passed unchanged from the caller.
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if !replacement.is_null() {
            record(new_size, layout.size());
        }
        replacement
    }
}

#[derive(Debug)]
pub(crate) struct HeapHighWater {
    pub(crate) peak_growth: usize,
    pub(crate) largest_allocation: usize,
}

struct MeasurementGuard;

impl Drop for MeasurementGuard {
    fn drop(&mut self) {
        MEASUREMENT.with(|slot| slot.set(Measurement::default()));
    }
}

pub(crate) fn measure_heap_high_water<T>(operation: impl FnOnce() -> T) -> (T, HeapHighWater) {
    MEASUREMENT.with(|slot| {
        assert!(!slot.get().active, "allocation measurements cannot nest");
        slot.set(Measurement {
            active: true,
            ..Measurement::default()
        });
    });
    let guard = MeasurementGuard;
    let result = operation();
    let measurement = MEASUREMENT.with(Cell::get);
    drop(guard);
    (
        result,
        HeapHighWater {
            peak_growth: usize::try_from(measurement.peak).unwrap(),
            largest_allocation: measurement.largest,
        },
    )
}

#[test]
fn high_water_tracks_real_overlapping_allocations_and_reallocation() {
    let (_, measured) = measure_heap_high_water(|| {
        let mut first = vec![1_u8; 4096];
        std::hint::black_box(&first);
        let second = vec![2_u8; 8192];
        std::hint::black_box(&second);
        first.reserve_exact(4096);
        std::hint::black_box(&first);
        drop(second);
        drop(first);
    });
    assert_eq!(measured.peak_growth, 16_384);
    assert_eq!(measured.largest_allocation, 8192);
}
