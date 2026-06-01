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

mod wsnet;

use hologram_runtime::Runtime;
use hologram_runtime_bare::BareMetalEngine;
use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{Capabilities, KappaStore, Realization};
use holospaces::assembly::{assemble_ext4, Layer};
use holospaces::boot::{devcontainer, provision, LifecycleError, Resolver, Session};
use holospaces::emulator::{Emulator, Halt};
use holospaces::identity::{Operator, Roster};
use holospaces::machine::MachineSpec;
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
}

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
        Console {
            runtime: Runtime::new(BareMetalEngine::new(), MemKappaStore::new()),
            operator: None,
            holospaces: Vec::new(),
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
        memory_bytes: f64,
    ) -> Result<String, JsValue> {
        devcontainer::parse(config_json).map_err(js_err)?;
        let artifact = self
            .runtime
            .store()
            .put("blake3", config_json)
            .map_err(js_err)?;
        self.provision_source(Source::HoloFile { artifact }, memory_bytes)
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
    pub fn run_devcontainer(
        &mut self,
        repo: &str,
        reference: &str,
        config_path: &str,
        config_json: &[u8],
        userland_module: &[u8],
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

    /// Resolve a holospace (or any κ) from the local store, verifying it by
    /// re-derivation (Law L5). Returns the bytes, or `undefined` if absent.
    pub fn resolve(&self, kappa: &str) -> Result<Option<Vec<u8>>, JsValue> {
        let kappa = parse_kappa(kappa)?;
        Resolver::resolve_local(self.runtime.store(), &kappa)
            .map(|opt| opt.map(|b| b.to_vec()))
            .map_err(js_err)
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
        })
    }

    /// Boot a **devcontainer** workspace: the Boot Orchestrator
    /// ([`MachineSpec`](holospaces::machine::MachineSpec)) generates the device
    /// tree and boots `kernel` on a machine whose `virtio-blk` disk is the
    /// assembled `rootfs` (from [`DevcontainerImage::assemble`]). The guest
    /// kernel mounts the rootfs over `/dev/vda` and runs the devcontainer's real
    /// OS — entirely in the browser peer (`CC-14`). Drive it with
    /// [`run`](Workspace::run), exactly like [`boot`](Workspace::boot).
    pub fn boot_devcontainer(kernel: &[u8], rootfs: &[u8]) -> Result<Workspace, JsValue> {
        let machine = MachineSpec::devcontainer()
            .boot(kernel, rootfs.to_vec())
            .map_err(js_err)?;
        Ok(Workspace {
            machine,
            store: MemKappaStore::new(),
            channel: Vec::new(),
            files: std::collections::BTreeMap::new(),
            halted: false,
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
        let event = {
            let mut projection = Projection::attach(&mut self.machine);
            projection.type_line(line, 400_000_000)
        };
        self.channel.push(event);
        // The `exit` line powers the machine off; reflect that in the workspace.
        if self.shows("WORKSPACE-DONE") {
            self.halted = true;
        }
        event.as_str().to_owned()
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
        self.channel.push(intent.kappa());
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
}
