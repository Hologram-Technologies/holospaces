//! **κ-disk** — a content-addressed block device (the execution surface's disk).
//!
//! Realizes the *κ-disk* of the System Emulator building block (arc42 chapter 5,
//! `docs/src/arc42/adoc/05_building_block_view.adoc`) and the execution-surface
//! concept (arc42 chapter 8): an operating-system image and a repository live in
//! the substrate as *κ-addressed content*, not as a located disk image (Law L1).
//!
//! [`KappaDisk`] implements hologram's [`BlockDevice`] HAL trait
//! ([`hologram_bare_hal`]) backed by a [`KappaStore`]: each sector is stored as
//! canonical content keyed by its κ-label, and the disk is an *index* of sector
//! κ-labels. Because sectors are content, identical sectors are stored once
//! (Laws L2/L3 — dedup is automatic), and the whole image is itself a κ
//! ([`KappaDisk::image_kappa`]) that is reproducible and migratable like any
//! other holospace part. This is the disk a [system emulator](crate::surface)
//! reads and writes when it boots an OS image (ADR-009).
//!
//! The κ-disk re-uses the substrate: it adds no storage medium of its own
//! (Law L4) — it is the `KappaStore` viewed through the block-device seam,
//! exactly as hologram's own `BareMetalKappaStore` is the inverse view (a store
//! *over* a block device). Conformance: `CC-7` (arc42 chapter 10), witnessed
//! that a real on-disk ext4 filesystem round-trips byte-for-byte.

pub use hologram_bare_hal::{BlockDevice, DeviceError};
use hologram_substrate_core::{Bytes, KappaStore};

use crate::realizations::{address, empty_kappa, Kappa};

use alloc::collections::BTreeMap;
// `async_trait` emits an unqualified `Box`; in `no_std` it must be in scope
// (under `std` it is in the prelude).
#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::{boxed::Box, vec, vec::Vec};

/// Working-set bound for the read-through cache (distinct sector *contents*).
/// RAM is a cache of the canonical store (Law L3), not a second medium (Law L4) —
/// it is transient and bounded; the κ-store remains the source of truth. Capped
/// so a large disk cannot make the cache grow without limit; on overflow the
/// **single** oldest entry is evicted (FIFO), so a hot working set stays resident
/// instead of being dropped wholesale, while total residency stays bounded.
const CACHE_CAPACITY: usize = 4096;

use spin::Mutex;

/// The κ-disk realization IRI — the canonical form whose κ is the disk image's
/// identity (an index of its sector κ-labels).
const IMAGE_IRI: &str = "https://uor.foundation/holospaces/realization/kappa-disk";

/// A `KappaStore`-backed block device: a real filesystem image as κ-addressed
/// content (`CC-7`).
///
/// The device is a fixed geometry (`sector_size` × `sector_count`). Its state is
/// an in-memory *index* of one κ-label per sector; the sentinel
/// [`empty_kappa`] marks a never-written (sparse, all-zero) sector. Every read
/// and write goes through the borrowed [`KappaStore`] — no second medium
/// (Law L4). The disk holds κ-labels, not sector bytes (Law L3).
pub struct KappaDisk<'a> {
    store: &'a dyn KappaStore,
    sector_size: u32,
    sector_count: u64,
    index: Mutex<Vec<Kappa>>,
    /// A read-through working-set cache: decoded sector contents keyed by their
    /// κ-label (so content-identical sectors — the dedup case — share one entry).
    /// RAM caching the canonical store (Law L3); a hit returns the same bytes a
    /// `store.get` would, so the device's observable behaviour is unchanged.
    /// Bounded with FIFO single-entry eviction ([`BoundedCache`]).
    cache: Mutex<BoundedCache>,
    uuid: [u8; 16],
}

