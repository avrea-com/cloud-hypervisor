// Copyright 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! QCOW2 disk image format.
//!
//! Provides [`QcowDisk`], the `DiskFile` wrapper for QCOW2 images
//! with backing file and compression support.

mod backing;
mod common;
mod decoder;
mod engine_sync;
#[cfg(feature = "io_uring")]
mod engine_uring;
mod header;
mod metadata;
mod parser;
mod qcow_raw_file;
mod refcount;
mod util;
mod vec_cache;

use std::fs::File;
use std::os::unix::io::AsRawFd;
#[cfg(any(test, feature = "test-utils"))]
use std::path::Path;
use std::sync::Arc;
use std::{fmt, io};

pub use parser::{
    BackingFileConfig, CompressionType, Error, ImageType, IncompatFeatures, MissingFeatureError,
    QcowHeader,
};
#[cfg(any(test, feature = "test-utils"))]
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
#[cfg(any(test, feature = "test-utils"))]
use vmm_sys_util::tempfile::TempFile;

use self::backing::shared_backing_from;
use self::engine_sync::QcowSync;
#[cfg(feature = "io_uring")]
use self::engine_uring::QcowAsync;
use self::metadata::{BackingRead, QcowMetadata};
use self::parser::{MAX_NESTING_DEPTH, parse_qcow};
use self::qcow_raw_file::QcowRawFile;
use crate::aligned_file::AlignedFile;
#[cfg(any(test, feature = "test-utils"))]
use crate::async_io::GuestMemoryTarget;
use crate::async_io::{AsyncIo, BorrowedDiskFd, DiskFileError};
use crate::disk_file;
#[cfg(any(test, feature = "test-utils"))]
use crate::disk_file::AsyncDiskFile;
use crate::error::{BlockError, BlockErrorKind, BlockResult, ErrorOp};

/// Unified DiskFile wrapper for QCOW2 disk images.
///
/// Holds the in memory QCOW2 metadata, the data file, and an optional
/// backing file. The metadata is wrapped in an `Arc` because
/// [`QcowSync`] and [`QcowAsync`] I/O workers receive a clone when
/// they are created via [`create_async_io`](DiskFile::create_async_io).
/// The backing file is likewise shared with workers through an `Arc`.
///
/// The `sparse` flag controls whether the image advertises discard
/// support to the guest. The `use_io_uring` flag selects between the
/// [`QcowSync`] and [`QcowAsync`] I/O backends. Both are recorded at
/// construction time and propagated through [`try_clone`](DiskFile::try_clone).
pub struct QcowDisk {
    metadata: Arc<QcowMetadata>,
    backing_file: Option<Arc<dyn BackingRead>>,
    sparse: bool,
    data_raw_file: QcowRawFile,
    use_io_uring: bool,
}

impl fmt::Debug for QcowDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QcowDisk")
            .field("sparse", &self.sparse)
            .field("has_backing", &self.backing_file.is_some())
            .field("use_io_uring", &self.use_io_uring)
            .finish_non_exhaustive()
    }
}

impl QcowDisk {
    pub fn new(
        file: File,
        direct_io: bool,
        backing_files: bool,
        sparse: bool,
        use_io_uring: bool,
    ) -> BlockResult<Self> {
        #[cfg(not(feature = "io_uring"))]
        if use_io_uring {
            return Err(BlockError::new(
                BlockErrorKind::UnsupportedFeature,
                DiskFileError::NewAsyncIo(io::Error::other(
                    "io_uring requested but feature is not enabled",
                )),
            ));
        }

        let max_nesting_depth = if backing_files { MAX_NESTING_DEPTH } else { 0 };
        let raw_file = AlignedFile::new(file, direct_io);
        let (inner, backing_file, sparse) = parse_qcow(raw_file, max_nesting_depth, sparse)
            .map_err(|e| {
                let e = if !backing_files && matches!(e.kind(), BlockErrorKind::Overflow) {
                    e.with_kind(BlockErrorKind::UnsupportedFeature)
                } else {
                    e
                };
                e.with_op(ErrorOp::Open)
            })?;
        let data_raw_file = inner.raw_file.clone();
        Ok(QcowDisk {
            metadata: Arc::new(QcowMetadata::new(inner)),
            backing_file: backing_file.map(shared_backing_from).transpose()?,
            sparse,
            data_raw_file,
            use_io_uring,
        })
    }

