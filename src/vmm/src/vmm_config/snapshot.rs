// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configurations used in the snapshotting context.

use std::path::PathBuf;

/// For crates that depend on `vmm` we export.
use roaring::RoaringBitmap;
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
    /// Backend snapshot: for a microVM with a memory backend attached, Firecracker does not write
    /// guest memory but returns the layout of the pages that changed since the last snapshot or
    /// `dirty-pages` call (the backend produces the memory from it), and writes the microVM state.
    /// Finalizes the incremental workflow begun with `PUT /snapshot/dirty-pages`.
    Backend,
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
    /// Path to the file that will contain the guest memory. Mandatory.
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

/// Describes, to a memory backend, which pages of the guest memory file a diff snapshot consists
/// of and where their content is. Returned by `PUT /snapshot/create` (`Diff`) and
/// `PUT /snapshot/create` (Backend) when a memory backend is attached. (`Full` snapshots are written
/// by Firecracker itself, backend or not.)
///
/// Every page of the file falls in exactly one of three classes:
///
/// - **memfd authoritative**: the backend must copy it from the memfd;
/// - **zero**: the backend must make it read as zero in the file it produces;
/// - **neither**: the page is unchanged since the dirty state was last consumed, so the base
///   the diff applies to already holds its content.
///
/// The classification is `dirty ∧ resident` / `dirty ∧ ¬resident` / `¬dirty`, where *dirty* is
/// the union of KVM's dirty log and Firecracker's own bitmap (every page written or discarded
/// since the dirty state was last consumed; every page of an unplugged virtio-mem slot) and
/// *resident* is `mincore(2)` on Firecracker's mapping, taken after the dirty state. Firecracker
/// upholds "dirty ⇒ memfd authoritative or zero" by faulting in every page it marks dirty ahead
/// of writing it ([`crate::vstate::memory::fault_in_marked_range`]).
///
/// Both sets are [Roaring bitmaps](https://roaringbitmap.org) of page indices (file offset /
/// `page_size`), sent in Roaring's portable serialization format, base64-encoded. Roaring
/// stores each 65536-page chunk as a sorted array (sparse), a plain bitmap (dense) or a run
/// list (long runs), so an idle guest, a released balloon and an unplugged hotplug region of
/// any size all cost a few bytes per chunk, a dense random dirty set costs at most 8 KiB per
/// chunk (the plain bitmap), and a backend can test any page in constant time without
/// decompressing anything.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct SnapshotMemoryLayout {
    /// Size of a full guest memory file (sum of all region sizes, including the virtio-mem
    /// hotplug region, plugged or not).
    pub total_size: u64,
    /// Granularity of both bitmaps, in bytes (the host page size).
    pub page_size: u64,
    /// How the bitmaps are encoded on the wire (before base64). Always `roaring` today; the
    /// field lets a backend fail cleanly if a future Firecracker changes the binary format.
    #[serde(default)]
    pub bitmap_encoding: BitmapEncoding,
    /// The pages (file offset / `page_size`) to copy from the memfd into the file at the same
    /// offset. Serialised as standard, padded base64 of the Roaring portable format.
    #[serde(with = "roaring_base64")]
    pub pages_to_copy: RoaringBitmap,
    /// The pages that must read as zero in the file. Disjoint from `pages_to_copy`.
    /// Serialised as standard, padded base64 of the Roaring portable format.
    #[serde(with = "roaring_base64")]
    pub pages_to_discard: RoaringBitmap,
}

/// Wire encoding of the bitmaps of a [`SnapshotMemoryLayout`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BitmapEncoding {
    /// Roaring bitmap, portable serialization format
    /// (<https://github.com/RoaringBitmap/RoaringFormatSpec>).
    #[default]
    Roaring,
}

impl SnapshotMemoryLayout {
    /// An empty layout: nothing authoritative, nothing zero.
    pub fn new(total_size: u64, page_size: u64) -> Self {
        Self {
            total_size,
            page_size,
            bitmap_encoding: BitmapEncoding::Roaring,
            pages_to_copy: RoaringBitmap::new(),
            pages_to_discard: RoaringBitmap::new(),
        }
    }

    /// Page index of file offset `offset`. Page indices are `u32`: 16 TiB of 4 KiB pages.
    pub fn page(&self, offset: u64) -> u32 {
        u32::try_from(offset / self.page_size).expect("offset beyond 16 TiB of pages")
    }

    /// Whether the page at file offset `offset` is to be copied from the memfd.
    pub fn page_is_authoritative(&self, offset: u64) -> bool {
        self.pages_to_copy.contains(self.page(offset))
    }

