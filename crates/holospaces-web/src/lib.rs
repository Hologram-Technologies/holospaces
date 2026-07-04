//! **The Hologram Platform Manager** — holospaces' browser peer.
//!
//! Realizes the *Platform Manager* (arc42 chapter 5,
//! `docs/src/arc42/adoc/05_building_block_view.adoc`) and the *Browser* peer
//! (arc42 chapter 7): the operator console, delivered as wasm and **served from
//! GitHub Pages** (arc42 chapter 3 / chapter 6 cold-start). Loading the
//! κ-addressed bundle makes the browser a peer that *is* the substrate — there
//! is no server (Law L1).
//!
//! This crate is the wasm-bindgen surface over holospaces' Manager model. It
//! composes a full browser peer: an in-memory `KappaStore` (RAM as a cache, Law
//! L3) and hologram's **interpreter `ContainerEngine`** (`hologram-runtime-bare`,
//! a `no_std` `wasmi` interpreter that runs in wasm32 where a JIT cannot). It
//! exposes the console operations — sign in, provision (both compute forms),
//! view, resolve (verify by re-derivation, Law L5), the operator roster (R5),
//! the browser `.holo` engine (RT2, `CC-2`), booting a userland container
//! in-browser through the substrate runtime (the execution surface, ADR-008;
//! `CC-6`), and importing and running a devcontainer in the browser — the
//! Codespaces/Gitpod scenario with no Docker daemon and no cloud VM (arc42
//! chapter 1, the motivating scenario). The same holospace κ boots on this
//! browser peer as on a native or remote one (Q6).
//!
//! It also exposes the **[`Workspace`]** — launching a holospace whose code is
//! the [system emulator](holospaces::emulator) **boots a real operating system
//! in the tab** (`CC-9`) and drives it through the [workspace
//! projection](holospaces::projection) (`CC-11`): a live terminal whose commands
//! are canonical events advancing the holospace's κ snapshot, and an editor that
//! reads and edits environment content by κ — the documented launch experience,
//! realized on the browser peer.

mod opfs_store;
mod webrtc;
mod wsnet;

pub use webrtc::WebRtcLink;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use hologram_runtime::Runtime;
use hologram_runtime_bare::BareMetalEngine;
use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{Bytes, Capabilities, KappaStore, Realization};
use holospaces::assembly::{assemble_ext4, assemble_ext4_bootable, Layer};
use holospaces::boot::{devcontainer, provision, LifecycleError, ReadVerify, Resolver, Session};
use holospaces::config::{Configuration, Directive, LifecycleAction};
use holospaces::content_net::ContentPeer;
use holospaces::emulator::{aarch64, net, x64, Emulator, Halt};
use holospaces::identity::{Operator, Roster};
use holospaces::machine::MachineSpec;
use holospaces::oci::ImagePull;
use holospaces::projection::{Intent, Workspace as Projection};
use holospaces::realizations::{address, verify, Holospace, Kappa, Source};
use holospaces::surface;
use wasm_bindgen::prelude::*;

fn js_err<E: core::fmt::Debug>(e: E) -> JsValue {
    JsValue::from_str(&format!("{e:?}"))
}

fn parse_kappa(kappa: &str) -> Result<Kappa, JsValue> {
    Kappa::from_bytes(kappa.as_bytes()).map_err(|_| JsValue::from_str("not a well-formed κ-label"))
}

/// A capability set with a memory budget; the other authorities default closed
/// (the browser peer is a single-participant content-addressed mesh).
fn capabilities(memory_bytes: f64) -> Capabilities {
    Capabilities {
        storage_roots: Vec::new(),
        storage_quota_bytes: 0,
        network_fetch: false,
        network_announce: false,
        publish_channels: Vec::new(),
        subscribe_channels: Vec::new(),
        memory_max_bytes: memory_bytes as u64,
        cpu_time_per_event_ms: 0,
        priority_weight: 0,
    }
}

/// Drive a substrate-runtime future to completion synchronously. The browser
/// peer's store is local (no network), so the lifecycle futures resolve without
/// yielding — a single poll completes them; the bounded loop fails loud rather
/// than spinning if that invariant is ever violated.
fn block_on<F: core::future::Future>(future: F) -> F::Output {
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut future = core::pin::pin!(future);
    for _ in 0..1024 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
    panic!("a local substrate-runtime future did not complete without yielding");
}

/// The κ-label of bytes on the substrate's default σ-axis (blake3) — the same
/// content address every peer computes (Law L1).
#[wasm_bindgen]
pub fn kappa(bytes: &[u8]) -> String {
    address(bytes).as_str().to_owned()
}

/// Verify bytes against a claimed κ-label by re-derivation (Law L5). This is
/// what makes content fetched from an untrusted gateway safe.
#[wasm_bindgen]
pub fn verify_kappa(bytes: &[u8], kappa: &str) -> Result<bool, JsValue> {
    verify(bytes, &parse_kappa(kappa)?).map_err(js_err)
}

/// The **usable default** Dev Container base image the peer provisions when a
/// repository declares no `devcontainer.json` (`buildpack-deps` — `curl`/`git`
/// over apt; the Dev Container spec's default, `CC-20`). Exposed so the
/// operator's page names the same default the host importer does — one source
/// of truth across native and wasm ([`holospaces::DEFAULT_DEVCONTAINER_IMAGE`]).
#[wasm_bindgen]
pub fn default_devcontainer_image() -> String {
    holospaces::DEFAULT_DEVCONTAINER_IMAGE.to_owned()
}

/// **Provision a devcontainer's real OCI image in the browser** — the deployed
/// path that makes a launched holospace the repository's *actual* devcontainer,
/// not a demo. The page drives it with the router as the transport: while
/// [`is_done`](DevcontainerProvision::is_done) is false, read
/// [`next_url`](DevcontainerProvision::next_url) /
/// [`next_accept`](DevcontainerProvision::next_accept) /
/// [`next_bearer`](DevcontainerProvision::next_bearer), fetch through the router
/// extension's CORS-free `fetch`, and feed the response back with
/// [`deliver`](DevcontainerProvision::deliver); then `assemble` yields the
/// bootable rootfs. The pull is the *same* [`ImagePull`] the native importer uses
/// and re-derives every blob (Law L5) — only the transport differs.
#[wasm_bindgen]
pub struct DevcontainerProvision {
    pull: ImagePull,
    store: MemKappaStore,
}

#[wasm_bindgen]
impl DevcontainerProvision {
    /// Begin provisioning `image_ref` (e.g. `mcr.microsoft.com/devcontainers/base:debian`)
    /// for `arch` (`"riscv64"` / `"aarch64"`).
    #[wasm_bindgen(constructor)]
    pub fn new(image_ref: &str, arch: &str) -> Result<DevcontainerProvision, JsValue> {
        let arch = holospaces::Arch::from_id(arch).unwrap_or_default();
        Ok(DevcontainerProvision {
            pull: ImagePull::new(image_ref, arch).map_err(js_err)?,
            store: MemKappaStore::new(),
        })
    }

    /// The URL the page must `GET` next through the router, or `undefined` when
    /// [`is_done`](DevcontainerProvision::is_done).
    #[wasm_bindgen(js_name = nextUrl)]
    #[must_use]
    pub fn next_url(&self) -> Option<String> {
        self.pull.next_fetch().map(|f| f.url)
    }

    /// The `Accept` header for the next fetch (manifests), or `undefined`.
    #[wasm_bindgen(js_name = nextAccept)]
    #[must_use]
    pub fn next_accept(&self) -> Option<String> {
        self.pull.next_fetch().and_then(|f| f.accept)
    }

    /// The bearer token for the next fetch once one is held, or `undefined`.
    #[wasm_bindgen(js_name = nextBearer)]
    #[must_use]
    pub fn next_bearer(&self) -> Option<String> {
        self.pull.next_fetch().and_then(|f| f.bearer)
    }

    /// Feed the router's response to the current fetch.
    pub fn deliver(&mut self, status: u16, content_type: &str, body: &[u8]) -> Result<(), JsValue> {
        self.pull
            .deliver(status, content_type, body.to_vec())
            .map_err(js_err)
    }

    /// Whether every blob has been delivered and the image is ready to
    /// [`assemble`](DevcontainerProvision::assemble).
    #[wasm_bindgen(js_name = isDone)]
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.pull.is_done()
    }

    /// Ingest the fully-fetched image (re-deriving every blob — Law L5) and
    /// assemble it into a **bootable** ext4 rootfs the emulator boots over
    /// `virtio-blk`. A real OCI image carries no `/init`, so the devcontainer
    /// init for a real image ([`REAL_IMAGE_INIT`](holospaces::machine::REAL_IMAGE_INIT)
    /// — `#!/bin/sh`, the image's own coreutils) is injected, and the filesystem
    /// is sized to `disk_bytes` so the guest has room to work (`apt`, builds, the
    /// files you create). On the paged κ-disk the free space is sparse (zero
    /// sectors are not stored), so a generous size is cheap. Pass the result to
    /// [`boot_devcontainer_routed_opfs`](Workspace::boot_devcontainer_routed_opfs).
    pub fn assemble(&self, disk_bytes: f64) -> Result<Vec<u8>, JsValue> {
        let image = self.pull.ingest(&self.store).map_err(js_err)?;
        let mut owned: Vec<(String, Vec<u8>)> = Vec::new();
        for (k, mt) in image.layers().iter().zip(image.layer_media_types()) {
            let bytes = self
                .store
                .get(k)
                .map_err(js_err)?
                .ok_or_else(|| JsValue::from_str("an ingested layer is missing from the store"))?
                .as_ref()
                .to_vec();
            owned.push((mt.clone(), bytes));
        }
        let layers: Vec<Layer> = owned
            .iter()
            .map(|(mt, b)| Layer {
                media_type: mt,
                blob: b,
            })
            .collect();
        assemble_ext4_bootable(
            &layers,
            holospaces::machine::REAL_IMAGE_INIT,
            disk_bytes as u64,
        )
        .map_err(js_err)
    }

    /// Assemble the bootable rootfs **straight into an OPFS file**, sparse and
    /// streaming — the `CC-50` provisioning path that never materializes a dense
    /// in-RAM image. Equivalent in content to [`assemble`](Self::assemble), but
    /// instead of returning a `Vec` sized to the whole (possibly multi-GiB) disk,
    /// it writes only the **non-zero 4 KiB blocks** to `rootfs_handle` at their
    /// byte offsets; the OPFS file's free space stays sparse (zero on read). Peak
    /// wasm heap tracks the image's *content*, not its declared size ("the
    /// KappaStore IS the memory, RAM is a cache", Laws L3/L4).
    ///
    /// Returns the total image length in bytes (a whole number of sectors). The
    /// page then boots from the file with
    /// [`boot_devcontainer_routed_opfs_streamed`](Workspace::boot_devcontainer_routed_opfs_streamed),
    /// which pages the disk sector-by-sector — so neither provisioning nor boot
    /// ever holds the whole image in RAM.
    #[wasm_bindgen(js_name = assembleIntoOpfs)]
    pub fn assemble_into_opfs(
        &self,
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        disk_bytes: f64,
    ) -> Result<f64, JsValue> {
        self.assemble_into_opfs_inner(rootfs_handle, None, disk_bytes)
    }

    /// Like [`assembleIntoOpfs`](Self::assemble_into_opfs), but also records the
    /// rootfs's **occupancy** — the ascending indices of the blocks it actually
    /// wrote — into `occupancy_handle` as packed little-endian `u64`s. That sidecar
    /// is what lets the deployed boot page an **arbitrarily large**, build-capable
    /// devcontainer disk **O(content)**: only the (few) occupied blocks are read at
    /// boot, never the declared size. The rootfs bytes are identical to the untracked
    /// assembler over the same image (Law L1); the sidecar is the sole addition.
    #[wasm_bindgen(js_name = assembleIntoOpfsTracked)]
    pub fn assemble_into_opfs_tracked(
        &self,
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        occupancy_handle: web_sys::FileSystemSyncAccessHandle,
        disk_bytes: f64,
    ) -> Result<f64, JsValue> {
        self.assemble_into_opfs_inner(rootfs_handle, Some(occupancy_handle), disk_bytes)
    }

    fn assemble_into_opfs_inner(
        &self,
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        occupancy_handle: Option<web_sys::FileSystemSyncAccessHandle>,
        disk_bytes: f64,
    ) -> Result<f64, JsValue> {
        let image = self.pull.ingest(&self.store).map_err(js_err)?;
        // Pull each layer blob out of the store one at a time, overlay it, and let
        // it drop before fetching the next — neither the whole layer stack nor the
        // dense ext4 image is ever resident (Laws L3/L4).
        let layer_kappas: Vec<Kappa> = image.layers().to_vec();
        let media_types: Vec<String> = image.layer_media_types().to_vec();
        let mut layer_idx = 0usize;
        let mut layer_err: Option<JsValue> = None;
        let next_layer =
            || -> Result<Option<(String, Vec<u8>)>, holospaces::assembly::AssemblyError> {
                if layer_idx >= layer_kappas.len() {
                    return Ok(None);
                }
                let i = layer_idx;
                layer_idx += 1;
                match self.store.get(&layer_kappas[i]) {
                    Ok(Some(bytes)) => Ok(Some((media_types[i].clone(), bytes.as_ref().to_vec()))),
                    Ok(None) => {
                        layer_err = Some(JsValue::from_str(
                            "an ingested layer is missing from the store",
                        ));
                        Ok(None)
                    }
                    Err(e) => {
                        layer_err = Some(js_err(e));
                        Ok(None)
                    }
                }
            };

        // Stream the ext4 image block-by-block into the OPFS file. Only non-zero
        // blocks are written (the free space stays sparse), so the wasm heap holds
        // the assembler's content working set, never the whole image.
        let mut io_err: Option<JsValue> = None;
        let track = occupancy_handle.is_some();
        // Tracked: a COMPACT rootfs — the non-zero blocks written contiguously, with a
        // sidecar (8-byte image_len header + each packed block's disk index) so a
        // multi-GiB declared disk costs only its content in OPFS (a sparse file's
        // logical size would count against the origin quota). Untracked: the classic
        // sparse layout (blocks at their disk offsets). See `BLOCK_BYTES`.
        let mut occupancy: Vec<u8> = if track {
            0u64.to_le_bytes().to_vec()
        } else {
            Vec::new()
        };
        let mut packed_blocks: u64 = 0;
        let geom = holospaces::assembly::stream_ext4_image_bootable_streamed_layers(
            next_layer,
            holospaces::machine::REAL_IMAGE_INIT,
            disk_bytes as u64,
            |block_index, bytes| {
                if io_err.is_some() {
                    return;
                }
                let opts = web_sys::FileSystemReadWriteOptions::new();
                let off = if track {
                    occupancy.extend_from_slice(&block_index.to_le_bytes());
                    packed_blocks * bytes.len() as u64 // contiguous
                } else {
                    block_index * bytes.len() as u64 // sparse, at the disk offset
                };
                opts.set_at(off as f64);
                if let Err(e) = rootfs_handle.write_with_u8_array_and_options(bytes, &opts) {
                    io_err = Some(e);
                }
                packed_blocks += 1;
            },
        )
        .map_err(js_err)?;
        if let Some(e) = layer_err {
            return Err(e);
        }
        if let Some(e) = io_err {
            return Err(e);
        }
        let image_len = geom.image_len();
        if let Some(occ) = occupancy_handle {
            // Compact rootfs (exactly the packed blocks) + the sidecar with image_len.
            rootfs_handle.truncate_with_f64((packed_blocks * BLOCK_BYTES) as f64)?;
            occupancy[0..8].copy_from_slice(&(image_len as u64).to_le_bytes());
            occ.truncate_with_f64(0.0)?;
            let opts = web_sys::FileSystemReadWriteOptions::new();
            opts.set_at(0.0);
            occ.write_with_u8_array_and_options(&occupancy, &opts)?;
        } else {
            // Sparse rootfs: grow to span the full image (the tail reads back zero).
            rootfs_handle.truncate_with_f64(image_len as f64)?;
        }
        Ok(image_len as f64)
    }
}

