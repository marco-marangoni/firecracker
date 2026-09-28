// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configurations used in the snapshotting context.

use std::path::PathBuf;

/// For crates that depend on `vmm` we export.
use serde::{Deserialize, Serialize};

use super::machine_config::HugePageConfig;

/// The snapshot type options that are available when
/// creating a new snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum SnapshotType {
    /// Diff snapshot.
    Diff,
    /// Full snapshot.
    #[default]
    Full,
}

/// Specifies the method through which guest memory will get populated when
/// resuming from a snapshot:
/// 1) A file that contains the guest memory to be loaded,
/// 2) An UDS where a custom page-fault handler process is listening for the UFFD set up by
///    Firecracker to handle its guest memory page faults.
/// 3) Like 2), but guest memory is backed by a single memfd which is handed to the page-fault
///    handler together with the UFFD, so that the handler can produce memory snapshots itself.
///    This is also the only variant accepted in `machine-config.mem_backend` (boot), where only
///    the memfd is handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum MemBackendType {
    /// Guest memory contents will be loaded from a file.
    File,
    /// Guest memory will be served through UFFD by a separate process.
    Uffd,
    /// Guest memory is shared with a separate process through a memfd. On restore the process
    /// also serves page faults through UFFD.
    SharedMemfd,
}

/// Stores the configuration that will be used for creating a snapshot.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSnapshotParams {
    /// This marks the type of snapshot we want to create.
    /// The default value is `Full`, which means a full snapshot.
    #[serde(default = "SnapshotType::default")]
    pub snapshot_type: SnapshotType,
    /// Path to the file that will contain the microVM state.
    pub snapshot_path: PathBuf,
    /// Path to the file that will contain the guest memory. Mandatory unless a memory backend
    /// is attached to the microVM, in which case it must be absent.
    #[serde(default)]
    pub mem_file_path: Option<PathBuf>,
    /// Whether to fsync the snapshot state and guest memory files.
    /// Activated virtio-block devices are always fsync'd, independently of this.
    #[serde(default = "default_sync_snapshot_files")]
    pub sync_snapshot_files: bool,
}

/// Default value for [CreateSnapshotParams::sync_snapshot_files].
fn default_sync_snapshot_files() -> bool {
    true
}

/// Allows for changing the mapping between tap devices and host devices
/// during snapshot restore
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct NetworkOverride {
    /// The index of the interface to modify
    pub iface_id: String,
    /// The new name of the interface to be assigned
    pub host_dev_name: String,
}

/// Allows for changing the host UDS of the vsock backend during snapshot restore
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct VsockOverride {
    /// The path to the UDS that will be used for the vsock interface
    pub uds_path: String,
}

/// Selects the huge-page configuration to use when loading a snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum SnapshotLoadHugePageConfig {
    /// Reuse the huge-page configuration serialized in the snapshot.
    #[default]
    Snapshot,
    /// Use the host's default memory-mapping behavior.
    None,
    /// Advise the kernel to use transparent huge pages for guest memory.
    Transparent,
    /// Back guest memory by 2 MiB hugetlbfs pages.
    #[serde(rename = "2M")]
    Hugetlbfs2M,
}

impl SnapshotLoadHugePageConfig {
    /// Resolves the configuration against the value serialized in the snapshot.
    pub fn resolve(self, snapshot: HugePageConfig) -> HugePageConfig {
        match self {
            Self::Snapshot => snapshot,
            Self::None => HugePageConfig::None,
            Self::Transparent => HugePageConfig::Transparent,
            Self::Hugetlbfs2M => HugePageConfig::Hugetlbfs2M,
        }
    }
}

/// Stores the configuration that will be used for loading a snapshot.
#[derive(Debug, PartialEq, Eq)]
pub struct LoadSnapshotParams {
    /// Path to the file that contains the microVM state to be loaded.
    pub snapshot_path: PathBuf,
    /// Specifies guest memory backend configuration.
    pub mem_backend: MemBackendConfig,
    /// Whether KVM dirty page tracking should be enabled, to space optimization
    /// of differential snapshots.
    pub track_dirty_pages: bool,
    /// When set to true, the vm is also resumed if the snapshot load
    /// is successful.
    pub resume_vm: bool,
    /// The network devices to override on load.
    pub network_overrides: Vec<NetworkOverride>,
    /// When set, the vsock backend UDS path will be overridden
    pub vsock_override: Option<VsockOverride>,
    /// [x86_64 only] When set to true, passes `KVM_CLOCK_REALTIME` to `KVM_SET_CLOCK` on restore,
    /// advancing kvmclock by the wall-clock time elapsed since the snapshot was taken. When false
    /// (default), kvmclock resumes from where it was at snapshot time.
    pub clock_realtime: bool,
    /// Selects the huge-page configuration to use for the restored microVM.
    pub huge_pages: SnapshotLoadHugePageConfig,
}