/// A bounded, κ-keyed FIFO cache of decoded sector contents. Capacity is fixed at
/// [`CACHE_CAPACITY`]; on overflow the single oldest entry is evicted (never a
/// wholesale clear), so a hot working set is preserved while residency is bounded.
struct BoundedCache {
    map: BTreeMap<Kappa, Bytes>,
    /// Insertion order of the keys currently in `map`, oldest at the front.
    order: alloc::collections::VecDeque<Kappa>,
}

impl BoundedCache {
    fn new() -> Self {
        BoundedCache {
            map: BTreeMap::new(),
            order: alloc::collections::VecDeque::new(),
        }
    }

    /// The cached bytes for `kappa`, if present.
    fn get(&self, kappa: &Kappa) -> Option<&Bytes> {
        self.map.get(kappa)
    }

    /// Insert `bytes` for `kappa`, evicting the single oldest entry at capacity.
    fn insert(&mut self, kappa: Kappa, bytes: Bytes) {
        use alloc::collections::btree_map::Entry;
        match self.map.entry(kappa) {
            Entry::Occupied(mut e) => {
                // Refresh in place; the FIFO order is unchanged for an existing key.
                e.insert(bytes);
            }
            Entry::Vacant(e) => {
                e.insert(bytes);
                self.order.push_back(kappa);
            }
        }
        // Evict the single oldest entry while over capacity (FIFO, never a clear).
        while self.order.len() > CACHE_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

impl<'a> KappaDisk<'a> {
    /// Open a blank κ-disk of `sector_count` sectors of `sector_size` bytes over
    /// `store`. All sectors begin sparse (read back as zeros until written).
    ///
    /// The device UUID is a reproducible function of the geometry (Law L1): the
    /// same geometry yields the same UUID on any peer.
    #[must_use]
    pub fn open(store: &'a dyn KappaStore, sector_size: u32, sector_count: u64) -> Self {
        let blank = empty_kappa();
        let index = vec![blank; sector_count as usize];
        let uuid = geometry_uuid(sector_size, sector_count);
        Self {
            store,
            sector_size,
            sector_count,
            index: Mutex::new(index),
            cache: Mutex::new(BoundedCache::new()),
            uuid,
        }
    }

    /// Open a κ-disk and write `image` into it from sector 0 (a real filesystem
    /// image becomes κ-addressed content). `image.len()` must be a multiple of
    /// `sector_size` and fit the geometry.
    ///
    /// # Errors
    ///
    /// [`DeviceError::OutOfRange`] if the image is misaligned or larger than the
    /// device.
    pub async fn from_image(
        store: &'a dyn KappaStore,
        sector_size: u32,
        image: &[u8],
    ) -> Result<KappaDisk<'a>, DeviceError> {
        if sector_size == 0 || !image.len().is_multiple_of(sector_size as usize) {
            return Err(DeviceError::OutOfRange);
        }
        let sectors = (image.len() / sector_size as usize) as u64;
        let disk = KappaDisk::open(store, sector_size, sectors);
        disk.write(0, sectors as u32, image).await?;
        Ok(disk)
    }

    /// Open a κ-disk of `total_sectors` × `sector_size` and populate it from a
    /// **stream of non-zero blocks** — the streaming, sparse counterpart of
    /// [`from_image`](KappaDisk::from_image) that never materializes a dense image
    /// (`CC-50`). `populate(&mut emit)` drives the producer (e.g. the ext4
    /// assembler), calling `emit(block_index, &block_bytes)` for each non-zero
    /// `block_bytes`-sized block; `block_bytes` must be a whole number of sectors.
    /// Blocks never emitted stay sparse (read back as zeros), so the disk's peak
    /// state holds only the *content*, not the full geometry (Law L3/L4).
    ///
    /// The same content yields the same [`image_kappa`](KappaDisk::image_kappa) as
    /// the dense [`from_image`](KappaDisk::from_image) path: identity is the sector
    /// κ-set, and the streamed disk's sectors are byte-identical to the dense image's.
    ///
    /// # Errors
    ///
    /// [`DeviceError::OutOfRange`] if `block_size` is not a positive multiple of
    /// `sector_size`, or an emitted block lands past the device.
    pub fn from_block_stream<F>(
        store: &'a dyn KappaStore,
        sector_size: u32,
        total_sectors: u64,
        block_size: u32,
        mut populate: F,
    ) -> Result<KappaDisk<'a>, DeviceError>
    where
        F: FnMut(&mut dyn FnMut(u64, &[u8]) -> Result<(), DeviceError>) -> Result<(), DeviceError>,
    {
        if sector_size == 0 || block_size == 0 || !block_size.is_multiple_of(sector_size) {
            return Err(DeviceError::OutOfRange);
        }
        let sectors_per_block = (block_size / sector_size) as u64;
        let disk = KappaDisk::open(store, sector_size, total_sectors);
        // Write each emitted block at its sector offset. The write path is the
        // synchronous core of [`BlockDevice::write`]: it content-addresses every
        // sector through the store (dedup, Laws L2/L3) and updates the sparse index
        // — never a dense buffer. (Streaming is sync: the producer's callback is
        // sync, and the store is sync, so no executor is involved.)
        {
            let mut sink = |block_index: u64, bytes: &[u8]| -> Result<(), DeviceError> {
                if bytes.len() != block_size as usize {
                    return Err(DeviceError::OutOfRange);
                }
                let lba = block_index * sectors_per_block;
                disk.write_sectors_sync(lba, sectors_per_block as u32, bytes)
            };
            populate(&mut sink)?;
        }
        Ok(disk)
    }

    /// The synchronous core of [`BlockDevice::write`]: content-address each sector
    /// of `buffer` (sized `sectors` × `sector_size`) through the store and update
    /// the sparse index at `lba`. The async trait method delegates to this; the
    /// streaming assembler calls it directly (no executor needed, the store is sync).
    fn write_sectors_sync(&self, lba: u64, sectors: u32, buffer: &[u8]) -> Result<(), DeviceError> {
        self.sector_range(lba, sectors, buffer.len())?;
        let ss = self.sector_size as usize;
        let blank = empty_kappa();
        let mut index = self.index.lock();
        let mut cache = self.cache.lock();
        for i in 0..sectors as usize {
            let slot = &buffer[i * ss..(i + 1) * ss];
            // An all-zero sector is *sparse*: it stores nothing and the index holds
            // the sentinel (read back as zeros). This makes a written-zeros sector
            // and a never-written sector canonically identical (Laws L1/L3/L4) — so
            // the dense [`from_image`] path and the streaming assembly produce the
            // *same* sector κ-set and the same [`image_kappa`] for the same content,
            // and the disk's free space costs nothing.
            if slot.iter().all(|&b| b == 0) {
                index[lba as usize + i] = blank;
                continue;
            }
            // Content-address the sector through the store (idempotent: identical
            // sectors store once — dedup, Laws L2/L3).
            let kappa = self
                .store
                .put("blake3", slot)
                .map_err(|_| DeviceError::HardwareFault(4))?;
            index[lba as usize + i] = kappa;
            // Write-through: a just-written sector is immediately readable from the
            // cache without re-hitting the store.
            cache.insert(kappa, Bytes::from(slot.to_vec()));
        }
        Ok(())
    }

    /// The disk image's identity: the κ-label of its canonical form — the IRI
    /// tag followed by every sector's κ-label in order (Law L1). Reproducible:
    /// the same sector contents yield the same image κ on any peer, so a κ-disk
    /// can be snapshotted and migrated like any other holospace part.
    #[must_use]
    pub fn image_kappa(&self) -> Kappa {
        let index = self.index.lock();
        let mut canonical = Vec::with_capacity(IMAGE_IRI.len() + 1 + index.len() * 71);
        canonical.extend_from_slice(IMAGE_IRI.as_bytes());
        canonical.push(0);
        for k in index.iter() {
            canonical.extend_from_slice(k.as_array());
        }
        address(&canonical)
    }

    /// The number of *distinct* stored sector contents — fewer than the sector
    /// count whenever the image repeats a sector (content-addressed dedup, Laws
    /// L2/L3). The sparse sentinel is not counted (it stores nothing).
    #[must_use]
    pub fn distinct_sectors(&self) -> usize {
        let index = self.index.lock();
        let blank = empty_kappa();
        let mut seen = Vec::new();
        for k in index.iter() {
            if *k != blank && !seen.contains(k) {
                seen.push(*k);
            }
        }
        seen.len()
    }

    fn sector_range(&self, lba: u64, sectors: u32, buf_len: usize) -> Result<(), DeviceError> {
        let ss = self.sector_size as usize;
        if buf_len != sectors as usize * ss {
            return Err(DeviceError::OutOfRange);
        }
        let end = lba
            .checked_add(sectors as u64)
            .ok_or(DeviceError::OutOfRange)?;
        if end > self.sector_count {
            return Err(DeviceError::OutOfRange);
        }
        Ok(())
    }
}

/// Derive a stable, geometry-scoped device UUID (Law L1 — content, not a random
/// hardware id). The first 16 bytes of the κ of the geometry descriptor.
fn geometry_uuid(sector_size: u32, sector_count: u64) -> [u8; 16] {
    let mut descriptor = [0u8; 12];
    descriptor[..4].copy_from_slice(&sector_size.to_le_bytes());
    descriptor[4..].copy_from_slice(&sector_count.to_le_bytes());
    let k = address(&descriptor);
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&k.as_array()[..16]);
    uuid
}