/// Run a `.holo` compute artifact in the browser via the hologram executor
/// compiled to wasm — the *browser `.holo` engine* (arc42 chapter 11, RT2;
/// conformance `CC-2`). Returns the κ-label of the first output. Because the
/// executor is deterministic and content-addressed, this κ equals the one the
/// native executor produces for the same `.holo` (the browser engine equals the
/// native one).
#[wasm_bindgen]
pub fn run_holo(archive: &[u8]) -> Result<String, JsValue> {
    let outputs = holospaces::engine::HoloEngine::run(archive, &[]).map_err(js_err)?;
    let first = outputs
        .first()
        .ok_or_else(|| JsValue::from_str("the .holo produced no outputs"))?;
    Ok(first.as_str().to_owned())
}

/// Validate that `module` is a recompiled userland fit for the *execution
/// surface* (ADR-008; `CC-6`): specification-valid WebAssembly that imports only
/// the substrate host ABI and presents the container ABI. This is the κ-boundary
/// contract the browser peer enforces before a userland may be a holospace's
/// code — ambient (WASI-style) imports and a missing container ABI are refused.
#[wasm_bindgen]
pub fn validate_userland(module: &[u8]) -> Result<(), JsValue> {
    surface::validate_userland(module).map_err(js_err)
}

/// A devcontainer's OCI image, assembled into a bootable root filesystem *in the
/// browser* — the Layer Assembler (`CC-7` / the in-crate ext4 writer) running as
/// the wasm peer. The operator's page fetches the devcontainer's image layers
/// from the cold-start gateway (verified by re-derivation before they are added),
/// then assembles them here; the result boots over the emulator's `virtio-blk`
/// ([`Workspace::boot_devcontainer`], `CC-14`). The browser peer *is* the
/// machine — no server assembles or boots the OS (Law L1/L4).
#[wasm_bindgen]
pub struct DevcontainerImage {
    layers: Vec<(String, Vec<u8>)>,
}

#[wasm_bindgen]
impl DevcontainerImage {
    /// A new, empty image (add its layers lowest-first with [`Self::add_layer`]).
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> DevcontainerImage {
        DevcontainerImage { layers: Vec::new() }
    }

    /// Add an OCI image layer (its media type + the verified blob bytes), in
    /// order from the base layer up.
    pub fn add_layer(&mut self, media_type: &str, blob: &[u8]) {
        self.layers.push((media_type.to_string(), blob.to_vec()));
    }

    /// Assemble the layers into a bootable `ext4` root filesystem (gunzip +
    /// untar + OCI whiteout overlay + the in-crate ext4 writer; Law L4). The
    /// bytes back a [`Workspace::boot_devcontainer`] machine's `virtio-blk` disk.
    pub fn assemble(&self) -> Result<Vec<u8>, JsValue> {
        let layers: Vec<Layer> = self
            .layers
            .iter()
            .map(|(mt, b)| Layer {
                media_type: mt,
                blob: b,
            })
            .collect();
        assemble_ext4(&layers).map_err(js_err)
    }

    /// Assemble the layers into a **bootable, interactive, writable** root
    /// filesystem on a `disk_bytes`-sized disk: the same overlay as
    /// [`Self::assemble`], plus the persistent devcontainer
    /// [`/init`](holospaces::machine::DEVCONTAINER_INIT) injected — it mounts the
    /// pseudo filesystems and the shared `virtio-9p` workspace and execs a shell,
    /// so the booted OS stays running as a dev environment instead of powering off
    /// after boot — and sized to `disk_bytes` so the OS has room to work (the
    /// devcontainer's disk; the caller's to choose, not a hidden cap). The base
    /// image must provide a static `/bin/busybox`.
    pub fn assemble_bootable(&self, disk_bytes: f64) -> Result<Vec<u8>, JsValue> {
        let layers: Vec<Layer> = self
            .layers
            .iter()
            .map(|(mt, b)| Layer {
                media_type: mt,
                blob: b,
            })
            .collect();
        assemble_ext4_bootable(
            &layers,
            holospaces::machine::DEVCONTAINER_INIT,
            disk_bytes as u64,
        )
        .map_err(js_err)
    }

    /// Assemble the **bootable** rootfs of [`Self::assemble_bootable`] **straight
    /// into an OPFS file**, sparse and streaming — the `CC-50` provisioning path
    /// that never materializes a dense in-RAM image. The content is identical to
    /// [`assemble_bootable`](Self::assemble_bootable) (the same overlay + injected
    /// [`DEVCONTAINER_INIT`](holospaces::machine::DEVCONTAINER_INIT) + `disk_bytes`
    /// sizing), but instead of returning a `Vec` sized to the whole disk it writes
    /// only the **non-zero 4 KiB blocks** to `rootfs_handle` at their byte offsets
    /// via the shared streaming serializer
    /// ([`stream_ext4_image_bootable`](holospaces::assembly::stream_ext4_image_bootable)) —
    /// the very primitive [`DevcontainerProvision::assemble_into_opfs`] uses. The
    /// file's free space stays sparse (zero on read); peak wasm heap tracks the
    /// image's *content*, not its declared size ("the KappaStore IS the memory, RAM
    /// is a cache", Laws L3/L4).
    ///
    /// Returns the total image length in bytes. The page then boots the file with
    /// [`boot_devcontainer_routed_opfs_streamed`](Workspace::boot_devcontainer_routed_opfs_streamed),
    /// which pages the disk sector-by-sector — so the streamed-into-OPFS image is
    /// what actually boots (not a dense image that merely shares its bytes).
    #[wasm_bindgen(js_name = assembleBootableIntoOpfs)]
    pub fn assemble_bootable_into_opfs(
        &self,
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        disk_bytes: f64,
    ) -> Result<f64, JsValue> {
        // Overlay the layers one at a time (each decompressed tar drops before the
        // next), and stream the ext4 image block-by-block into the OPFS file —
        // never materializing the dense disk-sized image (Laws L3/L4).
        let mut layer_idx = 0usize;
        let next_layer =
            || -> Result<Option<(String, Vec<u8>)>, holospaces::assembly::AssemblyError> {
                if layer_idx >= self.layers.len() {
                    return Ok(None);
                }
                let (mt, b) = &self.layers[layer_idx];
                layer_idx += 1;
                Ok(Some((mt.clone(), b.clone())))
            };

        let mut io_err: Option<JsValue> = None;
        let geom = holospaces::assembly::stream_ext4_image_bootable_streamed_layers(
            next_layer,
            holospaces::machine::DEVCONTAINER_INIT,
            disk_bytes as u64,
            |block_index, bytes| {
                if io_err.is_some() {
                    return;
                }
                let opts = web_sys::FileSystemReadWriteOptions::new();
                opts.set_at((block_index * bytes.len() as u64) as f64);
                if let Err(e) = rootfs_handle.write_with_u8_array_and_options(bytes, &opts) {
                    io_err = Some(e);
                }
            },
        )
        .map_err(js_err)?;
        if let Some(e) = io_err {
            return Err(e);
        }
        let image_len = geom.image_len();
        // Grow the file to span the full image so the trailing sparse region reads
        // back as zeros (OPFS truncate grows with a hole).
        rootfs_handle.truncate_with_f64(image_len as f64)?;
        Ok(image_len as f64)
    }

    /// Like [`assembleBootableIntoOpfs`](Self::assemble_bootable_into_opfs), but
    /// **COMPACT + occupancy-tracked**: the non-zero blocks are written *contiguously*
    /// (the Nth emitted block at file offset `N·4096`, not at its disk offset), so the
    /// rootfs file is **O(content)** — a few MiB — no matter how large the declared
    /// disk. The `occupancy_handle` sidecar records the disk geometry and placement:
    /// an 8-byte little-endian `image_len` header, then the disk block index of each
    /// packed block (little-endian `u64`, in packed order). That is what lets the
    /// deployed disk be **arbitrarily large** in the browser: a sparse file truncated
    /// to the declared size would count its *logical* size against the origin's OPFS
    /// quota (a multi-GiB disk fails to stage), whereas the compact file costs only
    /// the image's content. [`X64Workspace::boot_devcontainer_opfs_streamed_occupancy`]
    /// reads it back, reconstructing the κ-disk O(content). Returns the image length
    /// (the declared disk the guest sees), as the untracked variant does.
    #[wasm_bindgen(js_name = assembleBootableIntoOpfsTracked)]
    pub fn assemble_bootable_into_opfs_tracked(
        &self,
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        occupancy_handle: web_sys::FileSystemSyncAccessHandle,
        disk_bytes: f64,
    ) -> Result<f64, JsValue> {
        let mut layer_idx = 0usize;
        let next_layer =
            || -> Result<Option<(String, Vec<u8>)>, holospaces::assembly::AssemblyError> {
                if layer_idx >= self.layers.len() {
                    return Ok(None);
                }
                let (mt, b) = &self.layers[layer_idx];
                layer_idx += 1;
                Ok(Some((mt.clone(), b.clone())))
            };

        let mut io_err: Option<JsValue> = None;
        // The sidecar: an 8-byte image_len header (filled in after assembly) + the
        // disk block index of each packed block, in packed order.
        let mut occupancy: Vec<u8> = 0u64.to_le_bytes().to_vec();
        let mut packed_blocks: u64 = 0;
        let geom = holospaces::assembly::stream_ext4_image_bootable_streamed_layers(
            next_layer,
            holospaces::machine::DEVCONTAINER_INIT,
            disk_bytes as u64,
            |block_index, bytes| {
                if io_err.is_some() {
                    return;
                }
                occupancy.extend_from_slice(&block_index.to_le_bytes());
                let opts = web_sys::FileSystemReadWriteOptions::new();
                // Contiguous placement: the Nth block at N·blocklen, not its disk offset.
                opts.set_at((packed_blocks * bytes.len() as u64) as f64);
                if let Err(e) = rootfs_handle.write_with_u8_array_and_options(bytes, &opts) {
                    io_err = Some(e);
                }
                packed_blocks += 1;
            },
        )
        .map_err(js_err)?;
        if let Some(e) = io_err {
            return Err(e);
        }
        let image_len = geom.image_len();
        // The compact rootfs is exactly the packed blocks (no sparse tail).
        rootfs_handle.truncate_with_f64((packed_blocks * BLOCK_BYTES) as f64)?;
        // Stamp the image_len header, then persist the sidecar.
        occupancy[0..8].copy_from_slice(&(image_len as u64).to_le_bytes());
        occupancy_handle.truncate_with_f64(0.0)?;
        let opts = web_sys::FileSystemReadWriteOptions::new();
        opts.set_at(0.0);
        occupancy_handle.write_with_u8_array_and_options(&occupancy, &opts)?;
        Ok(image_len as f64)
    }
}

/// The ext4 block size the streaming assembler emits (`emit(block_index, &block)`),
/// in bytes — the unit the compact occupancy-tracked rootfs packs by.
const BLOCK_BYTES: u64 = 4096;

impl Default for DevcontainerImage {
    fn default() -> Self {
        Self::new()
    }
}

/// The Platform Manager console, running as a browser peer that composes the
/// substrate runtime over the interpreter `ContainerEngine`.
#[wasm_bindgen]
pub struct Console {
    runtime: Runtime<BareMetalEngine, MemKappaStore>,
    operator: Option<Operator>,
    holospaces: Vec<Kappa>,
    /// The latest configuration published per instance — `(instance κ, config κ,
    /// next seq)` (ADR-018; the control plane's reconfiguration record).
    configs: Vec<(Kappa, Kappa, u64)>,
    /// The content store the content network (`CC-38`) serves + caches into — the
    /// κ-content this peer offers other peers and fetches from them.
    content_store: Arc<MemKappaStore>,
    /// This peer's content-network endpoint over one transport. Its
    /// [`PacketLink`](holospaces::content_net::PacketLink) is the portable
    /// `NetworkInterface` the uor-native `BareNetSync` drives — the SAME interface
    /// a bare-metal peer uses (`CC-38`); the browser surface carries that link's
    /// frames over a real WebRTC data channel ([`WebRtcLink`]) via the product
    /// [`cn_pump`](Self::cn_pump) (`CC-49`), exactly as a NIC carries them on bare
    /// metal. Drives `BareNetSync` without naming the substrate sync type here.
    content: ContentPeer,
    /// An in-flight content-network fetch future, polled as the transport
    /// delivers frames (the browser's sync-poll discipline — no async runtime).
    cn_pending: Option<Pin<Box<dyn Future<Output = Option<Bytes>>>>>,
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl Console {
    /// Open a fresh console — a browser peer with a local content-addressed
    /// store and the interpreter container engine.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Console {
        let content_store = Arc::new(MemKappaStore::new());
        let content = ContentPeer::new(256 * 1024, content_store.clone());
        Console {
            runtime: Runtime::new(BareMetalEngine::new(), MemKappaStore::new()),
            operator: None,
            holospaces: Vec::new(),
            configs: Vec::new(),
            content_store,
            content,
            cn_pending: None,
        }
    }