    /// Synchronous write convenience for tests and benchmarks.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn write_all_at(&self, offset: u64, data: &[u8]) {
        let mut async_io = self.create_async_io(1).unwrap();
        let mem =
            Arc::new(GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), data.len())]).unwrap());
        mem.write_slice(data, GuestAddress(0)).unwrap();
        let range = [(GuestAddress(0), data.len() as u32)];
        let target = GuestMemoryTarget::new(Arc::clone(&mem), &range).unwrap();
        async_io
            .write_from_memory(offset as libc::off_t, target, 0)
            .unwrap();
        while async_io.next_completed_request().is_some() {}
    }

    /// Synchronous read convenience for tests and benchmarks.
    #[cfg(test)]
    pub fn read_all_at(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut async_io = self.create_async_io(1).unwrap();
        let mem = Arc::new(GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), len)]).unwrap());
        let range = [(GuestAddress(0), len as u32)];
        let target = GuestMemoryTarget::new(Arc::clone(&mem), &range).unwrap();
        async_io
            .read_to_memory(offset as libc::off_t, target, 0)
            .unwrap();
        while async_io.next_completed_request().is_some() {}
        let mut buf = vec![0u8; len];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        buf
    }

    #[cfg(test)]
    fn metadata(&self) -> &QcowMetadata {
        &self.metadata
    }
}

/// Writes a fresh qcow2 layout into `file`
#[cfg(test)]
pub(crate) fn create_image(
    file: &File,
    virtual_size: u64,
    backing_config: Option<&BackingFileConfig>,
) -> BlockResult<()> {
    create_image_with_cluster_bits(
        file,
        virtual_size,
        backing_config,
        header::DEFAULT_CLUSTER_BITS,
    )
}

/// Writes a fresh qcow2 layout with an explicit cluster size into `file`
#[cfg(any(test, feature = "test-utils"))]
pub(crate) fn create_image_with_cluster_bits(
    file: &File,
    virtual_size: u64,
    backing_config: Option<&BackingFileConfig>,
    cluster_bits: u32,
) -> BlockResult<()> {
    let path = backing_config.map(|cfg| cfg.path.as_str());
    let mut header =
        QcowHeader::create_for_size_path_and_cluster_bits(3, virtual_size, path, cluster_bits)
            .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
    if let Some(cfg) = backing_config
        && let Some(backing_file) = &mut header.backing_file
    {
        backing_file.format = cfg.format;
    }
    let raw = AlignedFile::new(
        file.try_clone()
            .map_err(|e| BlockError::new(BlockErrorKind::Io, DiskFileError::Clone(e)))?,
        false,
    );
    header
        .write_to(&raw)
        .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
    let (inner, _backing, _sparse) = parse_qcow(raw, MAX_NESTING_DEPTH, true)?;
    // Flush dirty caches and clear the dirty bit
    QcowMetadata::new(inner).shutdown();
    Ok(())
}

/// Helper struct to create a new qcow2 image in a temporary file.
#[cfg(any(test, feature = "test-utils"))]
pub struct QcowTempDisk {
    tmp: TempFile,
    disk: QcowDisk,
}

#[cfg(any(test, feature = "test-utils"))]
impl QcowTempDisk {
    /// Creates a new qcow2 image in a temporary file with optional
    /// backing file. Flags are passed to QcowDisk::new.
    pub fn new(
        virtual_size: u64,
        backing_config: Option<&BackingFileConfig>,
        direct_io: bool,
        sparse: bool,
        use_io_uring: bool,
    ) -> BlockResult<Self> {
        Self::new_with_cluster_bits(
            virtual_size,
            backing_config,
            direct_io,
            sparse,
            use_io_uring,
            header::DEFAULT_CLUSTER_BITS,
        )
    }

