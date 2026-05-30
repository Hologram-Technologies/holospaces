//! **The Hologram Platform Manager** — holospaces' browser peer.
//!
//! Realizes the *Platform Manager* (arc42 chapter 5,
//! `docs/src/arc42/adoc/05_building_block_view.adoc`) and the *Browser* peer
//! (arc42 chapter 7): the operator console, delivered as wasm and **served from
//! GitHub Pages** (arc42 chapter 3 / chapter 6 cold-start). Loading the
//! κ-addressed bundle makes the browser a peer that *is* the substrate — there
//! is no server (Law L1).
//!
//! This crate is the wasm-bindgen surface over holospaces' Manager model: it
//! composes a browser peer (an in-memory `KappaStore` — RAM as a cache, Law L3)
//! and exposes the console operations — sign in, provision (both compute forms:
//! a `.holo` artifact and a Wasm-recompiled userland over the execution
//! surface, ADR-008), view, resolve (verify by re-derivation, Law L5), the
//! operator roster (R5), and the browser `.holo` engine (arc42 chapter 11, RT2,
//! realized — `CC-2`). In-browser *container* boot needs a browser
//! `ContainerEngine` (not yet in the substrate) and remains future work; the
//! browser peer provisions, addresses, resolves, and verifies all forms today.

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{Capabilities, KappaStore, Realization};
use holospaces::boot::{provision, Resolver};
use holospaces::identity::{Operator, Roster};
use holospaces::realizations::{address, verify, Kappa, Source};
use holospaces::surface;
use wasm_bindgen::prelude::*;

fn js_err<E: core::fmt::Debug>(e: E) -> JsValue {
    JsValue::from_str(&format!("{e:?}"))
}

fn parse_kappa(kappa: &str) -> Result<Kappa, JsValue> {
    Kappa::from_bytes(kappa.as_bytes()).map_err(|_| JsValue::from_str("not a well-formed κ-label"))
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

/// The Platform Manager console, running as a browser peer.
#[wasm_bindgen]
pub struct Console {
    store: MemKappaStore,
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
    /// Open a fresh console (a browser peer with a local content-addressed
    /// store).
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Console {
        Console {
            store: MemKappaStore::new(),
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
        let artifact = self.store.put("blake3", code).map_err(js_err)?;
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
        let entry = self.store.put("blake3", module).map_err(js_err)?;
        self.provision_source(Source::Userland { entry }, memory_bytes)
    }

    fn provision_source(&mut self, source: Source, memory_bytes: f64) -> Result<String, JsValue> {
        let capabilities = Capabilities {
            storage_roots: Vec::new(),
            storage_quota_bytes: 0,
            network_fetch: false,
            network_announce: false,
            publish_channels: Vec::new(),
            subscribe_channels: Vec::new(),
            memory_max_bytes: memory_bytes as u64,
            cpu_time_per_event_ms: 0,
            priority_weight: 0,
        };
        let holospace = provision(&self.store, source, capabilities).map_err(js_err)?;
        let kappa = holospace.kappa();
        if !self.holospaces.contains(&kappa) {
            self.holospaces.push(kappa);
        }
        self.persist_roster()?;
        Ok(kappa.as_str().to_owned())
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
        Resolver::resolve_local(&self.store, &kappa)
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
            self.store
                .put("blake3", &roster.canonicalize())
                .map_err(js_err)?;
        }
        Ok(())
    }
}
