//! Realtime safety: `RigChain::process` and `RigChain::reset` never allocate (checked with a counting global
//! allocator) for every built-in stage kind, with and without resampling.

mod common;

use flux_nam::{ChainOptions, NoHooks, Quality, Rig, RigChain};
use common::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator, only counting calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn process_and_reset_never_allocate() {
    // the counter sees allocations at all
    let before = allocations();
    drop(std::hint::black_box(vec![1u8; 10]));
    assert!(allocations() > before, "the counting allocator is not installed");

    let dir = temp_dir("realtime");
    for (rate, quality) in [(48000u32, Quality::High), (44100, Quality::High), (44100, Quality::Scipy), (96000, Quality::High)] {
        let ir = write_ir(&dir, rate);
        let rig = Rig::parse(&full_rig_json(&ir), &dir, "full.rig.json").unwrap();
        let mut chain =
            RigChain::with_options(&rig, rate, 256, &mut NoHooks, ChainOptions { resample: quality, ..ChainOptions::default() }).unwrap();
        let mut x = guitar(rate as usize / 4, rate as f64);
        x[100] = f32::NAN;
        let before = allocations();
        for block in [256usize, 1, 37, 256, 255, 2, 1000] {
            for part in x.chunks_mut(block) {
                chain.process(part);
            }
            chain.reset();
        }
        assert_eq!(allocations() - before, 0, "{rate} Hz {quality:?}: allocated while processing");
        assert!(x.iter().all(|v| v.is_finite()));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