    /// Open a **forging** browser peer — a malicious responder that answers every
    /// content-network fetch with `forged` bytes (which do not re-derive to the
    /// requested κ). It drives the SAME content-network seam (`cn_inbound` /
    /// `cn_outbound`) over the same transport, so a real WebRTC peer fetching from
    /// it receives a well-formed but forged response and **rejects it on receipt**
    /// (SPINE-4 / Law L5). This is the adversary the `CC-49` witness uses to prove
    /// a forging responder is refused — a genuine attacker, not a mock.
    #[must_use]
    pub fn new_forging(forged: &[u8]) -> Console {
        let content_store = Arc::new(MemKappaStore::new());
        let content = ContentPeer::new_forging(256 * 1024, forged.to_vec());
        Console {
            runtime: Runtime::new(BareMetalEngine::new(), MemKappaStore::new()),
            operator: None,
            holospaces: Vec::new(),
            configs: Vec::new(),
            content_store,
            content,
            cn_pending: None,
        }
    }

    /// Sign in by unlocking a self-sovereign key (not a server account,
    /// ADR-001). Returns the operator's content-addressed identity κ.
    pub fn sign_in(&mut self, key: &[u8]) -> String {
        let operator = Operator::from_public_key(key);
        let identity = operator.identity().as_str().to_owned();
        self.operator = Some(operator);
        identity
    }

    /// Provision a holospace from a `.holo` compute artifact (the *holo-file*
    /// compute form) with a memory budget, κ-addressing its parts into the
    /// peer's store (Law L2). Returns the holospace identity κ.
    pub fn provision(&mut self, code: &[u8], memory_bytes: f64) -> Result<String, JsValue> {
        let artifact = self.runtime.store().put("blake3", code).map_err(js_err)?;
        self.provision_source(Source::HoloFile { artifact }, memory_bytes)
    }

    /// Provision a holospace from a **devcontainer** for the management console
    /// (CC-12): the `devcontainer.json` is validated against the Dev Container
    /// spec (`CC-4`) and κ-addressed into the store; the holospace's identity is
    /// the content address of its devcontainer definition (reproducible — same
    /// source ⇒ same κ, Law L1). This *provisions* (records) the holospace; the
    /// operator *enters* it to boot its OS in the workspace IDE (`CC-13`).
    /// Returns the holospace identity κ.
    pub fn provision_devcontainer(
        &mut self,
        config_json: &[u8],
        arch: &str,
        memory_bytes: f64,
    ) -> Result<String, JsValue> {
        devcontainer::parse(config_json).map_err(js_err)?;
        let config = self
            .runtime
            .store()
            .put("blake3", config_json)
            .map_err(js_err)?;
        // The operator's **architecture** selection (`riscv64`/`aarch64`, the
        // Manager's arch picker — ADR-021) is part of the source, hence part of
        // the holospace's content-addressed identity (Law L1): it is fixed for
        // the guest's life, and the same devcontainer config under two ISAs is
        // two distinct holospaces. (Other guest settings —
        // lifecycle/network/storage/account — are mutable `CC-28` directives via
        // [`Console::configure`].) The locally-supplied config doubles as the
        // userland reference; the arch-specific emulator surface is resolved at
        // boot (`CC-9`).
        let source = Source::Devcontainer {
            repo: String::new(),
            reference: String::new(),
            config_path: "devcontainer.json".to_owned(),
            config,
            userland: config,
            arch: holospaces::Arch::from_id(arch).unwrap_or_default(),
        };
        self.provision_source(source, memory_bytes)
    }

    /// Provision a holospace from a **git repository reference** — the
    /// Codespaces/Gitpod launch: the operator names a repository URL + reference
    /// (not a pasted config) and holospaces runs it as a devcontainer.
    ///
    /// The repository's own `.devcontainer/devcontainer.json` is fetched by the
    /// operator's page from the repository host and **verified on receipt** (Law
    /// L5) before it crosses into the peer here as `config_json`; when the
    /// repository declares none, the page passes the **usable default** config
    /// (`buildpack-deps` — `curl`/`git` over apt; the Dev Container spec's
    /// default, `CC-20`/`import`) so *any* repository runs. The `(repo,
    /// reference, config, arch)` tuple is the [`Source::Devcontainer`], hence the
    /// holospace's content-addressed identity (Law L1): the same repository at
    /// the same reference under the same ISA is the **same** holospace
    /// (reproducible), and a different repository / reference / architecture is a
    /// **distinct** one. Returns the holospace identity κ.
    ///
    /// The architecture (`arch`: `"riscv64"` / `"aarch64"`) is the operator's
    /// launch-time selection and is fixed for the holospace's lifetime (ADR-021).
    #[allow(clippy::too_many_arguments)]
    pub fn provision_repo(
        &mut self,
        repo: &str,
        reference: &str,
        config_path: &str,
        config_json: &[u8],
        arch: &str,
        memory_bytes: f64,
    ) -> Result<String, JsValue> {
        devcontainer::parse(config_json).map_err(js_err)?;
        let config = self
            .runtime
            .store()
            .put("blake3", config_json)
            .map_err(js_err)?;
        let source = Source::Devcontainer {
            repo: repo.to_owned(),
            reference: reference.to_owned(),
            config_path: config_path.to_owned(),
            config,
            userland: config,
            arch: holospaces::Arch::from_id(arch).unwrap_or_default(),
        };
        self.provision_source(source, memory_bytes)
    }

    /// Provision a holospace from a *Wasm-recompiled userland* (the execution
    /// surface, the second compute form — ADR-008). The module is validated
    /// against the surface contract ([`validate_userland`]) before it is
    /// κ-addressed into the store, so only a substrate-valid userland can become
    /// a holospace's code. Returns the holospace identity κ.
    pub fn provision_userland(
        &mut self,
        module: &[u8],
        memory_bytes: f64,
    ) -> Result<String, JsValue> {
        surface::validate_userland(module).map_err(js_err)?;
        let entry = self.runtime.store().put("blake3", module).map_err(js_err)?;
        self.provision_source(Source::Userland { entry }, memory_bytes)
    }

    /// Boot a userland holospace **in the browser**: provision it, then spawn it
    /// through the substrate runtime over the interpreter `ContainerEngine`,
    /// capture a κ snapshot of its state (suspend), resume, and terminate — the
    /// execution surface running on the browser peer (ADR-008; RT2; `CC-6`).
    /// Returns the κ-label of the suspend snapshot (state is content, Law L3).
    pub fn boot_userland(&mut self, module: &[u8], memory_bytes: f64) -> Result<String, JsValue> {
        surface::validate_userland(module).map_err(js_err)?;
        let entry = self.runtime.store().put("blake3", module).map_err(js_err)?;
        let holospace = provision(
            self.runtime.store(),
            Source::Userland { entry },
            capabilities(memory_bytes),
        )
        .map_err(js_err)?;
        self.record(&holospace)?;
        Ok(self.boot(holospace)?.as_str().to_owned())
    }

    /// Import and run a **devcontainer in the browser** — the Codespaces/Gitpod
    /// scenario without a Docker daemon or a cloud VM (arc42 chapter 1, the
    /// motivating scenario; chapter 6). The `devcontainer.json` is validated
    /// against the Dev Container spec (`CC-4`); the κ-addressed Wasm `userland`
    /// its config selects is validated against the host-ABI surface (`CC-6`) and
    /// booted through the substrate runtime over the interpreter engine — same
    /// lifecycle as a native or remote peer (Q6). Returns the suspend snapshot κ.
    ///
    /// `arch` is the operator's **architecture selection** (the Manager GUI's
    /// arch picker; ADR-021) — `"riscv64"` or `"aarch64"`. It becomes part of the
    /// holospace's content-addressed identity, so it is fixed for the holospace's
    /// lifetime (an unknown id falls back to the default RISC-V target).
    // A flat JS-facing signature (the repository URL, reference, config path,
    // config + userland bytes, the architecture, and the memory budget) — the
    // wasm-bindgen boundary takes scalars/byte-slices, not a Rust struct.
    #[allow(clippy::too_many_arguments)]
    pub fn run_devcontainer(
        &mut self,
        repo: &str,
        reference: &str,
        config_path: &str,
        config_json: &[u8],
        userland_module: &[u8],
        arch: &str,
        memory_bytes: f64,
    ) -> Result<String, JsValue> {
        devcontainer::parse(config_json).map_err(js_err)?;
        surface::validate_userland(userland_module).map_err(js_err)?;
        let userland = self
            .runtime
            .store()
            .put("blake3", userland_module)
            .map_err(js_err)?;
        let config = self
            .runtime
            .store()
            .put("blake3", config_json)
            .map_err(js_err)?;
        let source = Source::Devcontainer {
            repo: repo.to_owned(),
            reference: reference.to_owned(),
            config_path: config_path.to_owned(),
            config,
            userland,
            arch: holospaces::Arch::from_id(arch).unwrap_or_default(),
        };
        let holospace =
            provision(self.runtime.store(), source, capabilities(memory_bytes)).map_err(js_err)?;
        self.record(&holospace)?;
        Ok(self.boot(holospace)?.as_str().to_owned())
    }

    fn provision_source(&mut self, source: Source, memory_bytes: f64) -> Result<String, JsValue> {
        let holospace =
            provision(self.runtime.store(), source, capabilities(memory_bytes)).map_err(js_err)?;
        self.record(&holospace)?;
        Ok(holospace.kappa().as_str().to_owned())
    }

    /// Record a provisioned holospace in the View and persist the roster.
    fn record(&mut self, holospace: &Holospace) -> Result<(), JsValue> {
        let kappa = holospace.kappa();
        if !self.holospaces.contains(&kappa) {
            self.holospaces.push(kappa);
        }
        self.persist_roster()
    }

    /// Boot a holospace through the substrate runtime over the interpreter
    /// engine, returning the κ snapshot of its suspended state. The lifecycle
    /// (boot → suspend → resume → terminate) runs entirely in the browser peer.
    fn boot(&self, holospace: Holospace) -> Result<Kappa, JsValue> {
        block_on(async {
            let mut session = Session::provision(&self.runtime, holospace);
            session.boot().await?;
            let snapshot = session.suspend().await?;
            session.resume().await?;
            session.terminate().await?;
            Ok::<Kappa, LifecycleError>(snapshot)
        })
        .map_err(js_err)
    }

    /// The console's View — a JSON projection of the operator and their
    /// holospaces (what the UI renders).
    pub fn view(&self) -> String {
        let operator = self.operator.as_ref().map_or("", |o| o.identity().as_str());
        let holospaces: Vec<&str> = self.holospaces.iter().map(Kappa::as_str).collect();
        serde_json::json!({ "operator": operator, "holospaces": holospaces }).to_string()
    }

    /// Resolve a holospace (or any κ) from this peer's own in-session store.
    /// Returns the bytes, or `undefined` if absent.
    ///
    /// This is a *trusted* read ([`ReadVerify::Trusted`], ADR-019): the store is
    /// the canonical memory and RAM is its cache (Law L3), so content that
    /// entered this session was already verified on the way in (on receipt, or
    /// by `put` construction). The deployed peer does not re-derive κ on every
    /// local read — that would treat its own canonical store as untrusted and is
    /// pure overhead. The re-derivation invariant still holds where untrusted
    /// bytes enter (the import/fetch boundary) and is exercised end-to-end in CI.
    pub fn resolve(&self, kappa: &str) -> Result<Option<Vec<u8>>, JsValue> {
        let kappa = parse_kappa(kappa)?;
        Resolver::resolve_local_with(self.runtime.store(), &kappa, ReadVerify::Trusted)
            .map(|opt| opt.map(|b| b.to_vec()))
            .map_err(js_err)
    }

    /// Receive content the operator's page fetched from a substrate **HTTP-CAS
    /// gateway** (`GET /cas/{κ}`, `hologram-net-http`) and admit it into this
    /// peer's store — the *receive* side of [`get_with_fetch`], realized for the
    /// browser where the async `fetch` is the page's and the verification is the
    /// peer's. The bytes are **verified by re-derivation against the requested
    /// κ** before they are admitted (Law L5): a gateway is untrusted, so content
    /// that does not re-derive to the κ the page asked for is **refused**, never
    /// stored. On success the content is cached locally (so a subsequent
    /// [`resolve`](Self::resolve) is a trusted read) and the κ is returned.
    ///
    /// This is what lets the browser peer boot a devcontainer it did **not**
    /// assemble locally: the page fetches the rootfs + kernel by κ from any
    /// hologram gateway, hands each blob here for verify-and-cache, and the
    /// content is then trustworthy substrate content — no bespoke server, no
    /// trust in the gateway (`CC-20`).
    ///
    /// [`get_with_fetch`]: hologram_substrate_core::get_with_fetch
    pub fn receive(&mut self, bytes: &[u8], kappa: &str) -> Result<String, JsValue> {
        let expected = parse_kappa(kappa)?;
        if !verify(bytes, &expected).map_err(js_err)? {
            return Err(JsValue::from_str(
                "content from the gateway does not re-derive to the requested κ — refused (Law L5)",
            ));
        }
        let axis = expected
            .sigma_axis()
            .ok_or_else(|| JsValue::from_str("unknown σ-axis"))?;
        let stored = self.runtime.store().put(axis, bytes).map_err(js_err)?;
        Ok(stored.as_str().to_owned())
    }