#[async_trait::async_trait]
impl BlockDevice for KappaDisk<'_> {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    async fn read(&self, lba: u64, sectors: u32, buffer: &mut [u8]) -> Result<(), DeviceError> {
        self.sector_range(lba, sectors, buffer.len())?;
        let ss = self.sector_size as usize;
        let blank = empty_kappa();
        let index = self.index.lock();
        let mut cache = self.cache.lock();
        for i in 0..sectors as u64 {
            let kappa = index[(lba + i) as usize];
            let slot = &mut buffer[i as usize * ss..(i as usize + 1) * ss];
            if kappa == blank {
                // A never-written sector reads back as zeros (sparse).
                slot.fill(0);
                continue;
            }
            // Read-through cache (Law L3): serve the sector's content from RAM if
            // we have already decoded this κ; otherwise fetch it from the
            // canonical store and remember it. Identical sectors share a κ, so the
            // cache is keyed by content — a repeated read never re-hits the store.
            if let Some(bytes) = cache.get(&kappa) {
                slot.copy_from_slice(bytes.as_ref());
                continue;
            }
            let bytes = self
                .store
                .get(&kappa)
                .map_err(|_| DeviceError::HardwareFault(1))?
                .ok_or(DeviceError::HardwareFault(2))?;
            if bytes.len() != ss {
                return Err(DeviceError::HardwareFault(3));
            }
            slot.copy_from_slice(bytes.as_ref());
            cache.insert(kappa, bytes);
        }
        Ok(())
    }

    async fn write(&self, lba: u64, sectors: u32, buffer: &[u8]) -> Result<(), DeviceError> {
        // The whole write is bounded local work (content-address each sector
        // through the sync store, update the sparse index, write-through the
        // cache); the async core is shared with the streaming assembler.
        self.write_sectors_sync(lba, sectors, buffer)
    }

    async fn flush(&self) -> Result<(), DeviceError> {
        // Every write content-addresses through the store synchronously, so the
        // disk is already durable wherever the store is durable. Nothing to do.
        Ok(())
    }

    fn device_uuid(&self) -> [u8; 16] {
        self.uuid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hologram_store_mem::MemKappaStore;

    /// The read cache is bounded and evicts a *single* oldest entry on overflow
    /// (FIFO) rather than clearing wholesale. After inserting one past capacity,
    /// the cache holds exactly `CACHE_CAPACITY` entries (not 1, which a clear would
    /// give), the second-oldest survives, and only the very oldest is gone.
    #[test]
    fn read_cache_evicts_one_at_a_time_not_wholesale() {
        let mut cache = BoundedCache::new();
        let k = |i: u64| -> Kappa {
            let mut b = [0u8; 512];
            b[..8].copy_from_slice(&i.to_le_bytes());
            // A distinct content κ per i.
            crate::realizations::address(&b)
        };
        for i in 0..CACHE_CAPACITY as u64 {
            cache.insert(k(i), Bytes::from(vec![i as u8; 4]));
        }
        assert_eq!(cache.map.len(), CACHE_CAPACITY, "full to capacity");
        // One more triggers a single eviction of the oldest (k(0)).
        cache.insert(k(CACHE_CAPACITY as u64), Bytes::from(vec![0xFF; 4]));
        assert_eq!(
            cache.map.len(),
            CACHE_CAPACITY,
            "still exactly capacity — a single eviction, not a wholesale clear"
        );
        assert!(cache.get(&k(0)).is_none(), "the oldest entry was evicted");
        assert!(cache.get(&k(1)).is_some(), "the second-oldest survives");
        assert!(
            cache.get(&k(CACHE_CAPACITY as u64)).is_some(),
            "the newest entry is present"
        );
    }

    #[test]
    fn sectors_round_trip_and_sparse_reads_are_zero() {
        pollster::block_on(async {
            let store = MemKappaStore::new();
            let disk = KappaDisk::open(&store, 512, 8);
            // Write a real pattern into sector 2, leave the rest sparse.
            let mut sector = [0u8; 512];
            sector[..5].copy_from_slice(b"hello");
            disk.write(2, 1, &sector).await.unwrap();

            let mut back = [0xAAu8; 512];
            disk.read(2, 1, &mut back).await.unwrap();
            assert_eq!(back, sector, "written sector reads back identically");

            // A never-written sector reads back as zeros (sparse).
            let mut zero = [0xAAu8; 512];
            disk.read(0, 1, &mut zero).await.unwrap();
            assert_eq!(zero, [0u8; 512], "unwritten sector is sparse-zero");
        });
    }

    #[test]
    fn identical_sectors_dedup_and_image_kappa_is_reproducible() {
        pollster::block_on(async {
            let store = MemKappaStore::new();
            // Four sectors, two of them identical content.
            let mut image = vec![0u8; 512 * 4];
            image[0..512].fill(1);
            image[512..1024].fill(2);
            image[1024..1536].fill(1); // identical to sector 0
            image[1536..2048].fill(3);
            let disk = KappaDisk::from_image(&store, 512, &image).await.unwrap();

            // Dedup: 4 sectors, 3 distinct contents (sector 2 == sector 0).
            assert_eq!(disk.distinct_sectors(), 3, "identical sectors store once");

            // The whole image reads back byte-identically.
            let mut back = vec![0u8; 512 * 4];
            disk.read(0, 4, &mut back).await.unwrap();
            assert_eq!(back, image);

            // The image κ is reproducible from the same content (Law L1).
            let store2 = MemKappaStore::new();
            let again = KappaDisk::from_image(&store2, 512, &image).await.unwrap();
            assert_eq!(disk.image_kappa(), again.image_kappa());
        });
    }

    #[test]
    fn out_of_range_and_misaligned_io_are_refused() {
        pollster::block_on(async {
            let store = MemKappaStore::new();
            let disk = KappaDisk::open(&store, 512, 4);
            // Past the end of the device.
            let buf = [0u8; 512];
            assert_eq!(disk.write(4, 1, &buf).await, Err(DeviceError::OutOfRange));
            // Misaligned buffer length.
            let mut bad = [0u8; 500];
            assert_eq!(
                disk.read(0, 1, &mut bad).await,
                Err(DeviceError::OutOfRange)
            );
            // A misaligned image is refused at open.
            assert_eq!(
                KappaDisk::from_image(&store, 512, &[0u8; 700]).await.err(),
                Some(DeviceError::OutOfRange)
            );
        });
    }
}
