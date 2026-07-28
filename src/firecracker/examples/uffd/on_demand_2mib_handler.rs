// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! On-demand UFFD page fault handler that resolves each fault with a 2MiB
//! UFFDIO_COPY (aligned to the PMD boundary).  This allows the kernel to
//! install a single PMD-level THP mapping per fault instead of returning
//! one 4KiB page at a time.
//!
//! Based on `on_demand_handler.rs`.

mod uffd_utils;

use std::fs::File;
use std::os::unix::net::UnixListener;

use uffd_utils::{Runtime, UffdHandler};

/// 2 MiB — PMD-level huge page size on x86_64.
const THP_SIZE: usize = 2 * 1024 * 1024;

fn main() {
    let mut args = std::env::args();
    let uffd_sock_path = args.nth(1).expect("No socket path given");
    let mem_file_path = args.next().expect("No memory file given");

    let file = File::open(mem_file_path).expect("Cannot open memfile");

    let listener = UnixListener::bind(uffd_sock_path).expect("Cannot bind to socket path");
    let (stream, _) = listener.accept().expect("Cannot listen on UDS socket");

    let mut runtime = Runtime::new(stream, file);
    runtime.install_panic_hook();
    runtime.run(|uffd_handler: &mut UffdHandler| {
        let mut deferred_events = Vec::new();

        loop {
            let mut events_to_handle = Vec::from_iter(deferred_events.drain(..));

            while let Some(event) = uffd_handler.read_event().expect("Failed to read uffd_msg") {
                events_to_handle.push(event);
            }

            for event in events_to_handle.drain(..) {
                match event {
                    userfaultfd::Event::Pagefault { addr, .. } => {
                        let fault_addr = addr as usize;

                        // Try 2MiB-aligned copy first; fall back to page-sized.
                        let served = try_serve_thp(uffd_handler, fault_addr)
                            || uffd_handler.serve_pf(addr.cast(), uffd_handler.page_size);

                        if !served {
                            deferred_events.push(event);
                        }
                    }
                    userfaultfd::Event::Remove { start, end } => {
                        uffd_handler.unregister_range(start, end)
                    }
                    _ => panic!("Unexpected event on userfaultfd"),
                }
            }

            if deferred_events.is_empty() {
                break;
            }
        }
    });
}

/// Attempt to serve a page fault by copying the full 2MiB chunk containing it.
/// Returns true on success, false if alignment/region constraints prevent it or
/// the UFFDIO_COPY fails (e.g. EAGAIN from a pending remove event).
fn try_serve_thp(uffd_handler: &mut UffdHandler, fault_addr: usize) -> bool {
    let aligned_addr = fault_addr & !(THP_SIZE - 1);

    // The entire 2MiB range must fall within a single guest memory region.
    for region in uffd_handler.mem_regions.iter() {
        let region_start = region.base_host_virt_addr as usize;
        let region_end = region_start + region.size;

        if aligned_addr >= region_start && aligned_addr + THP_SIZE <= region_end {
            return uffd_handler.serve_pf(aligned_addr as *mut u8, THP_SIZE);
        }
    }

    false
}
