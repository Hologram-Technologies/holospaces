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
//! `CC-6`), **and importing and running a devcontainer in the browser** — the
//! Codespaces/Gitpod scenario with no Docker daemon and no cloud VM (arc42
//! chapter 1, the motivating scenario). The same holospace κ boots on this
//! browser peer as on a native or remote one (Q6).

use hologram_runtime::Runtime;
use hologram_runtime_bare::BareMetalEngine;
use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{Capabilities, KappaStore, Realization};
use holospaces::boot::{devcontainer, provision, LifecycleError, Resolver, Session};
use holospaces::identity::{Operator, Roster};
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
        let holospace =
            provision(self.runtime.store(), Source::Userland { entry }, capabilities(memory_bytes))
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