    /// Whether the page at file offset `offset` is to be zeroed.
    pub fn page_is_discarded(&self, offset: u64) -> bool {
        self.pages_to_discard.contains(self.page(offset))
    }

    /// Marks the page at file offset `offset` authoritative.
    pub fn set_authoritative(&mut self, offset: u64) {
        let page = self.page(offset);
        self.pages_to_copy.insert(page);
    }

    /// Marks the page at file offset `offset` zero.
    pub fn set_discarded(&mut self, offset: u64) {
        let page = self.page(offset);
        self.pages_to_discard.insert(page);
    }

    /// Builds the layout from the sets of `dirty` and `resident` pages (page indices):
    /// `dirty ∧ resident` is authoritative, `dirty ∧ ¬resident` is zero. This is the single
    /// definition of the classification. Both results are run-optimised for the wire.
    pub fn classify(
        total_size: u64,
        page_size: u64,
        dirty: &RoaringBitmap,
        resident: &RoaringBitmap,
    ) -> Self {
        let mut authoritative = dirty & resident;
        let mut zero = dirty - resident;
        authoritative.optimize();
        zero.optimize();
        Self {
            total_size,
            page_size,
            bitmap_encoding: BitmapEncoding::Roaring,
            pages_to_copy: authoritative,
            pages_to_discard: zero,
        }
    }

    /// Number of pages to copy from the memfd.
    pub fn authoritative_pages(&self) -> u64 {
        self.pages_to_copy.len()
    }

    /// Bytes to zero.
    pub fn discard_bytes(&self) -> u64 {
        self.pages_to_discard.len() * self.page_size
    }
}

/// Serde adapter: a [`RoaringBitmap`] as standard, padded base64 of its portable serialization.
mod roaring_base64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use roaring::RoaringBitmap;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        bitmap: &RoaringBitmap,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut bytes = Vec::with_capacity(bitmap.serialized_size());
        bitmap
            .serialize_into(&mut bytes)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<RoaringBitmap, D::Error> {
        let bytes = STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)?;
        RoaringBitmap::deserialize_from(&bytes[..]).map_err(serde::de::Error::custom)
    }
}

