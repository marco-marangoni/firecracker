// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// How expensive is it to report a virtqueue used-ring write to dirty page tracking?
//
// Compares, for a batch of N used-element writes followed by the index update:
//   * `batch_flag`:  set a bool per write, one `mark_dirty_host` per batch (flag approach)
//   * `per_write_lookup`: `mem.mark_dirty_host` after every write (region scan + bitmap)
//   * `per_write_cached`: `AtomicBitmap::set_addr_range` after every write, with the bitmap
//     pointer and region offset resolved once up front
//   * `per_write_set_bit`: as above, but the page index precomputed too, so one `fetch_or`
//   * `no_tracking`: the writes alone, as a floor

#![allow(clippy::cast_possible_truncation, clippy::undocumented_unsafe_blocks)]

use criterion::{Criterion, criterion_group, criterion_main};
use vm_memory::bitmap::AtomicBitmap;
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
use vmm::test_utils::single_region_mem_dirty_tracking;
use vmm::vstate::memory::GuestMemoryExtension;

const PAGE: usize = 4096;
// A 256-entry used ring: 4 + 8 * 256 + 2 bytes, spanning parts of two pages.
const RING_LEN: usize = 4 + 8 * 256 + 2;
const RING_GPA: u64 = 1 << 20;

#[inline(always)]
unsafe fn write_elem(base: *mut u8, i: usize) {
    unsafe { base.add(4 + 8 * i).cast::<u64>().write_volatile(i as u64) };
}
#[inline(always)]
unsafe fn write_idx(base: *mut u8, v: u16) {
    unsafe { base.add(2).cast::<u16>().write_volatile(v) };
}

pub fn bench(c: &mut Criterion) {
    let mem = single_region_mem_dirty_tracking(16 << 20);
    let region = mem.find_region(GuestAddress(RING_GPA)).unwrap();
    let slice = mem.get_slice(GuestAddress(RING_GPA), RING_LEN).unwrap();
    let base = slice.ptr_guard_mut().as_ptr();
    // The region's own bitmap (`MmapRegion::bitmap`), not the trait's slice view.
    let bitmap: &AtomicBitmap = (**region).bitmap().as_ref().unwrap();
    let ring_off = (RING_GPA - region.start_addr().0) as usize;

    for &n in &[1usize, 4, 16, 64] {
        let mut g = c.benchmark_group(format!("used_ring_batch_{n}"));

        g.bench_function("no_tracking", |b| {
            b.iter(|| unsafe {
                for i in 0..n {
                    write_elem(base, i);
                }
                write_idx(base, n as u16);
            })
        });

        g.bench_function("batch_flag", |b| {
            b.iter(|| unsafe {
                let mut dirty = false;
                for i in 0..n {
                    write_elem(base, i);
                    dirty = true;
                }
                write_idx(base, n as u16);
                dirty = std::hint::black_box(dirty);
                if dirty {
                    mem.mark_dirty_host(base, RING_LEN);
                }
            })
        });

        g.bench_function("per_write_lookup", |b| {
            b.iter(|| unsafe {
                for i in 0..n {
                    write_elem(base, i);
                    mem.mark_dirty_host(base.add(4 + 8 * i), 8);
                }
                write_idx(base, n as u16);
                mem.mark_dirty_host(base.add(2), 2);
            })
        });

        g.bench_function("per_write_cached", |b| {
            b.iter(|| unsafe {
                for i in 0..n {
                    write_elem(base, i);
                    bitmap.set_addr_range(ring_off + 4 + 8 * i, 8);
                }
                write_idx(base, n as u16);
                bitmap.set_addr_range(ring_off + 2, 2);
            })
        });

        g.bench_function("per_write_set_bit", |b| {
            b.iter(|| unsafe {
                for i in 0..n {
                    write_elem(base, i);
                    bitmap.set_bit((ring_off + 4 + 8 * i) / PAGE);
                }
                write_idx(base, n as u16);
                bitmap.set_bit((ring_off + 2) / PAGE);
            })
        });
        g.finish();
    }

    // The real thing: `Queue::add_used` × n + `advance_used_ring_idx`, which go through
    // `GuestMemorySliceMut::write_obj`, with and without dirty page tracking on the memory.
    use std::num::Wrapping;
    use vmm::devices::virtio::test_utils::VirtQueue;
    use vmm::test_utils::single_region_mem;
    for (label, m) in [
        ("tracked", single_region_mem_dirty_tracking(16 << 20)),
        ("untracked", single_region_mem(16 << 20)),
    ] {
        let vq = VirtQueue::new(GuestAddress(0), &m, 256);
        let mut q = vq.create_queue();
        q.ready = true;
        q.initialize(&m).unwrap();
        for &n in &[1usize, 16, 64] {
            c.bench_function(&format!("queue_add_used_{n}/{label}"), |b| {
                b.iter(|| {
                    q.next_used = Wrapping(0);
                    for i in 0..n {
                        q.add_used(i as u16, 0x1000).unwrap();
                    }
                    q.advance_used_ring_idx();
                })
            });
        }
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(200);
    targets = bench
}
criterion_main!(benches);
