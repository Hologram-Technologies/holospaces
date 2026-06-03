//! The **Workspace Projection** (arc42 chapter 5; ADR-009; Chapter 8) — the
//! Codespaces/Gitpod experience over a *running* holospace, `CC-11`.
//!
//! A workspace projection is a thin view + intent surface over a running
//! holospace (the [system emulator](crate::emulator) booted to an OS): an
//! **Editor / FS view** that reads the environment's content *by κ*, and a
//! **Terminal / Intent** surface that publishes the operator's input as
//! **canonical events** on the holospace's channels. It holds no state of its
//! own (Law L3) — the canonical state is the running holospace's κ snapshot and
//! the content κ of what is rendered; an operator action is a canonical event
//! ([`Intent`]) addressed by content (Laws L1, L2), and driving the terminal
//! advances the holospace's κ snapshot.
//!
//! This module is the **model**; a rendered editor/terminal (a browser tab, a
//! native window) is a thin presentation over it, and a peer performs the store
//! I/O the [`Intent`]s describe. The model is environment-agnostic (`no_std`),
//! so every peer — browser, native, bare-metal — drives a workspace the same way.

use alloc::string::String;
use alloc::vec::Vec;

use crate::emulator::{Emulator, Halt};
use crate::realizations::{address, Kappa};

/// A canonical operator **intent** on a workspace — what the projection publishes
/// as an event on the holospace's channel. Either a line typed into the terminal
/// or a file edit. Its [`canonicalize`](Intent::canonicalize) bytes are the
/// canonical form (Law L2) and its [`kappa`](Intent::kappa) is the event's
/// identity (Law L1) — content, never a location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A line the operator typed into the terminal (the keystrokes; the newline
    /// is implied).
    Type(String),
    /// A file edit: replace the content at `path` (the editor's save).
    Edit {
        /// The path within the environment the operator edited.
        path: String,
        /// The new content (the editor buffer).
        content: Vec<u8>,
    },
}

impl Intent {
    /// The canonical byte form — deterministic, so identical intents address to
    /// the identical κ on any peer (Law L1/L2). A length-prefixed, tagged
    /// encoding (no ambiguity between the fields).
    #[must_use]
    pub fn canonicalize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Intent::Type(line) => {
                out.extend_from_slice(b"type\0");
                out.extend_from_slice(&(line.len() as u64).to_le_bytes());
                out.extend_from_slice(line.as_bytes());
            }
            Intent::Edit { path, content } => {
                out.extend_from_slice(b"edit\0");
                out.extend_from_slice(&(path.len() as u64).to_le_bytes());
                out.extend_from_slice(path.as_bytes());
                out.extend_from_slice(&(content.len() as u64).to_le_bytes());
                out.extend_from_slice(content);
            }
        }
        out
    }

    /// The event's κ — the canonical identity published on the channel (Law L1).
    #[must_use]
    pub fn kappa(&self) -> Kappa {
        address(&self.canonicalize())
    }

    /// For an [`Intent::Edit`], the κ of the new content — the editor's FS view
    /// advances to this address (an edit *is* a new content identity, Law L1).
    #[must_use]
    pub fn content_kappa(&self) -> Option<Kappa> {
        match self {
            Intent::Edit { content, .. } => Some(address(content)),
            Intent::Type(_) => None,
        }
    }
}

/// A workspace projection over a running holospace (the booted [`Emulator`]).
///
/// The projection borrows the running machine; it owns only the **channel** —
/// the κ *identity* of each operator event (Law L1). It is intentionally
/// store-less (Law L3: no duplicated state — the canonical state lives in the
/// holospace's κ snapshot and the substrate); the operator's *store-backed* peer
/// (the browser `Workspace`) persists each event's canonical bytes into its
/// `KappaStore`, so the channel's κ re-derive and **resolve** there. The bare
/// projection records identities; the substrate-backed peer makes them content.
pub struct Workspace<'m> {
    machine: &'m mut Emulator,
    channel: Vec<Kappa>,
}

impl<'m> Workspace<'m> {
    /// Attach a projection to a running holospace.
    pub fn attach(machine: &'m mut Emulator) -> Self {
        Self {
            machine,
            channel: Vec::new(),
        }
    }

    // ── Terminal view ──

    /// Render the terminal — the console the holospace has produced (content).
    #[must_use]
    pub fn terminal(&self) -> &[u8] {
        self.machine.console()
    }

    /// The κ of the rendered terminal content (what the editor/terminal show is
    /// content, addressable — Law L1).
    #[must_use]
    pub fn terminal_kappa(&self) -> Kappa {
        address(self.machine.console())
    }

    /// The running holospace's κ snapshot — the canonical state the projection
    /// views (it has none of its own, Law L3).
    #[must_use]
    pub fn state_kappa(&self) -> Kappa {
        address(&self.machine.snapshot())
    }

    /// The events published on the channel so far (their κ — Law L1).
    #[must_use]
    pub fn channel(&self) -> &[Kappa] {
        &self.channel
    }

    /// Advance the holospace until the terminal renders `marker` (e.g. the ready
    /// banner or the shell prompt), bounded by `budget` instructions. Returns
    /// `true` if the marker appeared — the projection waiting for the holospace.
    pub fn run_until(&mut self, marker: &[u8], budget: u64) -> bool {
        let chunk = 5_000_000u64;
        let mut spent = 0u64;
        while spent < budget {
            self.machine.run(chunk);
            spent += chunk;
            if contains(self.machine.console(), marker) {
                return true;
            }
        }
        contains(self.machine.console(), marker)
    }

    // ── Terminal intent ──

    /// Drive the terminal: publish the operator's `line` as a **canonical event**
    /// on the channel, feed the keystrokes (plus the newline) to the running
    /// holospace, and advance it until the response settles (the terminal stops
    /// growing — the holospace is idle awaiting the next line) or the machine
    /// powers off, bounded by `budget` instructions. The holospace's κ snapshot
    /// changes — the input advanced the running machine. Returns the event κ.
    pub fn type_line(&mut self, line: &str, budget: u64) -> Kappa {
        let intent = Intent::Type(String::from(line));
        let event = intent.kappa();
        self.channel.push(event);
        let mut keystrokes = Vec::with_capacity(line.len() + 1);
        keystrokes.extend_from_slice(line.as_bytes());
        keystrokes.push(b'\n');
        self.machine.feed_console(&keystrokes);

        let chunk = 2_000_000u64;
        let mut spent = 0u64;
        let mut prev = self.machine.console().len();
        loop {
            let halt = self.machine.run(chunk);
            spent += chunk;
            if matches!(halt, Halt::Exit(_)) {
                break; // the machine powered off (e.g. the `exit` line)
            }
            let now = self.machine.console().len();
            if now == prev {
                break; // the response settled; the holospace idles for input
            }
            prev = now;
            if spent >= budget {
                break;
            }
        }
        event
    }
}

/// Sub-slice search (the terminal-marker test) — `no_std`, no allocation.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