    /// Witness the **uor-native content network in the browser** — the "browser
    /// as a router" model (ADR-006; the substrate is the network). Two in-process
    /// peers are linked by a [`PacketLink`](holospaces::content_net::PacketLink)
    /// pair (an in-process stand-in for a WebRTC data channel) and each wrapped in
    /// hologram's `BareNetSync` — the substrate's own `KappaSync` over the
    /// `NetworkInterface` HAL. Peer B fetches content it does **not** hold from
    /// peer A over the substrate frame protocol (`fetch`/`announce`/`discover`),
    /// and the bytes are **verified by re-derivation on receipt** (SPINE-4)
    /// before they are accepted — exactly as a bare-metal or std peer does it, no
    /// central operator. Returns a JSON summary (the fetched content matched, an
    /// unheld κ resolves to nothing — no forging). This exercises the real wasm
    /// peer's content-network path against an in-process link; the live
    /// browser-to-browser transport over a real WebRTC data channel is the product
    /// [`cn_pump`](Self::cn_pump) (`CC-49`), witnessed across two tabs.
    pub fn content_network_selftest(&self) -> Result<String, JsValue> {
        use holospaces::content_net::{drive_fetch, peer, PacketLink};

        // Peer A holds content; peer B is empty. (Distinct stores — two peers.)
        let store_a: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());
        let content: &[u8] = b"uor-native content, delivered peer-to-peer with no central operator";
        let kappa = store_a.put("blake3", content).map_err(js_err)?;
        let store_b: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());

        // An in-process peer link (stands in for a WebRTC data channel between
        // tabs). The SAME `content_net::peer`/`PacketLink` a bare-metal peer uses
        // (`CC-38`) — so this witnesses the wasm peer on the shared protocol.
        let (link_a, link_b) = PacketLink::loopback_pair(256 * 1024);
        let peer_a = peer(link_a, store_a);
        let peer_b = peer(link_b, store_b);

        // B fetches A's content over the uor-native protocol (verify-on-receipt).
        let fetched_ok = drive_fetch(&peer_b, &peer_a, &kappa).as_deref() == Some(content);
        // A κ no peer holds resolves to nothing — no forging, no false content.
        let unheld = address(b"content that no peer holds");
        let absent_is_none = drive_fetch(&peer_b, &peer_a, &unheld).is_none();

        Ok(format!(
            r#"{{"fetched":{fetched_ok},"absent_is_none":{absent_is_none},"kappa":"{}"}}"#,
            kappa.as_str()
        ))
    }

    // ── Content network (CC-38 / CC-49) — the live transport seam ─────────────
    // This peer's content-network frames cross a real WebRTC data channel to
    // another browser peer ([`WebRtcLink`]): the product [`cn_pump`] drains this
    // peer's outbound frames onto the channel and delivers the channel's inbound
    // frames into this peer, in one product call — no test glue. A fetch is
    // poll-driven (`cn_fetch_start` then `cn_pump` + `cn_fetch_poll` as frames
    // flow) — the browser's sync-poll discipline, no async runtime.
    //
    // [`cn_pump`]: Self::cn_pump

    /// Publish bytes into this peer's content store so it can serve them to other
    /// peers over the content network (`CC-38`). Returns the κ that addresses
    /// them — the handle a peer fetches by.
    pub fn cn_put(&self, bytes: &[u8]) -> Result<String, JsValue> {
        let kappa = self.content_store.put("blake3", bytes).map_err(js_err)?;
        Ok(kappa.as_str().to_owned())
    }

    /// Deliver a content-network frame the transport received from the peer, and
    /// service it (answer an inbound fetch from local content, or record a
    /// response for an in-flight `cn_fetch`).
    pub fn cn_inbound(&self, frame: &[u8]) {
        self.content.inbound(frame.to_vec());
    }

    /// Drain the next content-network frame this peer wants to send over the
    /// transport, or `undefined` if none is queued.
    pub fn cn_outbound(&self) -> Option<Vec<u8>> {
        self.content.outbound()
    }

    /// **Announce** to the peer that this node holds `kappa`, over the content
    /// network (`CC-38` `announce`). This queues a `KIND_ANNOUNCE` frame for the
    /// transport; the next [`cn_pump`](Self::cn_pump) carries it across the real
    /// WebRTC data channel to the peer. A deployed tab calls `cn_announce(κ)` then
    /// `cn_pump(link)` to advertise content it holds — the same `BareNetSync`
    /// `announce` a bare-metal peer drives, only the carrier differs (`CC-49`).
    ///
    /// The substrate's `announce` emits the frame without awaiting a reply, so the
    /// future settles immediately (the frame is then in the outbound queue); the
    /// transport pump moves it. No fabrication, no central operator.
    pub fn cn_announce(&self, kappa: &str) -> Result<(), JsValue> {
        let kappa = parse_kappa(kappa)?;
        block_on(self.content.announce(kappa));
        Ok(())
    }

    /// **Discover** which κs the peer holds, over the content network (`CC-38`
    /// `discover`). This broadcasts a `KIND_DISCOVER_REQ` frame (queued for the
    /// transport) and returns a snapshot — as a JSON array of κ-strings — of the κs
    /// learned from peers' `KIND_DISCOVER_RES` replies so far. Because discovery is
    /// a round-trip, a deployed tab calls `cn_discover()` to send the request,
    /// `cn_pump(link)` (both peers) to carry the request and the reply across the
    /// real WebRTC data channel, then `cn_discover()` again to read the now-known
    /// holders. Re-issuing is idempotent: each call re-broadcasts and re-snapshots,
    /// so the witness loops it until a holder appears (or a deadline, fail-loud).
    ///
    /// This is the SAME `BareNetSync` `discover` a bare-metal peer drives; the
    /// WebRTC data channel only changes the carrier (`CC-49`). κs returned are
    /// hints (which peer to fetch from); the bytes themselves are still verified on
    /// receipt when fetched (Law L5) — discovery fabricates nothing.
    pub fn cn_discover(&self) -> Result<String, JsValue> {
        let kappas = block_on(self.content.discover());
        let list: Vec<String> = kappas.iter().map(|k| k.as_str().to_owned()).collect();
        serde_json::to_string(&list).map_err(js_err)
    }

    /// **The product pump (CC-49).** Carry this peer's content-network frames
    /// across a real WebRTC data channel ([`WebRtcLink`]) to another browser peer:
    /// drain every frame this peer wants to transmit onto the channel
    /// ([`WebRtcLink::send`]) and deliver every frame the channel received from the
    /// peer into this peer ([`WebRtcLink::recv`] → [`cn_inbound`]). This is the
    /// browser surface's transport pump for the uor-native content network — the
    /// counterpart to a real NIC's RX/TX on bare metal — and it lives **in the
    /// product**, not the witness: a deployed tab calls `cn_fetch_start`, then
    /// `cn_pump(link)` + `cn_fetch_poll` as the channel signals readiness, and so
    /// fetches a κ from a peer over WebRTC entirely through this API.
    ///
    /// The pump moves only opaque frames; it never inspects content or addressing.
    /// Verify-on-receipt (SPINE-4 / Law L5) happens inside the content peer, so a
    /// forged response carried over the channel is rejected on re-derivation and a
    /// κ no peer holds resolves to nothing — the channel changes the carrier, not
    /// the law. While the channel is not yet open ([`WebRtcLink::is_open`]) there
    /// are no frames to move and this is a no-op.
    ///
    /// Returns the number of frames moved (outbound + inbound) — diagnostic only;
    /// the caller re-polls regardless until the fetch settles.
    ///
    /// [`cn_inbound`]: Self::cn_inbound
    pub fn cn_pump(&self, link: &WebRtcLink) -> Result<usize, JsValue> {
        let mut moved = 0usize;
        // TX: every frame this peer wants to send goes onto the data channel.
        if link.is_open() {
            while let Some(frame) = self.content.outbound() {
                link.send(&frame)?;
                moved += 1;
            }
        }
        // RX: every frame the channel received from the peer is serviced here
        // (answer an inbound request from local content, or record a response for
        // an in-flight `cn_fetch` — verify-on-receipt inside the content peer).
        while let Some(frame) = link.recv() {
            self.content.inbound(frame);
            moved += 1;
        }
        Ok(moved)
    }

    /// Begin fetching `kappa` from the peer across the transport (verify on
    /// receipt). Drive it by pumping frames and polling [`cn_fetch_poll`]; only
    /// one fetch is in flight at a time.
    ///
    /// [`cn_fetch_poll`]: Self::cn_fetch_poll
    pub fn cn_fetch_start(&mut self, kappa: &str) -> Result<(), JsValue> {
        let kappa = parse_kappa(kappa)?;
        self.cn_pending = Some(self.content.fetch(kappa));
        Ok(())
    }

    /// Poll the in-flight content-network fetch. Returns `undefined` while it is
    /// pending (pump more frames and poll again), `null` when it completed with
    /// the content absent (no peer holds it — no forging), or the verified bytes
    /// when it resolved. The fetched bytes are also admitted to this peer's
    /// content store (a subsequent fetch of the same κ is local).
    pub fn cn_fetch_poll(&mut self) -> Result<JsValue, JsValue> {
        let Some(fut) = self.cn_pending.as_mut() else {
            return Ok(JsValue::UNDEFINED);
        };
        // A no-op waker: the page re-polls explicitly as the transport delivers
        // frames, so readiness is observed by the next poll, not by scheduling.
        let waker = Waker::noop().clone();
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Pending => Ok(JsValue::UNDEFINED),
            Poll::Ready(None) => {
                self.cn_pending = None;
                Ok(JsValue::NULL)
            }
            Poll::Ready(Some(bytes)) => {
                self.cn_pending = None;
                // Admit the fetched content locally (verified on receipt inside
                // the sync), so this peer can now serve it on too.
                let _ = self.content_store.put("blake3", &bytes);
                Ok(js_sys::Uint8Array::from(bytes.as_ref()).into())
            }
        }
    }

    /// The operator's roster κ — the content address that links their instances
    /// (R5). Its bytes are in the store, so another instance can resolve it.
    pub fn roster_kappa(&self) -> Option<String> {
        self.operator.as_ref().map(|op| {
            Roster::new(op, self.holospaces.clone())
                .kappa()
                .as_str()
                .to_owned()
        })
    }

    fn persist_roster(&self) -> Result<(), JsValue> {
        if let Some(op) = &self.operator {
            let roster = Roster::new(op, self.holospaces.clone());
            self.runtime
                .store()
                .put("blake3", &roster.canonicalize())
                .map_err(js_err)?;
        }
        Ok(())
    }

    /// *Control panel: configure.* Reconfigure a running instance from the panel
    /// (ADR-018; `CC-28`). `directives_json` is a JSON array of operations across
    /// the four classes, e.g. `[{"lifecycle":"suspend"}, {"forwardPort":8080},
    /// {"unforwardPort":8080}, {"network":{"fetch":true,"announce":false}},
    /// {"quota":1073741824}, {"grant":"blake3:…"}]`. The panel builds a
    /// content-addressed [`Configuration`] issued by the signed-in operator,
    /// stores it (Law L2), and returns its κ — the content the running instance
    /// resolves and applies over the substrate (no server, no RPC).
    pub fn configure(&mut self, instance: &str, directives_json: &str) -> Result<String, JsValue> {
        let operator = self
            .operator
            .as_ref()
            .ok_or_else(|| JsValue::from_str("sign in before configuring an instance"))?
            .identity()
            .to_owned();
        let instance = parse_kappa(instance)?;
        let directives = parse_directives(directives_json)?;
        let seq = self
            .configs
            .iter()
            .find(|(i, _, _)| *i == instance)
            .map_or(0, |(_, _, next)| *next);
        let config = Configuration::new(operator, instance, seq, directives);
        let kappa = config.kappa();
        self.runtime
            .store()
            .put("blake3", &config.canonicalize())
            .map_err(js_err)?;
        match self.configs.iter_mut().find(|(i, _, _)| *i == instance) {
            Some(entry) => {
                entry.1 = kappa;
                entry.2 = seq + 1;
            }
            None => self.configs.push((instance, kappa, seq + 1)),
        }
        Ok(kappa.as_str().to_owned())
    }
}

/// Parse the control panel's directive JSON (`Console::configure`).
fn parse_directives(json: &str) -> Result<Vec<Directive>, JsValue> {
    let arr: Vec<serde_json::Value> =
        serde_json::from_str(json).map_err(|e| JsValue::from_str(&format!("directives: {e}")))?;
    let mut out = Vec::with_capacity(arr.len());
    for d in &arr {
        let directive = if let Some(a) = d.get("lifecycle").and_then(|v| v.as_str()) {
            let action = match a {
                "start" => LifecycleAction::Start,
                "suspend" => LifecycleAction::Suspend,
                "resume" => LifecycleAction::Resume,
                "terminate" => LifecycleAction::Terminate,
                other => return Err(JsValue::from_str(&format!("unknown lifecycle: {other}"))),
            };
            Directive::Lifecycle(action)
        } else if let Some(p) = d.get("forwardPort").and_then(serde_json::Value::as_u64) {
            Directive::ForwardPort(u16::try_from(p).map_err(|_| JsValue::from_str("bad port"))?)
        } else if let Some(p) = d.get("unforwardPort").and_then(serde_json::Value::as_u64) {
            Directive::UnforwardPort(u16::try_from(p).map_err(|_| JsValue::from_str("bad port"))?)
        } else if let Some(n) = d.get("network") {
            Directive::SetNetwork {
                fetch: n
                    .get("fetch")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                announce: n
                    .get("announce")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            }
        } else if let Some(q) = d.get("quota").and_then(serde_json::Value::as_u64) {
            Directive::SetStorageQuota(q)
        } else if let Some(op) = d.get("grant").and_then(|v| v.as_str()) {
            Directive::GrantAccess(parse_kappa(op)?)
        } else {
            return Err(JsValue::from_str("unrecognized directive"));
        };
        out.push(directive);
    }
    Ok(out)
}

/// A **workspace** over a running holospace, in the browser tab — the
/// Codespaces/Gitpod experience (ADR-009; `CC-9` + `CC-11`). The operator
/// launches a holospace whose code is the system emulator; it **boots a real
/// operating system** (the [system emulator](holospaces::emulator) running in
/// the browser's own wasm engine), and the [workspace
/// projection](holospaces::projection) drives it: a live **terminal**
/// (keystrokes published as canonical events that advance the holospace's κ
/// snapshot) and an **editor** that reads and edits environment content *by κ*.
///
/// The boot runs in instruction *chunks* ([`run`](Workspace::run)) so the UI
/// stays responsive and can stream the console as the kernel boots — there is no
/// server doing the work; the browser peer *is* the machine (Law L1).
#[wasm_bindgen]
pub struct Workspace {
    machine: Emulator,
    store: MemKappaStore,
    channel: Vec<Kappa>,
    files: std::collections::BTreeMap<String, Kappa>,
    halted: bool,
    /// The next unread byte of the console buffer — the cursor [`Workspace::terminal_delta`]
    /// advances, so the integrated terminal streams only newly-produced output
    /// instead of re-reading the whole console each tick.
    console_cursor: usize,
    /// The router seam, present when the guest's egress is carried by an external
    /// router (the extension / a node). The page pumps it via
    /// [`egress_outbound`](Workspace::egress_outbound) /
    /// [`egress_inbound`](Workspace::egress_inbound); `None` for the bridged /
    /// relay boots.
    router: Option<net::RouterChannel>,
}