    /// Creates a new qcow2 image with an explicit cluster size.
    pub fn new_with_cluster_bits(
        virtual_size: u64,
        backing_config: Option<&BackingFileConfig>,
        direct_io: bool,
        sparse: bool,
        use_io_uring: bool,
        cluster_bits: u32,
    ) -> BlockResult<Self> {
        let tmp = TempFile::new().map_err(io::Error::from)?;
        create_image_with_cluster_bits(tmp.as_file(), virtual_size, backing_config, cluster_bits)?;
        let file = tmp
            .as_file()
            .try_clone()
            .map_err(|e| BlockError::new(BlockErrorKind::Io, DiskFileError::Clone(e)))?;
        let disk = QcowDisk::new(
            file,
            direct_io,
            backing_config.is_some(),
            sparse,
            use_io_uring,
        )?;
        Ok(Self { tmp, disk })
    }

    pub fn path(&self) -> &Path {
        self.tmp.as_path()
    }

    pub fn as_file(&self) -> &File {
        self.tmp.as_file()
    }

    pub fn disk(&self) -> &QcowDisk {
        &self.disk
    }

    /// Drops the disk handle and returns the underlying TempFile.
    pub fn into_tempfile(self) -> TempFile {
        self.tmp
    }
}

impl disk_file::DiskSize for QcowDisk {
    fn logical_size(&self) -> BlockResult<u64> {
        Ok(self.metadata.virtual_size())
    }
}

impl disk_file::PhysicalSize for QcowDisk {
    fn physical_size(&self) -> BlockResult<u64> {
        Ok(self.data_raw_file.physical_size()?)
    }
}

impl disk_file::DiskFd for QcowDisk {
    fn fd(&self) -> BorrowedDiskFd<'_> {
        BorrowedDiskFd::new(self.data_raw_file.as_raw_fd())
    }
}

impl disk_file::Geometry for QcowDisk {}

impl disk_file::SparseCapable for QcowDisk {
    fn supports_sparse_operations(&self) -> bool {
        true
    }
}

impl disk_file::Resizable for QcowDisk {
    fn resize(&mut self, size: u64) -> BlockResult<()> {
        if self.backing_file.is_some() {
            return Err(BlockError::new(
                BlockErrorKind::UnsupportedFeature,
                DiskFileError::ResizeError(io::Error::other(
                    "resize not supported with backing files",
                )),
            )
            .with_op(ErrorOp::Resize));
        }
        self.metadata.resize(size).map_err(|e| {
            BlockError::new(BlockErrorKind::Io, DiskFileError::ResizeError(e))
                .with_op(ErrorOp::Resize)
        })
    }
}

impl disk_file::MetadataSync for QcowDisk {
    fn sync_metadata(&self) -> BlockResult<()> {
        self.metadata
            .flush()
            .map_err(|e| BlockError::new(BlockErrorKind::Io, DiskFileError::SyncMetadata(e)))
    }
}

impl disk_file::DiskFile for QcowDisk {}

impl disk_file::AsyncDiskFile for QcowDisk {
    fn try_clone(&self) -> BlockResult<Box<dyn disk_file::AsyncDiskFile>> {
        Ok(Box::new(QcowDisk {
            metadata: Arc::clone(&self.metadata),
            backing_file: self.backing_file.as_ref().map(Arc::clone),
            sparse: self.sparse,
            data_raw_file: self.data_raw_file.clone(),
            use_io_uring: self.use_io_uring,
        }))
    }