/// Body of a successful `PUT /snapshot/create` (Backend) or `PUT /snapshot/dirty-pages` when a memory
/// backend is attached.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct SnapshotMemoryResponse {
    /// The snapshot type, present only for `PUT /snapshot/create`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_type: Option<SnapshotType>,
    /// The memory layout.
    pub memory: SnapshotMemoryLayout,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(layout: &RoaringBitmap) -> Vec<u32> {
        layout.iter().collect()
    }

    #[test]
    fn test_snapshot_memory_layout_bits() {
        let mut layout = SnapshotMemoryLayout::new(16 * 4096, 4096);
        assert!(layout.pages_to_copy.is_empty());
        assert!(layout.pages_to_discard.is_empty());
        assert_eq!(layout.authoritative_pages(), 0);
        assert_eq!(layout.discard_bytes(), 0);
        assert!(!layout.page_is_authoritative(0));
        assert!(!layout.page_is_discarded(0));

        layout.set_authoritative(0);
        layout.set_authoritative(12 * 4096);
        assert_eq!(pages(&layout.pages_to_copy), vec![0, 12]);
        assert!(layout.page_is_authoritative(0));
        assert!(!layout.page_is_authoritative(4096));
        assert!(layout.page_is_authoritative(12 * 4096));
        assert!(!layout.page_is_authoritative(1 << 40));
        assert_eq!(layout.authoritative_pages(), 2);

        layout.set_discarded(8 * 4096);
        layout.set_discarded(9 * 4096);
        assert_eq!(pages(&layout.pages_to_discard), vec![8, 9]);
        assert!(layout.page_is_discarded(8 * 4096));
        assert!(layout.page_is_discarded(9 * 4096));
        assert!(!layout.page_is_discarded(10 * 4096));
        assert_eq!(layout.discard_bytes(), 2 * 4096);
    }

    #[test]
    fn test_snapshot_memory_layout_classify() {
        // 20 pages: dirty 0,1,3,8..16, resident everything but 3 and 8..16.
        let mut dirty = RoaringBitmap::new();
        dirty.insert(0);
        dirty.insert(1);
        dirty.insert(3);
        dirty.insert_range(8..16);
        let mut resident = RoaringBitmap::new();
        resident.insert_range(0..20);
        resident.remove(3);
        resident.remove_range(8..16);
        let layout = SnapshotMemoryLayout::classify(20 * 4096, 4096, &dirty, &resident);
        assert_eq!(pages(&layout.pages_to_copy), vec![0, 1]);
        assert_eq!(
            pages(&layout.pages_to_discard),
            vec![3, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(layout.authoritative_pages(), 2);
        assert_eq!(layout.discard_bytes(), 9 * 4096);
        assert!((&layout.pages_to_copy & &layout.pages_to_discard).is_empty());
        assert_eq!(layout, {
            let mut expected = SnapshotMemoryLayout::new(20 * 4096, 4096);
            expected.set_authoritative(0);
            expected.set_authoritative(4096);
            expected.set_discarded(3 * 4096);
            for p in 8..16 {
                expected.set_discarded(p * 4096);
            }
            expected
        });
    }

    #[test]
    fn test_snapshot_memory_layout_json() {
        // The example of docs/snapshotting/shared-memfd-design.md: a 64 KiB guest, pages 0, 1
        // and 12 authoritative, pages 8..12 (an unplugged slot) zero. Roaring portable format,
        // no run containers (tiny sets stay array containers).
        let mut layout = SnapshotMemoryLayout::new(16 * 4096, 4096);
        layout.set_authoritative(0);
        layout.set_authoritative(4096);
        layout.set_authoritative(12 * 4096);
        for page in 8..12 {
            layout.set_discarded(page * 4096);
        }
        let json = serde_json::to_string(&layout).unwrap();
        assert_eq!(
            json,
            r#"{"total_size":65536,"page_size":4096,"bitmap_encoding":"roaring","pages_to_copy":"OjAAAAEAAAAAAAIAEAAAAAAAAQAMAA==","pages_to_discard":"OjAAAAEAAAAAAAMAEAAAAAgACQAKAAsA"}"#
        );
        let back: SnapshotMemoryLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(back, layout);

        // A 1 GiB guest with nothing dirty: two empty bitmaps, 8 bytes each.
        let empty = SnapshotMemoryLayout::new(1 << 30, 4096);
        let json = serde_json::to_string(&empty).unwrap();
        assert!(json.len() < 200, "{json}");
        assert_eq!(
            serde_json::from_str::<SnapshotMemoryLayout>(&json).unwrap(),
            empty
        );

        // A 1 GiB guest entirely unplugged: one run per 65536-page chunk, 4 chunks.
        let mut unplugged = RoaringBitmap::new();
        unplugged.insert_range(0..(1u32 << 18));
        let layout =
            SnapshotMemoryLayout::classify(1 << 30, 4096, &unplugged, &RoaringBitmap::new());
        assert!(
            layout.pages_to_discard.serialized_size() < 64,
            "{}",
            layout.pages_to_discard.serialized_size()
        );

        let response = SnapshotMemoryResponse {
            snapshot_type: Some(SnapshotType::Diff),
            memory: layout.clone(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.starts_with(r#"{"snapshot_type":"Diff","memory":{"#));
        assert_eq!(
            serde_json::from_str::<SnapshotMemoryResponse>(&json).unwrap(),
            response
        );

        // Invalid base64 or Roaring is rejected, in either bitmap; an unknown encoding too;
        // the encoding field defaults.
        for bad in [
            r#"{"total_size":4096,"page_size":4096,"bitmap_encoding":"roaring","pages_to_copy":"!!","pages_to_discard":"OjAAAAAAAAA="}"#,
            r#"{"total_size":4096,"page_size":4096,"bitmap_encoding":"roaring","pages_to_copy":"OjAAAAAAAAA=","pages_to_discard":"!!"}"#,
            r#"{"total_size":4096,"page_size":4096,"bitmap_encoding":"roaring","pages_to_copy":"AQ==","pages_to_discard":"OjAAAAAAAAA="}"#,
            r#"{"total_size":4096,"page_size":4096,"bitmap_encoding":"packbits","pages_to_copy":"OjAAAAAAAAA=","pages_to_discard":"OjAAAAAAAAA="}"#,
        ] {
            serde_json::from_str::<SnapshotMemoryLayout>(bad).unwrap_err();
        }
        let empty: SnapshotMemoryLayout = serde_json::from_str(
            r#"{"total_size":4096,"page_size":4096,"pages_to_copy":"OjAAAAAAAAA=","pages_to_discard":"OjAAAAAAAAA="}"#,
        )
        .unwrap();
        assert_eq!(empty, SnapshotMemoryLayout::new(4096, 4096));
    }
}