#[wasm_bindgen]
impl Workspace {
    /// Launch a workspace: place the OS `kernel` image and `dtb` in a machine
    /// with `ram_bytes` of RAM at `base`, the device tree at `dtb_addr`, and hand
    /// off as the SBI firmware. The machine is now booting (drive it with
    /// [`run`](Workspace::run)).
    pub fn boot(
        kernel: &[u8],
        dtb: &[u8],
        ram_bytes: f64,
        base: f64,
        dtb_addr: f64,
    ) -> Result<Workspace, JsValue> {
        let mut machine = Emulator::new(base as u64, ram_bytes as usize);
        machine
            .boot_kernel(kernel, dtb, dtb_addr as u64)
            .map_err(js_err)?;
        Ok(Workspace {
            machine,
            store: MemKappaStore::new(),
            channel: Vec::new(),
            files: std::collections::BTreeMap::new(),
            halted: false,
            console_cursor: 0,
            router: None,
        })
    }

    /// Boot a **devcontainer** workspace: the Boot Orchestrator
    /// ([`MachineSpec`]) generates the device
    /// tree and boots `kernel` on a machine whose `virtio-blk` disk is the
    /// assembled `rootfs` (from [`DevcontainerImage::assemble`]). The guest
    /// kernel mounts the rootfs over `/dev/vda` and runs the devcontainer's real
    /// OS — entirely in the browser peer (`CC-14`). Drive it with
    /// [`run`](Workspace::run), exactly like [`boot`](Workspace::boot).
    pub fn boot_devcontainer(kernel: &[u8], rootfs: &[u8]) -> Result<Workspace, JsValue> {
        // Attach the shared `virtio-9p` workspace (CC-15) so the workbench's
        // FileSystemProvider (`ws_list`/`ws_read`/`ws_write`) and the OS share the
        // same files — the editor edits the content the devcontainer OS sees.
        let machine = MachineSpec::devcontainer()
            .boot_workspace(kernel, rootfs.to_vec(), &[])
            .map_err(js_err)?;
        Ok(Workspace {
            machine,
            store: MemKappaStore::new(),
            channel: Vec::new(),
            files: std::collections::BTreeMap::new(),
            halted: false,
            console_cursor: 0,
            router: None,
        })
    }

    /// Boot a **networked** devcontainer workspace (`CC-16`): like
    /// [`boot_devcontainer`](Workspace::boot_devcontainer), but the machine also
    /// has a `virtio-net` device whose userspace TCP/IP NAT tunnels the guest's
    /// TCP streams out over a WebSocket to the relay at `relay_url` (there is no
    /// raw NIC behind a tab; ADR-014). The guest brings its interface up with
    /// DHCP and can then reach the internet — `git clone`, `apt`, `npm` — from the
    /// browser peer. Drive it with [`run`](Workspace::run), yielding to the event
    /// loop between chunks so the WebSocket delivers host-side bytes.
    pub fn boot_devcontainer_net(
        kernel: &[u8],
        rootfs: &[u8],
        relay_url: &str,
    ) -> Result<Workspace, JsValue> {
        let egress = wsnet::WsEgress::connect_relay(relay_url)?;
        let machine = MachineSpec::devcontainer_net()
            .boot_net(kernel, rootfs.to_vec(), Box::new(egress))
            .map_err(js_err)?;
        Ok(Workspace {
            machine,
            store: MemKappaStore::new(),
            channel: Vec::new(),
            files: std::collections::BTreeMap::new(),
            halted: false,
            console_cursor: 0,
            router: None,
        })
    }

    /// Boot a devcontainer with the **in-process loopback bridge** enabled
    /// (ADR-020, `CC-33`): the guest's interface comes up with DHCP (so it has a
    /// real TCP stack), but instead of a WebSocket egress to the internet it gets
    /// a no-op egress and the *loopback ingress* — so the workbench, in this same
    /// process, can [`dial_guest`](Workspace::dial_guest) a server *inside* the
    /// devcontainer (a language server, a remote extension host) and exchange a
    /// byte stream with it, with no relay or socket. This is the transport the VS
    /// Code remote model runs over in the browser peer (ADR-015/ADR-020). Drive it
    /// with [`run`](Workspace::run), pumping the NAT so the bridge's bytes flow.
    pub fn boot_devcontainer_bridged(kernel: &[u8], rootfs: &[u8]) -> Result<Workspace, JsValue> {
        let mut machine = MachineSpec::devcontainer_net()
            .boot_workspace_net(kernel, rootfs.to_vec(), &[], Box::new(net::NoEgress))
            .map_err(js_err)?;
        machine.enable_loopback();
        Ok(Workspace {
            machine,
            store: MemKappaStore::new(),
            channel: Vec::new(),
            files: std::collections::BTreeMap::new(),
            halted: false,
            console_cursor: 0,
            router: None,
        })
    }

    /// Boot a devcontainer whose guest egress is carried by an external
    /// **router** — the router extension (`CC-41`) or a node (`CC-39`) — over the
    /// egress protocol ([`ChannelEgress`](holospaces::emulator::net::ChannelEgress)).
    /// The guest comes up with DHCP and a real TCP stack; the page carries its
    /// traffic to the router by pumping the seam (drain
    /// [`egress_outbound`](Workspace::egress_outbound), feed
    /// [`egress_inbound`](Workspace::egress_inbound)), and the router opens the
    /// real sockets a tab cannot — so the guest's package managers, network
    /// config, and apps reach the internet (Codespaces parity), with no relay and
    /// no proxy. Drive with [`run`](Workspace::run), pumping the seam each tick.
    pub fn boot_devcontainer_routed(kernel: &[u8], rootfs: &[u8]) -> Result<Workspace, JsValue> {
        let (egress, router) = net::ChannelEgress::new();
        let machine = MachineSpec::devcontainer_net()
            .boot_net(kernel, rootfs.to_vec(), Box::new(egress))
            .map_err(js_err)?;
        Ok(Workspace {
            machine,
            store: MemKappaStore::new(),
            channel: Vec::new(),
            files: std::collections::BTreeMap::new(),
            halted: false,
            console_cursor: 0,
            router: Some(router),
        })
    }

    /// Boot like [`boot_devcontainer_routed`](Workspace::boot_devcontainer_routed),
    /// but page the guest's disk from an **OPFS-backed store** (`handle` is an
    /// OPFS `FileSystemSyncAccessHandle` the worker opened) — so the disk's
    /// sectors live off the wasm heap and a large real image boots without holding
    /// it all in RAM (the paged κ-disk; "the KappaStore IS the memory, RAM is a
    /// cache"). Egress is routed (`ChannelEgress`); drive with
    /// [`run`](Workspace::run), pumping the router seam each tick.
    pub fn boot_devcontainer_routed_opfs(
        kernel: &[u8],
        rootfs: &[u8],
        disk_handle: web_sys::FileSystemSyncAccessHandle,
    ) -> Result<Workspace, JsValue> {
        let (egress, router) = net::ChannelEgress::new();
        let store = Box::new(opfs_store::OpfsKappaStore::new(disk_handle));
        let machine = MachineSpec::devcontainer_net()
            .boot_net_in(kernel, rootfs.to_vec(), Box::new(egress), store)
            .map_err(js_err)?;
        Ok(Workspace {
            machine,
            store: MemKappaStore::new(),
            channel: Vec::new(),
            files: std::collections::BTreeMap::new(),
            halted: false,
            console_cursor: 0,
            router: Some(router),
        })
    }

    /// Boot the paged κ-disk by **streaming** the rootfs from one OPFS file into
    /// an OPFS-backed store in another — the *transient-peak-free* path: neither
    /// the full rootfs nor the assembled image is ever held in wasm RAM.
    /// `rootfs_handle` is a sync access handle on the provisioned rootfs file (read
    /// sector-by-sector); `disk_handle` is the κ-store pack. Egress is routed.
    pub fn boot_devcontainer_routed_opfs_streamed(
        kernel: &[u8],
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        disk_handle: web_sys::FileSystemSyncAccessHandle,
    ) -> Result<Workspace, JsValue> {
        let (egress, router) = net::ChannelEgress::new();
        let store = Box::new(opfs_store::OpfsKappaStore::new(disk_handle));
        let total = rootfs_handle.get_size().map_err(js_err)? as u64;
        let sector_count = total.div_ceil(512);
        let rootfs = rootfs_handle.clone();
        let read = move |i: u64, buf: &mut [u8]| {
            let opts = web_sys::FileSystemReadWriteOptions::new();
            opts.set_at((i * 512) as f64);
            // A short read at the tail leaves the rest of `buf` zero (sparse pad).
            let _ = rootfs.read_with_u8_array_and_options(buf, &opts);
        };
        let machine = MachineSpec::devcontainer_net()
            .boot_net_streamed(kernel, sector_count, read, Box::new(egress), store)
            .map_err(js_err)?;
        // The κ-disk is fully ingested (every sector read up front), so release the
        // rootfs file's exclusive OPFS sync-access lock — otherwise re-provisioning
        // or removing `provisioned/<id>` is blocked and the handle leaks (quota).
        rootfs_handle.close();
        Ok(Workspace {
            machine,
            store: MemKappaStore::new(),
            channel: Vec::new(),
            files: std::collections::BTreeMap::new(),
            halted: false,
            console_cursor: 0,
            router: Some(router),
        })
    }

    /// Drain the next egress frame the guest produced, for the page to carry to
    /// the router. `undefined` when none is queued (or this is not a routed boot).
    #[must_use]
    pub fn egress_outbound(&self) -> Option<Vec<u8>> {
        self.router
            .as_ref()
            .and_then(net::RouterChannel::pop_outbound)
    }

    /// Deliver an egress frame the router returned (the host's bytes / connection
    /// events) into the guest's network. A no-op when this is not a routed boot.
    pub fn egress_inbound(&self, frame: &[u8]) {
        if let Some(r) = &self.router {
            r.feed_inbound(frame);
        }
    }

    /// Suspend the running machine to a κ snapshot — the canonical,
    /// content-addressed bytes of the whole machine: CPU, RAM, the rootfs disk,
    /// and the *workspace files* (virtio-9p). The browser persists these (gzipped)
    /// to OPFS so the next launch *resumes* instead of cold-booting (`CC-30`).
    /// Most of guest RAM is zero, so the gzipped snapshot is a small fraction of
    /// the machine size.
    pub fn suspend(&self) -> Vec<u8> {
        self.machine.snapshot()
    }

    /// Resume a devcontainer workspace from a κ snapshot [`suspend`](Workspace::suspend)
    /// produced, instead of cold-booting it (`CC-30`). The running OS, its disk,
    /// and the workspace files come back exactly — so a second launch skips the
    /// boot entirely and the editor's content is intact. The snapshot's integrity
    /// is the caller's to check by re-derivation before trusting it across a
    /// session boundary (Law L5; ADR-019) — OPFS is durable but untrusted storage.
    pub fn resume_devcontainer(snapshot: &[u8]) -> Result<Workspace, JsValue> {
        let base = MachineSpec::devcontainer().base;
        let machine = Emulator::restore(base, snapshot).map_err(js_err)?;
        Ok(Workspace {
            machine,
            store: MemKappaStore::new(),
            channel: Vec::new(),
            files: std::collections::BTreeMap::new(),
            halted: false,
            console_cursor: 0,
            router: None,
        })
    }

    /// Advance the running holospace by `budget` instructions (one chunk of the
    /// boot or of servicing input). Returns `true` once the machine has halted
    /// (powered off). Call repeatedly from a UI loop, rendering
    /// [`terminal`](Workspace::terminal) between chunks.
    pub fn run(&mut self, budget: f64) -> bool {
        if self.halted {
            return true;
        }
        if !matches!(self.machine.run(budget as u64), Halt::OutOfBudget) {
            self.halted = true;
        }
        self.halted
    }

