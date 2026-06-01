//! **Personalization** — the operator's settings, dotfiles, and secrets, carried
//! as content scoped to their identity (`CC-23`).
//!
//! A Codespace/Gitpod carries an operator's *personalization* — their editor
//! settings, their dotfiles, and their secrets — so their environment follows
//! them into any workspace. holospaces realizes this *without a server account*:
//! a [`Personalization`] is a hologram [`Realization`] — IRI-tagged canonical
//! bytes that **embed the operator identity** ([`crate::identity::Operator`], the
//! κ of their self-sovereign key) and carry the settings/dotfiles/secrets as
//! payload. The whole personalization is therefore itself content — a [`Kappa`]
//! scoped to the operator (the same content under a *different* operator is a
//! *different* κ), held in the store and synced by the substrate's `KappaSync`,
//! not by any host (Laws L1/L3; ADR-001).
//!
//! On entry holospaces *applies* it: the dotfiles are injected into the
//! devcontainer OS's home directory and the secrets are exported into its
//! environment by an entry `/init`
//! ([`assemble_ext4_with_files`](crate::assembly::assemble_ext4_with_files) +
//! [`Personalization::entry_init`]), and the editor settings are handed to the
//! workbench ([`Personalization::workbench_settings`]) — so the operator's
//! environment is ready on entry, on whatever peer they signed in to.

use hologram_substrate_core::{Realization, RealizationError, References};

use crate::identity::Operator;
use crate::realizations::{address, encode, extract_refs, Kappa};

use alloc::collections::BTreeMap;
#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::{
    borrow::ToOwned,
    format,
    string::{String, ToString},
    vec::Vec,
};

/// An operator's personalization: editor `settings`, `dotfiles` (name → bytes,
/// e.g. `.gitconfig`), and `secrets` (environment name → value), scoped to the
/// operator identity it is constructed for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Personalization {
    operator: Kappa,
    settings: Option<String>,
    dotfiles: BTreeMap<String, Vec<u8>>,
    secrets: BTreeMap<String, String>,
}

impl Personalization {
    /// The holospaces realization IRI for an operator's personalization.
    pub const IRI: &'static str = "https://uor.foundation/holospaces/realization/personalization";

    /// An empty personalization scoped to `operator`.
    #[must_use]
    pub fn new(operator: &Operator) -> Self {
        Self {
            operator: *operator.identity(),
            settings: None,
            dotfiles: BTreeMap::new(),
            secrets: BTreeMap::new(),
        }
    }

    /// Set the operator's editor settings (a workbench `settings.json`).
    #[must_use]
    pub fn with_settings(mut self, settings_json: impl Into<String>) -> Self {
        self.settings = Some(settings_json.into());
        self
    }

    /// Add a dotfile (`name` → `content`), e.g. `.gitconfig` → its bytes.
    #[must_use]
    pub fn with_dotfile(mut self, name: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        self.dotfiles.insert(name.into(), content.into());
        self
    }

    /// Add a secret (`name` → `value`), exposed to the OS as an environment
    /// variable (as a Codespace exposes its secrets).
    #[must_use]
    pub fn with_secret(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets.insert(name.into(), value.into());
        self
    }

    /// The operator identity this personalization is scoped to.
    #[must_use]
    pub fn operator(&self) -> &Kappa {
        &self.operator
    }

    /// The editor settings, if any — handed to the workbench on entry.
    #[must_use]
    pub fn workbench_settings(&self) -> Option<&str> {
        self.settings.as_deref()
    }

    /// The dotfiles (name → content).
    #[must_use]
    pub fn dotfiles(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.dotfiles
    }

    /// The secrets (name → value).
    #[must_use]
    pub fn secrets(&self) -> &BTreeMap<String, String> {
        &self.secrets
    }

    /// The personalization's κ — its content address, scoped to the operator
    /// (Law L1). The same content under a different operator yields a different κ.
    #[must_use]
    pub fn kappa(&self) -> Kappa {
        address(&self.canonicalize())
    }

    /// The dotfiles as files to inject into the devcontainer OS's home directory
    /// (`/root/<name>`) — the home of `root`, PID 1's user. Paths are relative to
    /// the rootfs (no leading `/`), ready for
    /// [`assemble_ext4_with_files`](crate::assembly::assemble_ext4_with_files).
    #[must_use]
    pub fn home_files(&self) -> Vec<(String, Vec<u8>)> {
        self.dotfiles
            .iter()
            .map(|(name, content)| (format!("root/{name}"), content.clone()))
            .collect()
    }