/// Stores the configuration for loading a snapshot that is provided by the user.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadSnapshotConfig {
    /// Path to the file that contains the microVM state to be loaded.
    pub snapshot_path: PathBuf,
    /// Path to the file that contains the guest memory to be loaded. To be used only if
    /// `mem_backend` is not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_file_path: Option<PathBuf>,
    /// Guest memory backend configuration. Is not to be used in conjunction with `mem_file_path`.
    /// None value is allowed only if `mem_file_path` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_backend: Option<MemBackendConfig>,
    /// Whether or not to enable KVM dirty page tracking.
    #[serde(default)]
    #[deprecated]
    pub enable_diff_snapshots: bool,
    /// Whether KVM dirty page tracking should be enabled.
    #[serde(default)]
    pub track_dirty_pages: bool,
    /// Whether or not to resume the vm post snapshot load.
    #[serde(default)]
    pub resume_vm: bool,
    /// The network devices to override on load.
    #[serde(default)]
    pub network_overrides: Vec<NetworkOverride>,
    /// Whether or not to override the vsock backend UDS path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vsock_override: Option<VsockOverride>,
    /// [x86_64 only] When set to true, passes `KVM_CLOCK_REALTIME` to `KVM_SET_CLOCK` on restore.
    #[serde(default)]
    pub clock_realtime: bool,
    /// Selects the huge-page configuration to use for the restored microVM.
    #[serde(default)]
    pub huge_pages: SnapshotLoadHugePageConfig,
}

/// Stores the configuration used for managing snapshot memory.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemBackendConfig {
    /// Path to the backend used to handle the guest memory.
    pub backend_path: PathBuf,
    /// Specifies the guest memory backend type.
    pub backend_type: MemBackendType,
}

/// The microVM state options.
#[derive(Debug, Deserialize, Serialize)]
pub enum VmState {
    /// The microVM is paused, which means that we can create a snapshot of it.
    Paused,
    /// The microVM is resumed; this state should be set after we load a snapshot.
    Resumed,
}

/// Keeps the microVM state necessary in the snapshotting context.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Vm {
    /// The microVM state, which can be `paused` or `resumed`.
    pub state: VmState,
}

/// A page-aligned byte range, in guest memory file offset space (which is also the offset space
/// of the memfd handed to a memory backend).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MemoryRange {
    /// Offset of the first byte of the range.
    pub offset: u64,
    /// Length of the range in bytes.
    pub len: u64,
}

/// Describes which pages of the guest memory file the pages dirtied since the dirty state was
/// last consumed occupy. This is the reply to a `DirtyPages` request on a `SharedMemfd`
/// backend connection: the fields other than `pages` form the JSON header of the reply frame,
/// `pages` travels as the raw binary payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SnapshotMemoryLayout {
    /// Size of a full guest memory file (sum of all region sizes).
    pub total_size: u64,
    /// Granularity of `pages`, in bytes (the host page size).
    pub page_size: u64,
    /// One bit per `page_size` bytes of the memory file: byte `i`, bit `b` (least significant
    /// first) is the page at offset `(8 * i + b) * page_size`. A set bit means the page must be
    /// copied from guest memory into the memory file at the same offset: every plugged page
    /// whose content changed since the dirty state was last consumed is set (the pages
    /// `dump_dirty` writes). Bits of currently unplugged slots are never set; `unplugged`
    /// describes those.
    ///
    /// The bitmap covers the file from offset 0 up to the end of the last plugged slot,
    /// `ceil(plugged_end / page_size / 8)` bytes, whatever is dirty: its length depends on the
    /// plug state only, and unplugged memory at the end of the file (the hotplug region) costs
    /// nothing, however large. Pages past its end are clear.
    ///
    /// Never serialised as JSON: it is the binary payload of the reply frame.
    #[serde(skip)]
    pub pages: Vec<u8>,
    /// Ranges that must be zeroed in the memory file: the currently unplugged virtio-mem slots.
    /// Sorted, merged and page-aligned.
    pub unplugged: Vec<MemoryRange>,
}

impl SnapshotMemoryLayout {
    /// Creates a layout with an all-clear bitmap covering the file up to `plugged_end`, the end
    /// offset of the last plugged slot.
    pub fn new(total_size: u64, page_size: u64, plugged_end: u64) -> Self {
        let pages = usize::try_from(plugged_end.div_ceil(page_size)).unwrap_or(usize::MAX);
        Self {
            total_size,
            page_size,
            pages: vec![0u8; pages.div_ceil(8)],
            unplugged: Vec::new(),
        }
    }