    /// Whether the machine has powered off.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn halted(&self) -> bool {
        self.halted
    }

    /// The rendered terminal — the console the running holospace has produced.
    #[must_use]
    pub fn terminal(&self) -> String {
        String::from_utf8_lossy(self.machine.console()).into_owned()
    }

    /// Whether the terminal has rendered `marker` yet (e.g. the ready banner).
    #[must_use]
    pub fn shows(&self, marker: &str) -> bool {
        self.machine
            .console()
            .windows(marker.len())
            .any(|w| w == marker.as_bytes())
    }

    /// Type a line into the terminal: publish it as a canonical event on the
    /// holospace's channel (Law L1/L2), feed the keystrokes to the running
    /// machine, and run until the response settles. The holospace's κ snapshot
    /// advances. Returns the event's κ.
    pub fn type_line(&mut self, line: &str) -> String {
        // Feed the keystrokes + advance the machine (the projection runs the same
        // intent on the live OS).
        {
            let mut projection = Projection::attach(&mut self.machine);
            projection.type_line(line, 400_000_000);
        }
        // Publish the event as *resolvable content* on the channel: the canonical
        // event bytes are stored, so its κ re-derives and resolves (the KappaStore
        // IS the memory, Law L3) — not a dangling label. `put` content-addresses,
        // so the stored κ equals the event's identity (Law L1/L2).
        let canonical = Intent::Type(line.to_owned()).canonicalize();
        let event = self
            .store
            .put("blake3", &canonical)
            .unwrap_or_else(|_| address(&canonical));
        let event_str = event.as_str().to_owned();
        self.channel.push(event);
        // The `exit` line powers the machine off; reflect that in the workspace.
        if self.shows("WORKSPACE-DONE") {
            self.halted = true;
        }
        event_str
    }

    /// Feed **raw terminal input** to the running holospace — the bytes an
    /// interactive terminal delivers for each keystroke, *unbuffered*: ordinary
    /// characters, control bytes (Ctrl-C = `0x03`, Ctrl-D = `0x04`), and escape
    /// sequences (arrows, Home/End). Unlike [`Workspace::type_line`] this does not
    /// line-buffer or block: the bytes go to the guest console and the caller's
    /// render loop ([`Workspace::run`] + [`Workspace::terminal_delta`]) advances
    /// the machine, so the guest's own tty echoes and edits the line and Ctrl-C
    /// raises SIGINT — a real terminal, not a line submitter. The input is part of
    /// the machine's canonical state (it is captured in the κ snapshot), so the
    /// session stays reproducible (Law L1).
    pub fn feed_input(&mut self, bytes: &[u8]) {
        self.machine.feed_console(bytes);
    }

    /// The console bytes produced **since the last call** (an internal cursor),
    /// for the integrated terminal's render loop. Returning only the delta avoids
    /// re-reading and re-encoding the whole console each tick — output stays O(new
    /// bytes), not O(total) per frame. Returns raw bytes (the terminal decodes
    /// them); [`Workspace::terminal`] still returns the full buffer for tests.
    pub fn terminal_delta(&mut self) -> Vec<u8> {
        let console = self.machine.console();
        let from = self.console_cursor.min(console.len());
        let delta = console[from..].to_vec();
        self.console_cursor = console.len();
        delta
    }

    /// Dial an in-process connection to a server *inside* the devcontainer,
    /// listening on `guest_port`, over the loopback substrate bridge (ADR-020,
    /// `CC-33`). Returns the connection id, or `None` if the machine was not booted
    /// with the bridge ([`boot_devcontainer_bridged`](Workspace::boot_devcontainer_bridged)).
    /// The workbench uses this to reach a language server / the remote extension
    /// host (ADR-015) without a relay or socket. Pump with [`run`](Workspace::run)
    /// so the NAT opens the connection and the byte stream flows.
    pub fn dial_guest(&mut self, guest_port: u16) -> Option<u32> {
        self.machine.dial_guest(guest_port)
    }

    /// Write bytes toward the guest server on a loopback connection (`CC-33`).
    pub fn guest_send(&mut self, id: u32, data: &[u8]) {
        self.machine.guest_send(id, data);
    }

    /// Drain the guest server's reply bytes on a loopback connection (empty until
    /// the machine is pumped enough for the stream to advance; `CC-33`).
    pub fn guest_recv(&mut self, id: u32) -> Vec<u8> {
        self.machine.guest_recv(id)
    }

    /// Close the host side of a loopback connection (`CC-33`).
    pub fn guest_close(&mut self, id: u32) {
        self.machine.guest_close(id);
    }

    /// Whether a loopback connection is still usable — the guest has not closed it,
    /// or has but unread bytes remain (`CC-33`).
    #[must_use]
    pub fn guest_is_open(&self, id: u32) -> bool {
        self.machine.guest_is_open(id)
    }

    /// The running holospace's κ snapshot — its canonical state (Law L1/L3/L5).
    #[must_use]
    pub fn state_kappa(&self) -> String {
        address(&self.machine.snapshot()).as_str().to_owned()
    }

    /// The κ of every operator event published on the terminal channel so far.
    #[must_use]
    pub fn channel(&self) -> Vec<JsValue> {
        self.channel
            .iter()
            .map(|k| JsValue::from_str(k.as_str()))
            .collect()
    }

    /// The **editor** surface: save a file's content (the operator's edit). The
    /// content is κ-addressed into the substrate (Law L2), so the returned κ is
    /// the file's new identity — an edit advances it (Law L1). The canonical edit
    /// event for `path` is published on the channel.
    pub fn save_file(&mut self, path: &str, content: &[u8]) -> Result<String, JsValue> {
        let intent = Intent::Edit {
            path: path.to_owned(),
            content: content.to_vec(),
        };
        // Publish the canonical event as *resolvable content* on the channel — its
        // bytes are stored so the κ re-derives and resolves (Law L1/L3), not a
        // dangling label — then store the file content itself.
        let event = self
            .store
            .put("blake3", &intent.canonicalize())
            .map_err(js_err)?;
        self.channel.push(event);
        let stored = self.store.put("blake3", content).map_err(js_err)?;
        self.files.insert(path.to_owned(), stored);
        Ok(stored.as_str().to_owned())
    }

    /// The editor's read: fetch a file's content *by κ*, verifying it by
    /// re-derivation (Law L5). `undefined` if it is not in the workspace store.
    pub fn open_file(&self, kappa: &str) -> Result<Option<Vec<u8>>, JsValue> {
        let kappa = parse_kappa(kappa)?;
        self.store
            .get(&kappa)
            .map(|opt| opt.map(|b| b.to_vec()))
            .map_err(js_err)
    }

    /// The **file tree**: the workspace's files as a JSON array of
    /// `{ path, kappa }` — each file's current content κ (its identity, Law L1).
    /// What the editor's explorer renders.
    #[must_use]
    pub fn files(&self) -> String {
        let entries: Vec<serde_json::Value> = self
            .files
            .iter()
            .map(|(path, kappa)| serde_json::json!({ "path": path, "kappa": kappa.as_str() }))
            .collect();
        serde_json::Value::Array(entries).to_string()
    }

    /// Open a file *by path*: the content at the file's current κ (the editor
    /// reads the environment content by κ). `undefined` if the path is unknown.
    pub fn read_path(&self, path: &str) -> Result<Option<Vec<u8>>, JsValue> {
        match self.files.get(path) {
            Some(kappa) => self
                .store
                .get(kappa)
                .map(|opt| opt.map(|b| b.to_vec()))
                .map_err(js_err),
            None => Ok(None),
        }
    }

    // ── the workbench's filesystem: the shared virtio-9p workspace (CC-15/CC-17) ──
    //
    // The real VS Code web workbench (CC-17) edits the holospace's files through a
    // `FileSystemProvider`. Per ADR-012/015 that provider is the running
    // holospace's `virtio-9p` workspace — the κ-addressed content the editor and
    // the OS *share* (Law L1), not a separate store. A service worker bridges the
    // workbench's web-extension provider to these methods on the wasm peer, so the
    // editor reads and writes the *same content* the devcontainer OS sees.

    /// The shared workspace's directory listing — a JSON array of
    /// `{ name, dir, size }` over the running holospace's `virtio-9p` workspace
    /// (the workbench `FileSystemProvider.readDirectory`).
    #[must_use]
    pub fn ws_list(&self) -> String {
        let entries: Vec<serde_json::Value> = self
            .machine
            .workspace_list()
            .into_iter()
            .map(|(name, dir, size)| serde_json::json!({ "name": name, "dir": dir, "size": size }))
            .collect();
        serde_json::Value::Array(entries).to_string()
    }

    /// Read a file from the shared workspace (the workbench
    /// `FileSystemProvider.readFile`) — the same content the OS reads over
    /// `virtio-9p`. `undefined` if absent.
    #[must_use]
    pub fn ws_read(&self, name: &str) -> Option<Vec<u8>> {
        self.machine.workspace_file(name).map(<[u8]>::to_vec)
    }

    /// Write a file into the shared workspace (the workbench
    /// `FileSystemProvider.writeFile`) — the editor saving the *same content* the
    /// OS reads over `virtio-9p` (one content, Law L1). Returns the content's κ
    /// (its identity, Law L1/L2).
    pub fn ws_write(&mut self, name: &str, content: &[u8]) -> String {
        self.machine.workspace_write(name, content);
        address(content).as_str().to_owned()
    }

    /// Delete a file or folder from the shared workspace (the workbench
    /// `FileSystemProvider.delete`) — the editor removing content the OS sees
    /// over `virtio-9p`. `true` if it existed.
    pub fn ws_delete(&mut self, name: &str) -> bool {
        self.machine.workspace_delete(name)
    }

    /// Rename a file or folder in the shared workspace (the workbench
    /// `FileSystemProvider.rename`). `true` if the source existed.
    pub fn ws_rename(&mut self, from: &str, to: &str) -> bool {
        self.machine.workspace_rename(from, to)
    }

    /// Create a folder in the shared workspace (the workbench
    /// `FileSystemProvider.createDirectory`).
    pub fn ws_mkdir(&mut self, name: &str) {
        self.machine.workspace_mkdir(name);
    }

    // ── Nested-path workspace API (`CC-15` parity with the guest tree, `CC-51`) ──
    // The flat `ws_*` above address only the share root; these address the full
    // nested tree (a `.git` object store, `src/…` sources) the guest reads over
    // `virtio-9p` — one content, Law L1. The workbench file explorer and the
    // Source Control provider's Git engine (native browser-peer exec, the `CC-48`
    // discipline) drive a real repository through these.

    /// Read a file by nested path (e.g. `.git/HEAD`, `src/main.rs`) from the
    /// shared workspace. `undefined` if absent or a directory.
    #[must_use]
    pub fn ws_read_path(&self, path: &str) -> Option<Vec<u8>> {
        self.machine.workspace_file_path(path).map(<[u8]>::to_vec)
    }

    /// Write a file at a nested path, creating parent directories — the editor /
    /// the Git engine saving the *same content* the OS reads over `virtio-9p`
    /// (Law L1). Returns the content's κ (its identity).
    pub fn ws_write_path(&mut self, path: &str, content: &[u8]) -> String {
        self.machine.workspace_write_path(path, content);
        address(content).as_str().to_owned()
    }

    /// List a directory by nested path — a JSON array `[{name,dir,size}]` in name
    /// order, or `null` if the path is absent or not a directory.
    #[must_use]
    pub fn ws_list_path(&self, path: &str) -> Option<String> {
        self.machine.workspace_list_path(path).map(|entries| {
            let arr: Vec<serde_json::Value> = entries
                .into_iter()
                .map(|(name, dir, size)| serde_json::json!({ "name": name, "dir": dir, "size": size }))
                .collect();
            serde_json::Value::Array(arr).to_string()
        })
    }

    /// Stat a nested path — a JSON object `{dir,size}`, or `null` if absent. The
    /// Git engine's `stat`/`lstat` over the workspace FS.
    #[must_use]
    pub fn ws_stat_path(&self, path: &str) -> Option<String> {
        self.machine
            .workspace_stat_path(path)
            .map(|(dir, size)| serde_json::json!({ "dir": dir, "size": size }).to_string())
    }

    /// `mkdir -p` at a nested path in the shared workspace.
    pub fn ws_mkdir_path(&mut self, path: &str) {
        self.machine.workspace_mkdir_path(path);
    }

    /// Delete a file or directory subtree at a nested path (`unlink`/`rmdir`).
    /// `true` if it existed.
    pub fn ws_delete_path(&mut self, path: &str) -> bool {
        self.machine.workspace_delete_path(path)
    }

    /// Rename a nested path (creating the destination's parents). `true` if the
    /// source existed.
    pub fn ws_rename_path(&mut self, from: &str, to: &str) -> bool {
        self.machine.workspace_rename_path(from, to)
    }

    /// **Apply a configuration** the control plane published (ADR-018; `CC-28`):
    /// decode the κ-addressed [`Configuration`] bytes (resolved + verified over
    /// the substrate by the caller, Law L5) and enact its live directives on the
    /// *running* machine — each `forwardPort` begins forwarding on the running
    /// instance, without a reboot. Returns a JSON summary of what was applied
    /// (`{ "forwarded": [{ "guest": 8080, "host": 8080 }], "lifecycle": "…",
    /// "unsupported": [...] }`). The instance state changes from the panel's
    /// configuration, carried as content over the substrate — no RPC.
    pub fn reconfigure(&mut self, config_bytes: &[u8]) -> Result<String, JsValue> {
        let config = Configuration::from_canonical(config_bytes).map_err(js_err)?;
        let mut forwarded = Vec::new();
        let mut unsupported = Vec::new();
        let mut lifecycle: Option<&str> = None;
        for d in config.directives() {
            match d {
                Directive::ForwardPort(guest) => match self.machine.forward_port(*guest) {
                    Some(host) => {
                        forwarded.push(serde_json::json!({ "guest": guest, "host": host }))
                    }
                    // This peer's forwarded-port transport cannot bind live (the
                    // browser uses a relay route, ADR-016) — reported, not dropped.
                    None => unsupported.push(serde_json::json!({ "forwardPort": guest })),
                },
                Directive::Lifecycle(a) => {
                    lifecycle = Some(match a {
                        LifecycleAction::Start => "start",
                        LifecycleAction::Suspend => "suspend",
                        LifecycleAction::Resume => "resume",
                        LifecycleAction::Terminate => "terminate",
                    });
                }
                // Network/storage authority and account grants change the
                // instance's capability set (its identity, Law L1); the panel
                // records them — they are not live-machine effects.
                other => unsupported.push(serde_json::json!({ "deferred": format!("{other:?}") })),
            }
        }
        Ok(serde_json::json!({
            "forwarded": forwarded,
            "lifecycle": lifecycle,
            "unsupported": unsupported,
        })
        .to_string())
    }
}

/// **The browser peer's AArch64 holospace** — a real arm64 devcontainer booted on
/// the [AArch64 core](holospaces::emulator::aarch64) (`CC-36`), its κ-disk paged
/// from OPFS (the same substrate as the RISC-V [`Workspace`]). The AArch64 core
/// reaches the **shared** `emulator::devbus` for the 9p workspace, the network
/// (router egress), and the in-process guest bridge (`CC-46`) — the same device
/// surface the RISC-V [`Workspace`] exposes, here over the GIC transport.
#[wasm_bindgen]
pub struct Aarch64Workspace {
    cpu: aarch64::Cpu,
    halted: bool,
    console_cursor: usize,
    /// The router seam, present when the guest's egress is carried by an external
    /// router (the extension / a node) over the egress protocol (`CC-46` net
    /// parity). The page pumps it via [`egress_outbound`](Aarch64Workspace::egress_outbound)
    /// / [`egress_inbound`](Aarch64Workspace::egress_inbound).
    router: Option<net::RouterChannel>,
}

#[wasm_bindgen]
impl Aarch64Workspace {
    /// Boot a provisioned arm64 image, **streaming** its κ-disk from OPFS (no full
    /// image in RAM): `rootfs_handle` is the provisioned rootfs (read
    /// sector-by-sector into the OPFS-backed store on `disk_handle`). Drive with
    /// [`run`](Aarch64Workspace::run), rendering
    /// [`terminal_delta`](Aarch64Workspace::terminal_delta) between chunks.
    pub fn boot_devcontainer_opfs_streamed(
        kernel: &[u8],
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        disk_handle: web_sys::FileSystemSyncAccessHandle,
    ) -> Result<Aarch64Workspace, JsValue> {
        let store = Box::new(opfs_store::OpfsKappaStore::new(disk_handle));
        let total = rootfs_handle.get_size().map_err(js_err)? as u64;
        let sector_count = total.div_ceil(512);
        let rootfs = rootfs_handle.clone();
        let read = move |i: u64, buf: &mut [u8]| {
            let opts = web_sys::FileSystemReadWriteOptions::new();
            opts.set_at((i * 512) as f64);
            let _ = rootfs.read_with_u8_array_and_options(buf, &opts);
        };
        let cpu = aarch64::Cpu::boot_linux_disk_streamed(
            512 * 1024 * 1024,
            kernel,
            "console=ttyAMA0 root=/dev/vda rw init=/init",
            store,
            sector_count,
            read,
        );
        // The κ-disk is fully ingested up front; release the rootfs's exclusive OPFS
        // lock so re-provisioning/removal isn't blocked and the handle doesn't leak.
        rootfs_handle.close();
        Ok(Aarch64Workspace {
            cpu,
            halted: false,
            console_cursor: 0,
            router: None,
        })
    }