    /// The entry `/init` that *applies* the personalization in the booted OS:
    /// exports each secret into the environment, then confirms the secrets are
    /// present (without printing their values) and the injected dotfiles are in
    /// place — and powers off. The dotfiles themselves are placed in the rootfs
    /// by [`home_files`](Self::home_files); this runner makes the secrets live in
    /// the environment and proves the personalization is applied on entry.
    #[must_use]
    pub fn entry_init(&self) -> Vec<u8> {
        let mut s = String::from("#!/bin/busybox sh\n");
        s.push_str("export PATH=/bin:/usr/bin\n");
        s.push_str("export HOME=/root\n");
        s.push_str("echo PERSONALIZATION-START\n");
        // Secrets → the environment (a Codespace exposes secrets as env vars).
        for (k, v) in &self.secrets {
            s.push_str("export ");
            s.push_str(k);
            s.push_str("='");
            s.push_str(v);
            s.push_str("'\n");
        }
        // Confirm each secret is present in the environment — without leaking it.
        for k in self.secrets.keys() {
            s.push_str("[ -n \"$");
            s.push_str(k);
            s.push_str("\" ] && echo SECRET-PRESENT:");
            s.push_str(k);
            s.push('\n');
        }
        // The dotfiles are injected into $HOME by the assembler; confirm + show
        // each (dotfiles are not secret).
        for name in self.dotfiles.keys() {
            s.push_str("echo DOTFILE-PRESENT:");
            s.push_str(name);
            s.push('\n');
            s.push_str("busybox cat /root/");
            s.push_str(name);
            s.push('\n');
        }
        s.push_str("echo PERSONALIZATION-DONE\n");
        s.push_str("busybox reboot -f\n");
        s.into_bytes()
    }

    /// The deterministic payload — the settings/dotfiles/secrets, length-framed
    /// (the `BTreeMap`s iterate in sorted order, so the bytes are reproducible).
    fn payload(&self) -> Vec<u8> {
        fn frame(out: &mut Vec<u8>, bytes: &[u8]) {
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        let mut out = Vec::new();
        frame(&mut out, self.settings.as_deref().unwrap_or("").as_bytes());
        out.extend_from_slice(&(self.dotfiles.len() as u32).to_le_bytes());
        for (k, v) in &self.dotfiles {
            frame(&mut out, k.as_bytes());
            frame(&mut out, v);
        }
        out.extend_from_slice(&(self.secrets.len() as u32).to_le_bytes());
        for (k, v) in &self.secrets {
            frame(&mut out, k.as_bytes());
            frame(&mut out, v.as_bytes());
        }
        out
    }

    /// Recover a personalization from its canonical form (the operator is the one
    /// embedded operand; the settings/dotfiles/secrets are the payload).
    ///
    /// # Errors
    ///
    /// [`RealizationError`] if the bytes are not a well-formed personalization.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, RealizationError> {
        let refs = <Self as Realization>::references(bytes)?;
        let operator = *refs.first().ok_or(RealizationError::Malformed)?;
        let payload = canonical_payload(bytes)?;
        let mut cur = 0usize;
        let settings_bytes = take_frame(payload, &mut cur)?;
        let settings = if settings_bytes.is_empty() {
            None
        } else {
            Some(
                core::str::from_utf8(settings_bytes)
                    .map_err(|_| RealizationError::Malformed)?
                    .to_owned(),
            )
        };
        let mut dotfiles = BTreeMap::new();
        let n = take_u32(payload, &mut cur)? as usize;
        for _ in 0..n {
            let k = core::str::from_utf8(take_frame(payload, &mut cur)?)
                .map_err(|_| RealizationError::Malformed)?
                .to_owned();
            let v = take_frame(payload, &mut cur)?.to_vec();
            dotfiles.insert(k, v);
        }
        let mut secrets = BTreeMap::new();
        let n = take_u32(payload, &mut cur)? as usize;
        for _ in 0..n {
            let k = core::str::from_utf8(take_frame(payload, &mut cur)?)
                .map_err(|_| RealizationError::Malformed)?
                .to_owned();
            let v = core::str::from_utf8(take_frame(payload, &mut cur)?)
                .map_err(|_| RealizationError::Malformed)?
                .to_owned();
            secrets.insert(k, v);
        }
        Ok(Self {
            operator,
            settings,
            dotfiles,
            secrets,
        })
    }
}

