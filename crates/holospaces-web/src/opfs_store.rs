//! An **OPFS-backed `KappaStore`** — the paged κ-disk's off-heap backing.
//!
//! The emulator's κ-disk content-addresses every sector in a [`KappaStore`].
//! Backing that store with OPFS instead of the wasm heap is what lets the browser
//! peer boot a *real* image without holding it all in RAM: sectors live in an
//! OPFS pack file, paged in on demand — "the KappaStore IS the memory, RAM is a
//! cache" (the canonical-forms principle).
//!
//! One OPFS file (a single synchronous access handle, opened once in the worker
//! where the emulator runs) holds every blob, appended; a small in-RAM index maps
//! κ → (offset, len) — the offsets, not the data. `get`/`put` are synchronous (the
//! `KappaStore` contract) via the sync access handle. Dedup is intrinsic: an
//! already-held κ is never rewritten (idempotent put, the substrate's cost model).

use std::cell::RefCell;
use std::collections::BTreeMap;

use hologram_substrate_core::{
    address_bytes_axis, Bytes, KappaLabel, KappaLabel71, KappaStore, StoreError,
};
use web_sys::{FileSystemReadWriteOptions, FileSystemSyncAccessHandle};

/// A content-addressed store whose blobs live in an OPFS pack file (off the wasm
/// heap), addressed by an in-RAM offset index.
pub struct OpfsKappaStore {
    handle: FileSystemSyncAccessHandle,
    index: RefCell<BTreeMap<[u8; 71], (f64, usize)>>,
    /// Logical end offset = `pending_base + pending.len()` (the next blob's offset).
    append_at: RefCell<f64>,
    /// Write-coalescing staging buffer: blobs are appended here and flushed to OPFS in big
    /// chunks, turning the κ-disk ingest's ~one-OPFS-write-per-512 B-sector (hundreds of
    /// thousands of sync writes for a desktop-scale rootfs — the constructor wall) into a few
    /// dozen bulk writes. A blob whose logical offset is ≥ `pending_base` lives here (served by
    /// `get` directly) until flushed; flushed blobs sit at OPFS offset == their logical offset.
    pending: RefCell<Vec<u8>>,
    /// The OPFS/logical offset at which `pending` begins (everything below is already in OPFS).
    pending_base: RefCell<f64>,
}

/// Flush the staging buffer once it reaches this size — bounds peak staging RAM while amortizing
/// the per-write OPFS overhead over many sectors.
const FLUSH_THRESHOLD: usize = 8 * 1024 * 1024;

// The browser peer is single-threaded (wasm has no threads), and the sync access
// handle is never shared across threads — so the `Send + Sync` the `KappaStore`
// trait requires hold trivially here.
unsafe impl Send for OpfsKappaStore {}
unsafe impl Sync for OpfsKappaStore {}

impl OpfsKappaStore {
    /// Wrap a freshly-truncated OPFS sync access handle (opened in the worker).
    #[must_use]
    pub fn new(handle: FileSystemSyncAccessHandle) -> Self {
        OpfsKappaStore {
            handle,
            index: RefCell::new(BTreeMap::new()),
            append_at: RefCell::new(0.0),
            pending: RefCell::new(Vec::new()),
            pending_base: RefCell::new(0.0),
        }
    }

    fn axis_arr(axis: &str, bytes: &[u8]) -> Result<[u8; 71], StoreError> {
        let label = address_bytes_axis(axis, bytes).map_err(|_| StoreError::UnknownAxis)?;
        <[u8; 71]>::try_from(label.as_slice()).map_err(|_| StoreError::UnknownAxis)
    }

    /// Write the staging buffer to OPFS in one call at `pending_base`, then advance the base and
    /// clear the buffer. Flushed blobs end up at OPFS offset == their logical offset (so `get`'s
    /// flushed-path read is correct). Idempotent when `pending` is empty.
    fn flush(&self) -> Result<(), StoreError> {
        let mut pending = self.pending.borrow_mut();
        if pending.is_empty() {
            return Ok(());
        }
        let base = *self.pending_base.borrow();
        let opts = FileSystemReadWriteOptions::new();
        opts.set_at(base);
        let wrote = self
            .handle
            .write_with_u8_array_and_options(&pending, &opts)
            .map_err(|_| StoreError::BackendFailure("opfs flush write"))?;
        if wrote as usize != pending.len() {
            return Err(StoreError::BackendFailure("opfs flush short write"));
        }
        *self.pending_base.borrow_mut() = base + pending.len() as f64;
        pending.clear();
        Ok(())
    }
}

impl KappaStore for OpfsKappaStore {
    fn put(&self, axis: &str, bytes: &[u8]) -> Result<KappaLabel71, StoreError> {
        let arr = Self::axis_arr(axis, bytes)?;
        let kappa = KappaLabel::from_bytes(&arr).map_err(|_| StoreError::InvalidKappa)?;
        // Idempotent: an already-held κ is not rewritten (dedup, Law L3).
        if !self.index.borrow().contains_key(&arr) {
            // Stage into the coalescing buffer at the current logical offset; the actual OPFS
            // write happens in bulk on flush. The blob is `get`-able immediately (from `pending`).
            let at = *self.append_at.borrow();
            self.pending.borrow_mut().extend_from_slice(bytes);
            self.index.borrow_mut().insert(arr, (at, bytes.len()));
            *self.append_at.borrow_mut() = at + bytes.len() as f64;
            if self.pending.borrow().len() >= FLUSH_THRESHOLD {
                self.flush()?;
            }
        }
        Ok(kappa)
    }

    fn get(&self, kappa: &KappaLabel71) -> Result<Option<Bytes>, StoreError> {
        let (at, len) = match self.index.borrow().get(kappa.as_array()) {
            Some(&v) => v,
            None => return Ok(None),
        };
        // Serve a not-yet-flushed blob straight from the staging buffer (its logical offset is
        // ≥ pending_base) so reads during/after ingest see staged content without a flush.
        let base = *self.pending_base.borrow();
        if at >= base {
            let off = (at - base) as usize;
            let pending = self.pending.borrow();
            if off + len <= pending.len() {
                return Ok(Some(Bytes::from(pending[off..off + len].to_vec())));
            }
            return Err(StoreError::BackendFailure("opfs staged read OOB"));
        }
        let mut buf = vec![0u8; len];
        let opts = FileSystemReadWriteOptions::new();
        opts.set_at(at);
        let read = self
            .handle
            .read_with_u8_array_and_options(&mut buf, &opts)
            .map_err(|_| StoreError::BackendFailure("opfs read"))?;
        if read as usize != len {
            return Err(StoreError::BackendFailure("opfs short read"));
        }
        Ok(Some(Bytes::from(buf)))
    }

    fn contains(&self, kappa: &KappaLabel71) -> bool {
        self.index.borrow().contains_key(kappa.as_array())
    }

    fn pin(&self, _kappa: &KappaLabel71) -> Result<(), StoreError> {
        Ok(())
    }

    fn unpin(&self, _kappa: &KappaLabel71) -> Result<(), StoreError> {
        Ok(())
    }

    fn iterate(&self) -> Vec<KappaLabel71> {
        self.index
            .borrow()
            .keys()
            .filter_map(|a| KappaLabel::from_bytes(a).ok())
            .collect()
    }

    fn pinned_roots(&self) -> Vec<KappaLabel71> {
        Vec::new()
    }

    fn approximate_count(&self) -> usize {
        self.index.borrow().len()
    }

    fn approximate_bytes(&self) -> u64 {
        *self.append_at.borrow() as u64
    }
}