    /// Boot like [`boot_devcontainer_opfs_streamed`](Aarch64Workspace::boot_devcontainer_opfs_streamed),
    /// additionally attaching the **shared workspace filesystem** (`virtio-9p`,
    /// `CC-15`/`CC-46`), a **router-backed network** (`virtio-net` + the userspace
    /// NAT, carried over the egress protocol — `CC-16`/`CC-46`), and the
    /// **in-process guest bridge** (`CC-33`/`CC-46`). The editor shares files with
    /// the OS ([`workspace_file`](Aarch64Workspace::workspace_file)/[`workspace_write`](Aarch64Workspace::workspace_write)),
    /// the page carries the guest's egress to the router, and the workbench can
    /// [`dial_guest`](Aarch64Workspace::dial_guest) a server inside the
    /// devcontainer — the full shared-devbus surface the RISC-V workspace exposes.
    pub fn boot_devcontainer_opfs_full(
        kernel: &[u8],
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        disk_handle: web_sys::FileSystemSyncAccessHandle,
    ) -> Result<Aarch64Workspace, JsValue> {
        let store = Box::new(opfs_store::OpfsKappaStore::new(disk_handle));
        let total = rootfs_handle.get_size().map_err(js_err)? as u64;
        let sector_count = total.div_ceil(512);
        let rootfs = rootfs_handle.clone();
        let read = move |i: u64, buf: &mut [u8]| {
            let opts = web_sys::FileSystemReadWriteOptions::new();
            opts.set_at((i * 512) as f64);
            let _ = rootfs.read_with_u8_array_and_options(buf, &opts);
        };
        let mut cpu = aarch64::Cpu::boot_linux_disk_streamed(
            512 * 1024 * 1024,
            kernel,
            "console=ttyAMA0 root=/dev/vda rw init=/init ip=dhcp",
            store,
            sector_count,
            read,
        );
        // The κ-disk is fully ingested up front; release the rootfs's exclusive OPFS
        // lock so re-provisioning/removal isn't blocked and the handle doesn't leak.
        rootfs_handle.close();
        // Seed the shared workspace, attach the router-backed network, and enable
        // the in-process bridge — all serviced by the shared devbus over the GIC.
        cpu.attach_workspace(&[]);
        let (egress, router) = net::ChannelEgress::new();
        cpu.attach_net(Box::new(egress));
        cpu.enable_loopback();
        Ok(Aarch64Workspace {
            cpu,
            halted: false,
            console_cursor: 0,
            router: Some(router),
        })
    }

    /// Boot a devcontainer from an **in-RAM** assembled rootfs (`rootfs`) — the
    /// no-provisioning, no-router path a **blank/bundled** holospace takes: the
    /// deploy ships a small `arm64` layer, the page assembles it here, and this
    /// boots it directly with the full device surface (9p workspace + router-
    /// backed net + bridge), identical to the streamed path but sourced from RAM.
    pub fn boot_devcontainer(kernel: &[u8], rootfs: Vec<u8>) -> Result<Aarch64Workspace, JsValue> {
        let mut cpu = aarch64::Cpu::boot_linux_disk(
            512 * 1024 * 1024,
            kernel,
            rootfs,
            "console=ttyAMA0 root=/dev/vda rw init=/init ip=dhcp",
        );
        cpu.attach_workspace(&[]);
        let (egress, router) = net::ChannelEgress::new();
        cpu.attach_net(Box::new(egress));
        cpu.enable_loopback();
        Ok(Aarch64Workspace {
            cpu,
            halted: false,
            console_cursor: 0,
            router: Some(router),
        })
    }

    /// Run a chunk of guest execution; returns `true` once the machine halts.
    pub fn run(&mut self, budget: f64) -> bool {
        if self.halted {
            return true;
        }
        if !matches!(self.cpu.run(budget as u64), aarch64::Halt::OutOfBudget) {
            self.halted = true;
        }
        self.halted
    }

    /// The full console the guest has produced.
    #[must_use]
    pub fn terminal(&self) -> String {
        String::from_utf8_lossy(self.cpu.console()).into_owned()
    }

    /// The console bytes produced since the last call (the integrated terminal
    /// streams these).
    pub fn terminal_delta(&mut self) -> Vec<u8> {
        let console = self.cpu.console();
        let from = self.console_cursor.min(console.len());
        let delta = console[from..].to_vec();
        self.console_cursor = console.len();
        delta
    }

    /// Feed keystrokes to the guest's serial console.
    pub fn feed_input(&mut self, bytes: &[u8]) {
        self.cpu.feed_console(bytes);
    }

    /// Whether the machine has powered off.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn halted(&self) -> bool {
        self.halted
    }

    // ── the shared-devbus surface (CC-46): 9p workspace, net, bridge ─────────

    /// Drain the next egress frame the guest produced, for the page to carry to
    /// the router (`CC-46` net parity). `undefined` when none is queued (or this
    /// is not a `*_full` boot).
    #[must_use]
    pub fn egress_outbound(&self) -> Option<Vec<u8>> {
        self.router
            .as_ref()
            .and_then(net::RouterChannel::pop_outbound)
    }

    /// Deliver an egress frame the router returned into the guest's network. A
    /// no-op when this is not a `*_full` boot.
    pub fn egress_inbound(&self, frame: &[u8]) {
        if let Some(r) = &self.router {
            r.feed_inbound(frame);
        }
    }

    /// Read a file from the shared workspace — how the editor observes the OS's
    /// edits over `virtio-9p` (`CC-15`/`CC-46`). `undefined` if absent / no 9p.
    #[must_use]
    pub fn workspace_file(&self, name: &str) -> Option<Vec<u8>> {
        self.cpu.workspace_file(name).map(<[u8]>::to_vec)
    }

    /// Write a file into the shared workspace — the editor saving content the OS
    /// reads over `virtio-9p` (one content, Law L1; `CC-15`/`CC-46`).
    pub fn workspace_write(&mut self, name: &str, data: &[u8]) {
        self.cpu.workspace_write(name, data);
    }

    // ── the workbench FileSystemProvider surface (CC-15/CC-17/CC-51 parity) ──
    // The SAME method names the riscv64 `Workspace` exposes, so `holospace-fs`
    // binds the editor to any core purely by capability detection — one
    // FileSystemProvider, three ISAs, no per-arch branches (Law L4).

    /// The shared workspace's directory listing — a JSON array of
    /// `{ name, dir, size }` (the workbench `FileSystemProvider.readDirectory`).
    #[must_use]
    pub fn ws_list(&self) -> String {
        let entries: Vec<serde_json::Value> = self
            .cpu
            .workspace_list()
            .into_iter()
            .map(|(name, dir, size)| serde_json::json!({ "name": name, "dir": dir, "size": size }))
            .collect();
        serde_json::Value::Array(entries).to_string()
    }

    /// Read a file from the shared workspace. `undefined` if absent.
    #[must_use]
    pub fn ws_read(&self, name: &str) -> Option<Vec<u8>> {
        self.cpu.workspace_file(name).map(<[u8]>::to_vec)
    }

    /// Write a file into the shared workspace. Returns the content's κ (Law L1/L2).
    pub fn ws_write(&mut self, name: &str, content: &[u8]) -> String {
        self.cpu.workspace_write(name, content);
        address(content).as_str().to_owned()
    }

    /// Delete a file or folder from the shared workspace. `true` if it existed.
    pub fn ws_delete(&mut self, name: &str) -> bool {
        self.cpu.workspace_delete(name)
    }

    /// Rename a file or folder in the shared workspace. `true` if the source existed.
    pub fn ws_rename(&mut self, from: &str, to: &str) -> bool {
        self.cpu.workspace_rename(from, to)
    }

    /// Create a folder in the shared workspace.
    pub fn ws_mkdir(&mut self, name: &str) {
        self.cpu.workspace_mkdir(name);
    }

    /// Read a file by nested path (e.g. `.vscode/tasks.json`). `undefined` if
    /// absent or a directory.
    #[must_use]
    pub fn ws_read_path(&self, path: &str) -> Option<Vec<u8>> {
        self.cpu.workspace_file_path(path).map(<[u8]>::to_vec)
    }

    /// Write a file at a nested path, creating parent directories. Returns the
    /// content's κ (its identity).
    pub fn ws_write_path(&mut self, path: &str, content: &[u8]) -> String {
        self.cpu.workspace_write_path(path, content);
        address(content).as_str().to_owned()
    }

    /// List a directory by nested path — a JSON array `[{name,dir,size}]`, or
    /// `null` if the path is absent or not a directory.
    #[must_use]
    pub fn ws_list_path(&self, path: &str) -> Option<String> {
        self.cpu.workspace_list_path(path).map(|entries| {
            let arr: Vec<serde_json::Value> = entries
                .into_iter()
                .map(|(name, dir, size)| serde_json::json!({ "name": name, "dir": dir, "size": size }))
                .collect();
            serde_json::Value::Array(arr).to_string()
        })
    }

    /// Stat a nested path — a JSON object `{dir,size}`, or `null` if absent.
    #[must_use]
    pub fn ws_stat_path(&self, path: &str) -> Option<String> {
        self.cpu
            .workspace_stat_path(path)
            .map(|(dir, size)| serde_json::json!({ "dir": dir, "size": size }).to_string())
    }

    /// `mkdir -p` at a nested path in the shared workspace.
    pub fn ws_mkdir_path(&mut self, path: &str) {
        self.cpu.workspace_mkdir_path(path);
    }

    /// Delete a file or folder (recursively) at a nested path. `true` if it existed.
    pub fn ws_delete_path(&mut self, path: &str) -> bool {
        self.cpu.workspace_delete_path(path)
    }

    /// Rename/move a file or folder at a nested path. `true` if the source existed.
    pub fn ws_rename_path(&mut self, from: &str, to: &str) -> bool {
        self.cpu.workspace_rename_path(from, to)
    }

    /// Dial an in-process connection to a server inside the devcontainer over the
    /// loopback bridge (`CC-33`/`CC-46`). `None` if not a `*_full` boot.
    pub fn dial_guest(&mut self, guest_port: u16) -> Option<u32> {
        self.cpu.dial_guest(guest_port)
    }

    /// Write bytes toward the guest server on a loopback connection (`CC-33`).
    pub fn guest_send(&mut self, id: u32, data: &[u8]) {
        self.cpu.guest_send(id, data);
    }

    /// Drain the guest server's reply bytes on a loopback connection (`CC-33`).
    pub fn guest_recv(&mut self, id: u32) -> Vec<u8> {
        self.cpu.guest_recv(id)
    }

    /// Close the host side of a loopback connection (`CC-33`).
    pub fn guest_close(&mut self, id: u32) {
        self.cpu.guest_close(id);
    }

    /// Whether a loopback connection is still usable (`CC-33`).
    #[must_use]
    pub fn guest_is_open(&self, id: u32) -> bool {
        self.cpu.guest_is_open(id)
    }
}

/// A booted **x86-64** (amd64) devcontainer on the holospaces x64 core
/// (`CC-43`/`CC-44`/`CC-45`) — the ubiquitous registry/Codespaces architecture,
/// so a launched x64 holospace runs the ecosystem's stock `linux/amd64` images and
/// their x64 extensions. The browser-peer analogue of [`Aarch64Workspace`]: the
/// provisioned amd64 image is paged from OPFS (`CC-7`, no full image in RAM) and
/// the page drives the integrated terminal. Selected from the Platform Manager's
/// architecture picker (ADR-021; the arch is fixed at provisioning, part of the
/// holospace's content-addressed identity, Law L1).
#[wasm_bindgen]
pub struct X64Workspace {
    cpu: x64::Cpu,
    halted: bool,
    console_cursor: usize,
    /// The router seam, present when the guest's egress is carried by an external
    /// router (the extension / a node) over the egress protocol (net parity with
    /// the other cores). The page pumps it via
    /// [`egress_outbound`](X64Workspace::egress_outbound) /
    /// [`egress_inbound`](X64Workspace::egress_inbound).
    router: Option<net::RouterChannel>,
}

#[wasm_bindgen]
impl X64Workspace {
    /// Boot a provisioned amd64 image, **streaming** its κ-disk from OPFS (no full
    /// image in RAM): `rootfs_handle` is the provisioned rootfs (read
    /// sector-by-sector into the OPFS-backed store on `disk_handle`). Drive with
    /// [`run`](X64Workspace::run), rendering [`terminal_delta`](X64Workspace::terminal_delta)
    /// between chunks. The x64 analogue of
    /// [`Aarch64Workspace::boot_devcontainer_opfs_streamed`].
    pub fn boot_devcontainer_opfs_streamed(
        kernel: &[u8],
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        disk_handle: web_sys::FileSystemSyncAccessHandle,
    ) -> Result<X64Workspace, JsValue> {
        let store = Box::new(opfs_store::OpfsKappaStore::new(disk_handle));
        let total = rootfs_handle.get_size().map_err(js_err)? as u64;
        let sector_count = total.div_ceil(512);
        let rootfs = rootfs_handle.clone();
        let read = move |i: u64, buf: &mut [u8]| {
            let opts = web_sys::FileSystemReadWriteOptions::new();
            opts.set_at((i * 512) as f64);
            let _ = rootfs.read_with_u8_array_and_options(buf, &opts);
        };
        // x86-64 has no device tree, so the virtio-mmio κ-disk is discovered from
        // the command line (`virtio_mmio.device=<size>@<base>:<irq>` — the κ-disk's
        // MMIO slot at 0xd000_0000, IRQ 11), exactly as the CC-45 boot witness. The
        // aarch64 peer discovers the same device from its DTB instead, hence the
        // shorter cmdline there. `random.trust_cpu=on` lets the RNG seed from RDRAND
        // so early userspace doesn't block on entropy.
        let mut cpu = x64::Cpu::boot_linux_disk_streamed(
            512 * 1024 * 1024,
            kernel,
            "console=ttyS0 root=/dev/vda rw init=/init ip=dhcp \
             virtio_mmio.device=0x200@0xd0000000:11 \
             virtio_mmio.device=0x200@0xd0000200:10 \
             virtio_mmio.device=0x200@0xd0000400:12 random.trust_cpu=on",
            store,
            sector_count,
            read,
        );
        // The κ-disk is fully ingested up front; release the rootfs's exclusive OPFS
        // lock so re-provisioning/removal isn't blocked and the handle doesn't leak.
        rootfs_handle.close();
        // Net parity with the other cores: the router-backed egress (the page
        // pumps frames to the extension / an egress node) + the in-process
        // loopback bridge (CC-33) for reaching guest servers from the tab.
        let (egress, router) = net::ChannelEgress::new();
        cpu.attach_net(Box::new(egress));
        cpu.enable_loopback();
        Ok(X64Workspace {
            cpu,
            halted: false,
            console_cursor: 0,
            router: Some(router),
        })
    }