impl Realization for Personalization {
    const IRI: hologram_substrate_core::RealizationId = Personalization::IRI;

    fn canonicalize(&self) -> Vec<u8> {
        encode(Self::IRI, &[self.operator], &self.payload())
    }

    fn references(canonical_bytes: &[u8]) -> Result<References, RealizationError> {
        extract_refs(Self::IRI, canonical_bytes)
    }
}

/// The payload slice of a canonical personalization: skip the IRI, the operand
/// κ-labels, and the payload length prefix.
fn canonical_payload(bytes: &[u8]) -> Result<&[u8], RealizationError> {
    const KAPPA71: usize = 71;
    let nul = bytes
        .iter()
        .position(|&b| b == 0)
        .ok_or(RealizationError::Malformed)?;
    let mut cur = nul + 1;
    let n = take_u32(bytes, &mut cur)? as usize;
    cur = cur
        .checked_add(n.checked_mul(KAPPA71).ok_or(RealizationError::Truncated)?)
        .ok_or(RealizationError::Truncated)?;
    let len = take_u32(bytes, &mut cur)? as usize;
    let end = cur.checked_add(len).ok_or(RealizationError::Truncated)?;
    bytes.get(cur..end).ok_or(RealizationError::Truncated)
}

fn take_u32(bytes: &[u8], cur: &mut usize) -> Result<u32, RealizationError> {
    let end = cur.checked_add(4).ok_or(RealizationError::Truncated)?;
    let arr: [u8; 4] = bytes
        .get(*cur..end)
        .ok_or(RealizationError::Truncated)?
        .try_into()
        .map_err(|_| RealizationError::Truncated)?;
    *cur = end;
    Ok(u32::from_le_bytes(arr))
}

fn take_frame<'a>(bytes: &'a [u8], cur: &mut usize) -> Result<&'a [u8], RealizationError> {
    let len = take_u32(bytes, cur)? as usize;
    let end = cur.checked_add(len).ok_or(RealizationError::Truncated)?;
    let out = bytes.get(*cur..end).ok_or(RealizationError::Truncated)?;
    *cur = end;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(operator: &Operator) -> Personalization {
        Personalization::new(operator)
            .with_settings(r#"{"editor.fontSize":15}"#)
            .with_dotfile(".gitconfig", &b"[user]\n\tname = Operator\n"[..])
            .with_secret("GH_TOKEN", "gho_example")
    }

    #[test]
    fn personalization_is_content_scoped_to_the_operator() {
        let alice = Operator::from_public_key(b"alice-key");
        let bob = Operator::from_public_key(b"bob-key");
        // Same content + same operator → the same κ (reproducible identity).
        assert_eq!(sample(&alice).kappa(), sample(&alice).kappa());
        // The same content under a different operator → a different κ (scoped).
        assert_ne!(sample(&alice).kappa(), sample(&bob).kappa());
    }

    #[test]
    fn personalization_round_trips_through_its_canonical_form() {
        let operator = Operator::from_public_key(b"operator-key");
        let p = sample(&operator);
        let bytes = p.canonicalize();
        let back = Personalization::from_canonical(&bytes).expect("decode");
        assert_eq!(back, p);
        assert_eq!(back.operator(), operator.identity());
        assert_eq!(back.workbench_settings(), Some(r#"{"editor.fontSize":15}"#));
        assert_eq!(back.kappa(), p.kappa());
    }

    #[test]
    fn entry_init_applies_secrets_and_confirms_dotfiles() {
        let operator = Operator::from_public_key(b"operator-key");
        let init = String::from_utf8(sample(&operator).entry_init()).unwrap();
        assert!(init.starts_with("#!/bin/busybox sh\n"));
        assert!(init.contains("export GH_TOKEN='gho_example'"));
        // Presence is confirmed without leaking the value.
        assert!(init.contains("echo SECRET-PRESENT:GH_TOKEN"));
        assert!(init.contains("echo DOTFILE-PRESENT:.gitconfig"));
        assert!(init.contains("busybox cat /root/.gitconfig"));
        assert!(init.contains("busybox reboot -f"));
    }

    #[test]
    fn home_files_place_dotfiles_under_root_home() {
        let operator = Operator::from_public_key(b"operator-key");
        let files = sample(&operator).home_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "root/.gitconfig");
        assert_eq!(files[0].1, b"[user]\n\tname = Operator\n");
    }
}