    /// Whether the page at file offset `offset` must be copied.
    pub fn page_is_set(&self, offset: u64) -> bool {
        let page = offset / self.page_size;
        let byte = usize::try_from(page / 8).unwrap();
        self.pages
            .get(byte)
            .is_some_and(|b| b & (1 << (page % 8)) != 0)
    }

    /// Whether the page at file offset `offset` is a plugged page (part of a full copy).
    pub fn page_is_plugged(&self, offset: u64) -> bool {
        offset < self.total_size
            && !self
                .unplugged
                .iter()
                .any(|r| r.offset <= offset && offset < r.offset + r.len)
    }

    /// Sets the bits of the `len` bytes at file offset `offset` (both `page_size`-aligned).
    ///
    /// # Panics
    ///
    /// If the range lies past the end of the bitmap.
    pub fn set_range(&mut self, offset: u64, len: u64) {
        let first = offset / self.page_size;
        let last = (offset + len).div_ceil(self.page_size);
        for page in first..last {
            let byte = usize::try_from(page / 8).unwrap();
            self.pages[byte] |= 1 << (page % 8);
        }
    }

    /// Number of set pages.
    pub fn set_pages(&self) -> u64 {
        self.pages.iter().map(|b| u64::from(b.count_ones())).sum()
    }

    /// Number of plugged pages (the pages of a full copy).
    pub fn plugged_pages(&self) -> u64 {
        let unplugged: u64 = self.unplugged.iter().map(|r| r.len).sum();
        (self.total_size - unplugged) / self.page_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_memory_layout_bits() {
        // 16 pages, all plugged.
        let mut layout = SnapshotMemoryLayout::new(16 * 4096, 4096, 16 * 4096);
        assert_eq!(layout.pages, vec![0u8, 0]);
        assert_eq!(layout.set_pages(), 0);
        assert_eq!(layout.plugged_pages(), 16);

        layout.set_range(0, 2 * 4096);
        layout.set_range(8 * 4096, 4096);
        assert_eq!(layout.pages, vec![0b11u8, 0b1]);
        assert_eq!(layout.set_pages(), 3);
        assert!(layout.page_is_set(0));
        assert!(layout.page_is_set(4096));
        assert!(!layout.page_is_set(2 * 4096));
        assert!(layout.page_is_set(8 * 4096));
        assert!(!layout.page_is_set(1 << 40));

        // The bitmap covers the plugged part of the file only, rounded up to whole bytes; its
        // length does not depend on what is set.
        let layout = SnapshotMemoryLayout::new(64 * 4096, 4096, 13 * 4096);
        assert_eq!(layout.pages.len(), 2);
        assert!(!layout.page_is_set(63 * 4096));
        let layout = SnapshotMemoryLayout::new(64 * 4096, 4096, 0);
        assert!(layout.pages.is_empty());

        // Plugged pages: everything but `unplugged`.
        let mut layout = SnapshotMemoryLayout::new(16 * 4096, 4096, 8 * 4096);
        layout.unplugged.push(MemoryRange {
            offset: 8 * 4096,
            len: 4 * 4096,
        });
        assert_eq!(layout.plugged_pages(), 12);
        assert!(layout.page_is_plugged(0));
        assert!(layout.page_is_plugged(7 * 4096));
        assert!(!layout.page_is_plugged(8 * 4096));
        assert!(!layout.page_is_plugged(11 * 4096));
        assert!(layout.page_is_plugged(12 * 4096));
        assert!(!layout.page_is_plugged(16 * 4096));
    }

    #[test]
    fn test_snapshot_memory_layout_json() {
        // The example of docs/snapshotting/shared-memfd-design.md: pages 0, 1 and 12 set, the
        // slot at pages 8..12 unplugged. The bitmap is not part of the JSON.
        let mut layout = SnapshotMemoryLayout::new(16 * 4096, 4096, 16 * 4096);
        layout.set_range(0, 2 * 4096);
        layout.set_range(12 * 4096, 4096);
        layout.unplugged.push(MemoryRange {
            offset: 8 * 4096,
            len: 4 * 4096,
        });
        let json = serde_json::to_string(&layout).unwrap();
        assert_eq!(
            json,
            r#"{"total_size":65536,"page_size":4096,"unplugged":[{"offset":32768,"len":16384}]}"#
        );
        assert_eq!(layout.pages, vec![0x03, 0x10]);
        let back: SnapshotMemoryLayout = serde_json::from_str(&json).unwrap();
        assert!(back.pages.is_empty());
        assert_eq!(back.unplugged, layout.unplugged);
    }
}