    /// Boot a provisioned amd64 image **O(content)** by occupancy — the deployed path
    /// for an arbitrarily large, build-capable devcontainer disk. `occupancy_handle`
    /// is the sidecar [`assembleBootableIntoOpfsTracked`](DevcontainerImage::assemble_bootable_into_opfs_tracked)
    /// wrote (packed little-endian `u64` block indices); only those blocks are read
    /// from `rootfs_handle` (each a 4 KiB block = 8 sectors), so a multi-GiB declared
    /// disk pages in proportion to its **content**, never reading its holes. The
    /// streamed-from-OPFS analogue of [`x64::Cpu::boot_linux_disk_occupancy_streamed`].
    #[wasm_bindgen(js_name = bootDevcontainerOpfsStreamedOccupancy)]
    pub fn boot_devcontainer_opfs_streamed_occupancy(
        kernel: &[u8],
        rootfs_handle: web_sys::FileSystemSyncAccessHandle,
        occupancy_handle: web_sys::FileSystemSyncAccessHandle,
        disk_handle: web_sys::FileSystemSyncAccessHandle,
    ) -> Result<X64Workspace, JsValue> {
        let store = Box::new(opfs_store::OpfsKappaStore::new(disk_handle));

        // The sidecar (written by assembleIntoOpfsTracked): an 8-byte little-endian
        // image_len header — the DECLARED disk the guest sees, which the rootfs file
        // (compact: only the packed content blocks) does NOT span — then each packed
        // block's disk index, in packed order.
        let occ_len = occupancy_handle.get_size().map_err(js_err)? as usize;
        let mut occ_bytes = vec![0u8; occ_len];
        let opts = web_sys::FileSystemReadWriteOptions::new();
        opts.set_at(0.0);
        occupancy_handle
            .read_with_u8_array_and_options(&mut occ_bytes, &opts)
            .map_err(js_err)?;
        occupancy_handle.close();
        if occ_bytes.len() < 8 {
            return Err(JsValue::from_str("occupancy sidecar is missing its header"));
        }
        let image_len = u64::from_le_bytes(occ_bytes[0..8].try_into().unwrap());
        // The block device must span the declared IMAGE, not the compact file.
        let sector_count = image_len / 512;
        // VALIDATE the sidecar before trusting it: the payload after the header must be
        // a whole number of 8-byte block indices, else `chunks_exact` would silently
        // drop a truncated tail and desynchronize packed slots from block indices.
        let payload = &occ_bytes[8..];
        if !payload.len().is_multiple_of(8) {
            return Err(JsValue::from_str(
                "occupancy sidecar payload is not a whole number of 8-byte block indices (corrupt/truncated)",
            ));
        }
        let occupied_blocks: Vec<u64> = payload
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        // Every named block must lie within the declared disk (a stray index would map
        // to a sector outside the device).
        let block_count = image_len.div_ceil(BLOCK_BYTES);
        if occupied_blocks.iter().any(|&b| b >= block_count) {
            return Err(JsValue::from_str(
                "occupancy sidecar names a block outside the declared disk",
            ));
        }
        // The compact rootfs file holds exactly one 4 KiB slot per occupied block; if it
        // is shorter, a packed-slot read would run off the end and corrupt the boot.
        let rootfs_size = rootfs_handle.get_size().map_err(js_err)? as u64;
        let expected = occupied_blocks.len() as u64 * BLOCK_BYTES;
        if rootfs_size < expected {
            return Err(JsValue::from_str(
                "compact rootfs file is smaller than the occupancy index implies (truncated)",
            ));
        }

        // The rootfs file is COMPACT — the Nth occupied block at file offset N·4096 —
        // so read it sequentially: from_occupancy_streamed visits the occupied blocks
        // in order, and the read for the Nth block is the Nth packed slot. The disk
        // sector it carries is `occupied_blocks[N]·8` (the core records that).
        let rootfs = rootfs_handle.clone();
        let mut packed_slot: u64 = 0;
        let read = move |_disk_sector: u64, buf: &mut [u8]| {
            let opts = web_sys::FileSystemReadWriteOptions::new();
            opts.set_at((packed_slot * BLOCK_BYTES) as f64);
            let _ = rootfs.read_with_u8_array_and_options(buf, &opts);
            packed_slot += 1;
        };
        // 4 KiB ext4 blocks over 512-byte disk sectors → 8 sectors per block. Same
        // cmdline as the streamed boot (x86-64 discovers the κ-disk from the cmdline,
        // having no device tree).
        const SECTORS_PER_BLOCK: u64 = 8;
        let mut cpu = x64::Cpu::boot_linux_disk_occupancy_streamed(
            512 * 1024 * 1024,
            kernel,
            "console=ttyS0 root=/dev/vda rw init=/init ip=dhcp \
             virtio_mmio.device=0x200@0xd0000000:11 \
             virtio_mmio.device=0x200@0xd0000200:10 \
             virtio_mmio.device=0x200@0xd0000400:12 random.trust_cpu=on",
            store,
            sector_count,
            &occupied_blocks,
            SECTORS_PER_BLOCK,
            read,
        );
        rootfs_handle.close();
        // Net parity with the other cores: the router-backed egress (the page
        // pumps frames to the extension / an egress node) + the in-process
        // loopback bridge (CC-33) for reaching guest servers from the tab.
        let (egress, router) = net::ChannelEgress::new();
        cpu.attach_net(Box::new(egress));
        cpu.enable_loopback();
        Ok(X64Workspace {
            cpu,
            halted: false,
            console_cursor: 0,
            router: Some(router),
        })
    }

    /// Boot a devcontainer from an **in-RAM** assembled rootfs (`rootfs`) — the
    /// no-provisioning, no-router path a **blank/bundled** holospace takes: the
    /// deploy ships a small `amd64` layer, the page assembles it here, and this
    /// boots it directly with the full device surface (9p workspace + router-
    /// backed net + bridge), identical to the paged path but sourced from RAM.
    pub fn boot_devcontainer(kernel: &[u8], rootfs: Vec<u8>) -> Result<X64Workspace, JsValue> {
        let mut cpu = x64::Cpu::boot_linux_disk(
            512 * 1024 * 1024,
            kernel,
            rootfs,
            "console=ttyS0 root=/dev/vda rw init=/init ip=dhcp \
             virtio_mmio.device=0x200@0xd0000000:11 \
             virtio_mmio.device=0x200@0xd0000200:10 \
             virtio_mmio.device=0x200@0xd0000400:12 random.trust_cpu=on",
        );
        let (egress, router) = net::ChannelEgress::new();
        cpu.attach_net(Box::new(egress));
        cpu.enable_loopback();
        Ok(X64Workspace {
            cpu,
            halted: false,
            console_cursor: 0,
            router: Some(router),
        })
    }

    /// Run a chunk of guest execution; returns `true` once the machine halts.
    pub fn run(&mut self, budget: f64) -> bool {
        if self.halted {
            return true;
        }
        if !matches!(self.cpu.run(budget as u64), x64::Halt::OutOfBudget) {
            self.halted = true;
        }
        self.halted
    }

    /// The full console the guest has produced.
    #[must_use]
    pub fn terminal(&self) -> String {
        String::from_utf8_lossy(self.cpu.console()).into_owned()
    }

    /// The console bytes produced since the last call (the integrated terminal
    /// streams these).
    pub fn terminal_delta(&mut self) -> Vec<u8> {
        let console = self.cpu.console();
        let from = self.console_cursor.min(console.len());
        let delta = console[from..].to_vec();
        self.console_cursor = console.len();
        delta
    }

    /// Feed keystrokes to the guest's serial console.
    pub fn feed_input(&mut self, bytes: &[u8]) {
        self.cpu.feed_console(bytes);
    }

    // ── the router-backed network + the in-process bridge (net parity) ──────

    /// Drain the next egress frame the guest produced, for the page to carry to
    /// the router. `undefined` when none is queued.
    #[must_use]
    pub fn egress_outbound(&self) -> Option<Vec<u8>> {
        self.router
            .as_ref()
            .and_then(net::RouterChannel::pop_outbound)
    }

    /// Deliver an egress frame the router returned into the guest's network.
    pub fn egress_inbound(&self, frame: &[u8]) {
        if let Some(r) = &self.router {
            r.feed_inbound(frame);
        }
    }

    /// Dial an in-process connection to a server inside the devcontainer over
    /// the loopback bridge (`CC-33` parity). `None` if the bridge is off.
    pub fn dial_guest(&mut self, guest_port: u16) -> Option<u32> {
        self.cpu.dial_guest(guest_port)
    }

    /// Send bytes on a dialed in-process connection.
    pub fn guest_send(&mut self, id: u32, data: &[u8]) {
        self.cpu.guest_send(id, data);
    }

    /// Receive any bytes the guest server produced on a dialed connection.
    #[must_use]
    pub fn guest_recv(&mut self, id: u32) -> Vec<u8> {
        self.cpu.guest_recv(id)
    }

    /// Close a dialed in-process connection.
    pub fn guest_close(&mut self, id: u32) {
        self.cpu.guest_close(id);
    }

    /// Whether a dialed in-process connection is open.
    #[must_use]
    pub fn guest_is_open(&self, id: u32) -> bool {
        self.cpu.guest_is_open(id)
    }

    // ── the workbench FileSystemProvider surface (CC-15/CC-17/CC-51 parity) ──
    // The SAME method names the riscv64 `Workspace` exposes, so `holospace-fs`
    // binds the editor to any core purely by capability detection — one
    // FileSystemProvider, three ISAs, no per-arch branches (Law L4).

    /// The shared workspace's directory listing — a JSON array of
    /// `{ name, dir, size }` (the workbench `FileSystemProvider.readDirectory`).
    #[must_use]
    pub fn ws_list(&self) -> String {
        let entries: Vec<serde_json::Value> = self
            .cpu
            .workspace_list()
            .into_iter()
            .map(|(name, dir, size)| serde_json::json!({ "name": name, "dir": dir, "size": size }))
            .collect();
        serde_json::Value::Array(entries).to_string()
    }

    /// Read a file from the shared workspace. `undefined` if absent.
    #[must_use]
    pub fn ws_read(&self, name: &str) -> Option<Vec<u8>> {
        self.cpu.workspace_file(name).map(<[u8]>::to_vec)
    }

    /// Write a file into the shared workspace. Returns the content's κ (Law L1/L2).
    pub fn ws_write(&mut self, name: &str, content: &[u8]) -> String {
        self.cpu.workspace_write(name, content);
        address(content).as_str().to_owned()
    }

    /// Delete a file or folder from the shared workspace. `true` if it existed.
    pub fn ws_delete(&mut self, name: &str) -> bool {
        self.cpu.workspace_delete(name)
    }

    /// Rename a file or folder in the shared workspace. `true` if the source existed.
    pub fn ws_rename(&mut self, from: &str, to: &str) -> bool {
        self.cpu.workspace_rename(from, to)
    }

    /// Create a folder in the shared workspace.
    pub fn ws_mkdir(&mut self, name: &str) {
        self.cpu.workspace_mkdir(name);
    }

    /// Read a file by nested path (e.g. `.vscode/tasks.json`). `undefined` if
    /// absent or a directory.
    #[must_use]
    pub fn ws_read_path(&self, path: &str) -> Option<Vec<u8>> {
        self.cpu.workspace_file_path(path).map(<[u8]>::to_vec)
    }

    /// Write a file at a nested path, creating parent directories. Returns the
    /// content's κ (its identity).
    pub fn ws_write_path(&mut self, path: &str, content: &[u8]) -> String {
        self.cpu.workspace_write_path(path, content);
        address(content).as_str().to_owned()
    }

    /// List a directory by nested path — a JSON array `[{name,dir,size}]`, or
    /// `null` if the path is absent or not a directory.
    #[must_use]
    pub fn ws_list_path(&self, path: &str) -> Option<String> {
        self.cpu.workspace_list_path(path).map(|entries| {
            let arr: Vec<serde_json::Value> = entries
                .into_iter()
                .map(|(name, dir, size)| serde_json::json!({ "name": name, "dir": dir, "size": size }))
                .collect();
            serde_json::Value::Array(arr).to_string()
        })
    }

    /// Stat a nested path — a JSON object `{dir,size}`, or `null` if absent.
    #[must_use]
    pub fn ws_stat_path(&self, path: &str) -> Option<String> {
        self.cpu
            .workspace_stat_path(path)
            .map(|(dir, size)| serde_json::json!({ "dir": dir, "size": size }).to_string())
    }

    /// `mkdir -p` at a nested path in the shared workspace.
    pub fn ws_mkdir_path(&mut self, path: &str) {
        self.cpu.workspace_mkdir_path(path);
    }

    /// Delete a file or folder (recursively) at a nested path. `true` if it existed.
    pub fn ws_delete_path(&mut self, path: &str) -> bool {
        self.cpu.workspace_delete_path(path)
    }

    /// Rename/move a file or folder at a nested path. `true` if the source existed.
    pub fn ws_rename_path(&mut self, from: &str, to: &str) -> bool {
        self.cpu.workspace_rename_path(from, to)
    }

    /// Whether the machine has powered off.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn halted(&self) -> bool {
        self.halted
    }
}