    fn create_async_io(&self, ring_depth: u32) -> BlockResult<Box<dyn AsyncIo>> {
        if self.use_io_uring {
            #[cfg(feature = "io_uring")]
            {
                return Ok(Box::new(
                    QcowAsync::new(
                        Arc::clone(&self.metadata),
                        self.data_raw_file.clone(),
                        self.backing_file.as_ref().map(Arc::clone),
                        self.sparse,
                        ring_depth,
                    )
                    .map_err(|e| {
                        BlockError::new(BlockErrorKind::Io, DiskFileError::NewAsyncIo(e))
                    })?,
                ));
            }

            #[cfg(not(feature = "io_uring"))]
            unreachable!("use_io_uring is set but io_uring feature is not enabled");
        }

        let _ = ring_depth;
        Ok(Box::new(QcowSync::new(
            Arc::clone(&self.metadata),
            self.data_raw_file.clone(),
            self.backing_file.as_ref().map(Arc::clone),
            self.sparse,
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;

    use super::*;
    use crate::async_io::AsyncIo;
    use crate::disk_file::{AsyncDiskFile, DiskSize, PhysicalSize};

    const TEST_SIZE: u64 = 0x5566_7788;

    fn make_qcow_file() -> File {
        QcowTempDisk::new(TEST_SIZE, None, false, true, false)
            .unwrap()
            .into_tempfile()
            .into_file()
    }

    fn dirty_bit_is_set(file: &File) -> bool {
        let mut buf = [0u8; 8];
        file.read_exact_at(&mut buf, header::V2_BARE_HEADER_SIZE as u64)
            .unwrap();
        u64::from_be_bytes(buf) & IncompatFeatures::DIRTY.bits() != 0
    }

    #[test]
    fn new_sync_returns_correct_size() {
        let file = make_qcow_file();
        let disk = QcowDisk::new(file, false, false, true, false).unwrap();
        assert_eq!(disk.logical_size().unwrap(), TEST_SIZE);
    }

    fn assert_async_io_from_dyn(disk: &dyn AsyncDiskFile, expect_batch: bool) {
        let io: Box<dyn AsyncIo> = disk.create_async_io(128).unwrap();
        assert_eq!(io.batch_requests_enabled(), expect_batch);
    }

    fn assert_async_io(disk: &QcowDisk, expect_batch: bool) {
        assert_async_io_from_dyn(disk, expect_batch);
    }

    #[test]
    fn sync_backend_disables_batch_requests() {
        let file = make_qcow_file();
        let disk = QcowDisk::new(file, false, false, true, false).unwrap();
        assert_async_io(&disk, false);
    }

    #[cfg(feature = "io_uring")]
    #[test]
    fn io_uring_backend_enables_batch_requests() {
        let file = make_qcow_file();
        let disk = QcowDisk::new(file, false, false, true, true).unwrap();
        assert_async_io(&disk, true);
    }

    #[test]
    fn try_clone_preserves_sync_dispatch() {
        let file = make_qcow_file();
        let disk = QcowDisk::new(file, false, false, true, false).unwrap();
        let cloned = disk.try_clone().unwrap();
        assert_async_io_from_dyn(cloned.as_ref(), false);
    }

    #[test]
    fn dropping_clone_does_not_clear_dirty_bit() {
        let file = make_qcow_file();
        let disk = QcowDisk::new(file, false, false, true, false).unwrap();
        let cloned = disk.try_clone().unwrap();

        drop(cloned);

        assert_ne!(
            disk.metadata().header().incompatible_features & IncompatFeatures::DIRTY.bits(),
            0
        );
    }

    #[test]
    fn async_io_clears_dirty_bit_when_last_metadata_owner_drops() {
        let file = make_qcow_file();
        let inspect = file.try_clone().unwrap();
        let disk = QcowDisk::new(file, false, false, true, false).unwrap();
        let async_io = disk.create_async_io(1).unwrap();

        drop(disk);
        assert!(dirty_bit_is_set(&inspect));

        drop(async_io);
        assert!(!dirty_bit_is_set(&inspect));
    }

    #[cfg(feature = "io_uring")]
    #[test]
    fn try_clone_preserves_io_uring_dispatch() {
        let file = make_qcow_file();
        let disk = QcowDisk::new(file, false, false, true, true).unwrap();
        let cloned = disk.try_clone().unwrap();
        assert_async_io_from_dyn(cloned.as_ref(), true);
    }

    #[test]
    fn physical_size_less_than_logical() {
        // make_qcow_file() writes no guest data, so the file on disk
        // only contains QCOW2 headers and metadata tables.
        let file = make_qcow_file();
        let disk = QcowDisk::new(file, false, false, true, false).unwrap();
        assert!(disk.physical_size().unwrap() < disk.logical_size().unwrap());
    }

    /// Freed clusters normally become reusable on a guest flush. A guest
    /// whose flushes never reach the image (ignore_flush=on) must still get
    /// the clusters it discarded back once the file outgrows its virtual
    /// size, instead of extending the file on every rewrite.
    #[test]
    fn discarded_clusters_are_reused_past_virtual_size_without_flush() {
        use super::metadata::{ALLOC_HEADROOM_CLUSTERS, DeallocAction};

        const VIRTUAL_SIZE: u64 = 1 << 20;
        const CYCLES: u64 = 64;
        let file = QcowTempDisk::new(VIRTUAL_SIZE, None, false, true, false)
            .unwrap()
            .into_tempfile()
            .into_file();
        let probe = file.try_clone().unwrap();
        let disk = QcowDisk::new(file, false, false, true, false).unwrap();
        let metadata = disk.metadata();
        let cluster_size = metadata.cluster_size();

        let mut first_cycle_len = 0;
        for cycle in 0..CYCLES {
            for address in (0..VIRTUAL_SIZE).step_by(cluster_size as usize) {
                metadata
                    .map_cluster_for_write(address, None)
                    .unwrap_or_else(|e| panic!("cycle {cycle}: write at {address:#x}: {e}"));
            }
            let actions = metadata
                .deallocate_bytes(0, VIRTUAL_SIZE as usize, true, false, None)
                .unwrap_or_else(|e| panic!("cycle {cycle}: discard: {e}"));
            // The engines publish a freed cluster only after its host
            // punch-hole succeeds.
            for action in actions {
                if let DeallocAction::PunchHole { host_offset, .. } = action {
                    metadata.complete_punch_hole(host_offset);
                }
            }
            if cycle == 0 {
                first_cycle_len = probe.metadata().unwrap().len();
            }
        }
        // Without reuse every cycle appends VIRTUAL_SIZE to the file.
        let bound = first_cycle_len.max(metadata.reclaim_threshold())
            + ALLOC_HEADROOM_CLUSTERS * cluster_size;
        assert!(probe.metadata().unwrap().len() <= bound);

        let pattern = vec![0x5a; VIRTUAL_SIZE as usize];
        disk.write_all_at(0, &pattern);
        metadata.flush().unwrap();
        drop(disk);
        let reopened = QcowDisk::new(probe, false, false, true, false).unwrap();
        assert_eq!(reopened.read_all_at(0, VIRTUAL_SIZE as usize), pattern);
    }

    /// Reclaim is gated on the file end, so a rewrite below the threshold
    /// must extend the file, and the first allocation past it must reuse
    /// freed clusters instead. Fewer than `ALLOC_HEADROOM_CLUSTERS` pending
    /// clusters never reclaim, and a discard past the threshold reclaims
    /// just like a write.
    #[test]
    fn freed_clusters_are_reclaimed_only_past_the_threshold() {
        use super::metadata::{ALLOC_HEADROOM_CLUSTERS, DeallocAction};

        // 1/32 of the virtual size is 16 clusters here, only 8 past the
        // headroom, so once the threshold is crossed the reclaimed batch
        // drains to a handful of clusters. At 1 MiB the headroom exceeds
        // the slack and the threshold never gates anything.
        const VIRTUAL_SIZE: u64 = 32 << 20;
        // Never written up front: fresh allocations to drain the free list.
        const RESERVE: u64 = 16;
        let file = QcowTempDisk::new(VIRTUAL_SIZE, None, false, true, false)
            .unwrap()
            .into_tempfile()
            .into_file();
        let probe = file.try_clone().unwrap();
        let disk = QcowDisk::new(file, false, false, true, false).unwrap();
        let metadata = disk.metadata();
        let cluster_size = metadata.cluster_size();
        let clusters = VIRTUAL_SIZE / cluster_size;
        let written = clusters - RESERVE;
        let headroom = ALLOC_HEADROOM_CLUSTERS as usize;
        let headroom_bytes = ALLOC_HEADROOM_CLUSTERS * cluster_size;
        let threshold = metadata.reclaim_threshold();
        let file_len = || probe.metadata().unwrap().len();
        let avail = || metadata.free_list_len() - metadata.pending_reclaim_len();
        let write = |cluster: u64| {
            metadata
                .map_cluster_for_write(cluster * cluster_size, None)
                .unwrap_or_else(|e| panic!("write of cluster {cluster}: {e}"));
        };
        let discard = |first: u64, count: u64| {
            let actions = metadata
                .deallocate_bytes(
                    first * cluster_size,
                    (count * cluster_size) as usize,
                    true,
                    false,
                    None,
                )
                .unwrap_or_else(|e| panic!("discard of {count} clusters at {first}: {e}"));
            for action in actions {
                if let DeallocAction::PunchHole { host_offset, .. } = action {
                    metadata.complete_punch_hole(host_offset);
                }
            }
        };

        for cluster in 0..written {
            write(cluster);
        }
        let start_len = file_len();
        assert!(
            start_len + headroom_bytes <= threshold,
            "image must start below the threshold"
        );

        // Rewriting discarded clusters below the threshold extends the file;
        // the first allocation that would cross it reclaims instead.
        let to_cross = (threshold - headroom_bytes - start_len).div_ceil(cluster_size);
        let batch = to_cross + 16;
        assert!(batch < written);
        let pending_before = metadata.pending_reclaim_len();
        discard(0, batch);
        assert_eq!(
            metadata.pending_reclaim_len() as u64,
            pending_before as u64 + batch
        );
        let mut crossed = false;
        for cluster in 0..batch {
            let before = file_len();
            write(cluster);
            if before + headroom_bytes <= threshold {
                assert_eq!(
                    file_len(),
                    before + cluster_size,
                    "cluster {cluster} below the threshold"
                );
            } else {
                crossed = true;
                assert_eq!(file_len(), before, "cluster {cluster} past the threshold");
            }
        }
        assert!(crossed, "rewrites must carry the file past the threshold");
        for cluster in written..clusters {
            write(cluster);
        }
        let avail_left = avail();
        assert!(
            avail_left < headroom,
            "free list after draining: {avail_left}"
        );

        // A batch smaller than the headroom is metadata churn, not freed
        // guest data: it stays pending and allocations come from the free
        // list or the file end, without a sync.
        let pending = metadata.pending_reclaim_len();
        assert!(
            pending + 2 < headroom,
            "pending after the crossing: {pending}"
        );
        discard(0, 2);
        let pending = metadata.pending_reclaim_len();
        let before = file_len();
        write(0);
        write(1);
        assert_eq!(
            metadata.pending_reclaim_len(),
            pending,
            "a small batch was reclaimed"
        );
        let from_file_end = 2u64.saturating_sub(avail_left as u64);
        assert_eq!(file_len(), before + from_file_end * cluster_size);

        // Past the threshold a discard reclaims as well, before its own
        // refcount change relocates a clean block.
        discard(100, 16);
        let pending = metadata.pending_reclaim_len();
        assert!(
            pending >= 16,
            "the discard batch must be pending: {pending}"
        );
        let before = file_len();
        discard(300, 1);
        let pending = metadata.pending_reclaim_len();
        assert!(
            pending < headroom,
            "the discard did not reclaim: {pending} pending"
        );
        assert_eq!(
            file_len(),
            before,
            "the relocated block must come from the free list"
        );

        let pattern = vec![0xa5; VIRTUAL_SIZE as usize];
        disk.write_all_at(0, &pattern);
        metadata.flush().unwrap();
        drop(disk);
        let reopened = QcowDisk::new(probe, false, false, true, false).unwrap();
        assert_eq!(reopened.read_all_at(0, VIRTUAL_SIZE as usize), pattern);
    }
}
