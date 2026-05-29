//! **The `.holo` Engine** — runs a `.holo` compute artifact.
//!
//! Realizes the *.holo Engine* building block (arc42 chapter 5,
//! `docs/src/arc42/adoc/05_building_block_view.adoc`) and the cross-cutting
//! concept *Two compute forms* (arc42 chapter 8): a `.holo` (tensor) artifact
//! is run by the [hologram](https://github.com/Hologram-Technologies/hologram)
//! executor. holospaces supplies this execution backend (arc42 chapter 7); it
//! does not re-implement the executor — it binds to `hologram-exec`.
//!
//! Execution is deterministic and content-addressed: identical `.holo` + inputs
//! yield identical output bytes, hence identical output κ-labels under the
//! substrate's σ-axis (Conformance `CC-2`).

use hologram_backend::CpuBackend;
use hologram_exec::{BufferArena, InferenceSession, InputBuffer};

use crate::realizations::{address, Kappa};
#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::{
    borrow::ToOwned,
    boxed::Box,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

/// The `.holo` execution backend: loads a `.holo` archive and runs it through
/// the hologram executor, content-addressing the outputs.
pub struct HoloEngine;

impl HoloEngine {
    /// Run a `.holo` archive over `inputs`, returning the κ-label of each output
    /// (the substrate's blake3 σ-axis). The output κ is reproducible from the
    /// `.holo` and its inputs (`CC-2`).
    ///
    /// # Errors
    ///
    /// [`EngineError::Load`] if the archive is not a loadable `.holo`;
    /// [`EngineError::Execute`] if execution fails.
    pub fn run(archive: &[u8], inputs: &[&[u8]]) -> Result<Vec<Kappa>, EngineError> {
        let backend = CpuBackend::<BufferArena>::new();
        let mut session = InferenceSession::load(archive, backend)
            .map_err(|e| EngineError::Load(format!("{e:?}")))?;
        let buffers: Vec<InputBuffer> = inputs.iter().map(|b| InputBuffer { bytes: b }).collect();
        let outputs = session
            .execute(&buffers)
            .map_err(|e| EngineError::Execute(format!("{e:?}")))?;
        Ok(outputs.iter().map(|o| address(&o.bytes)).collect())
    }
}

/// Why running a `.holo` failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The archive could not be loaded as a `.holo`.
    Load(String),
    /// Executing the `.holo` failed.
    Execute(String),
}

impl core::fmt::Display for EngineError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EngineError::Load(e) => write!(f, "could not load .holo archive: {e}"),
            EngineError::Execute(e) => write!(f, "could not execute .holo: {e}"),
        }
    }
}

impl core::error::Error for EngineError {}
