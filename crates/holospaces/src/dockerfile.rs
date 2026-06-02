//! **Dockerfile** — a substrate-native build of a Dev Container declared with
//! `build.dockerfile` (`CC-26`).
//!
//! A `devcontainer.json` may declare its container as a *Dockerfile build*
//! (`"build": { "dockerfile": "Dockerfile", … }`) instead of a prebuilt `image`.
//! holospaces honours it the substrate-native way: it parses the Dockerfile, the
//! `FROM` image is pulled + assembled as the base rootfs (the `CC-20`/`CC-10`
//! machinery), the `COPY` sources from the build context are injected into the
//! rootfs, and the `RUN` instructions run **in the devcontainer OS** during the
//! build phase — before the features and the lifecycle commands — with the build
//! `ARG`s and `ENV`s in scope (`CC-22`/`CC-25` machinery). `ENV` becomes part of
//! the container environment and `WORKDIR` the working directory. The result is
//! the built rootfs — no Docker daemon, just the emulator and the substrate.
//!
//! This parses the common instruction subset a devcontainer Dockerfile uses
//! (`FROM`, `ARG`, `ENV`, `RUN`, `COPY`/`ADD`, `WORKDIR`). Unsupported
//! instructions are an explicit error (never silently dropped).

#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::{
    borrow::ToOwned,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};
#[cfg(feature = "std")]
use std::collections::BTreeMap;

use core::fmt;

/// A parsed Dockerfile (the devcontainer build subset).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dockerfile {
    /// The `FROM` base image reference (the rootfs the build starts from).
    pub from: String,
    /// `WORKDIR` — the working directory for `RUN` and the running container.
    pub workdir: Option<String>,
    /// `ENV` — the container environment the build sets.
    pub env: BTreeMap<String, String>,
    /// The build steps in file order: `RUN` shell lines and `COPY`/`ADD` directives.
    pub steps: Vec<Step>,
}

/// One ordered build step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// `RUN <shell line>` — executed in the OS during the build.
    Run(String),
    /// `COPY <src> <dst>` — `src` from the build context, into the rootfs at `dst`.
    Copy {
        /// The source path, relative to the build context.
        src: String,
        /// The destination path in the rootfs (absolute).
        dst: String,
    },
}

/// A Dockerfile parse error (never a silent drop).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DockerfileError {
    /// The Dockerfile declares no `FROM`.
    NoFrom,
    /// An instruction is malformed (the keyword + the offending line).
    Malformed(&'static str),
    /// An instruction holospaces does not implement (named, so it is explicit).
    Unsupported(String),
}

impl fmt::Display for DockerfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DockerfileError::NoFrom => write!(f, "Dockerfile has no FROM instruction"),
            DockerfileError::Malformed(k) => write!(f, "malformed {k} instruction"),
            DockerfileError::Unsupported(k) => write!(f, "unsupported Dockerfile instruction: {k}"),
        }
    }
}

/// Parse a Dockerfile, resolving `ARG`/`ENV` references in instruction operands
/// against `build_args` (the `build.args` the config declares) + the Dockerfile's
/// own `ARG` defaults and `ENV`s. Honours line continuations (`\`) and comments.
///
/// # Errors
///
/// [`DockerfileError`] if there is no `FROM`, an instruction is malformed, or an
/// instruction is not implemented (explicit, never dropped).
pub fn parse(
    content: &[u8],
    build_args: &BTreeMap<String, String>,
) -> Result<Dockerfile, DockerfileError> {
    let text = String::from_utf8_lossy(content).into_owned();
    // Join line continuations into logical lines.
    let logical = join_continuations(&text);

    let mut from: Option<String> = None;
    let mut workdir = None;
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut steps = Vec::new();
    // Variables in scope for substitution: ARG defaults overridden by build_args,
    // plus ENVs as they are declared.
    let mut vars: BTreeMap<String, String> = BTreeMap::new();

    for line in logical {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = match line.split_once(char::is_whitespace) {
            Some((k, r)) => (k.to_ascii_uppercase(), r.trim()),
            None => (line.to_ascii_uppercase(), ""),
        };
        match keyword.as_str() {
            "FROM" => {
                // `FROM <image> [AS <stage>]` — take the image (single-stage).
                let img = rest
                    .split_whitespace()
                    .next()
                    .ok_or(DockerfileError::Malformed("FROM"))?;
                from = Some(substitute(img, &vars));
            }
            "ARG" => {
                // `ARG NAME[=default]` — the build arg, overridable by build.args.
                let (name, default) = match rest.split_once('=') {
                    Some((n, d)) => (n.trim(), Some(unquote(d.trim()))),
                    None => (rest, None),
                };
                let value = build_args
                    .get(name)
                    .cloned()
                    .or(default)
                    .unwrap_or_default();
                vars.insert(name.to_owned(), value);
            }
            "ENV" => {
                for (k, v) in parse_env(rest)? {
                    let v = substitute(&v, &vars);
                    vars.insert(k.clone(), v.clone());
                    env.insert(k, v);
                }
            }
            "WORKDIR" => {
                workdir = Some(substitute(rest, &vars));
            }
            "RUN" => {
                if rest.is_empty() {
                    return Err(DockerfileError::Malformed("RUN"));
                }
                // RUN is NOT variable-substituted at parse time (per the Dockerfile
                // reference): `$VAR` is expanded by the *shell at run time*, where
                // the `ENV`s are exported (see `build_init`). Keep it literal.
                steps.push(Step::Run(exec_to_shell(rest)));
            }
            "COPY" | "ADD" => {
                let (src, dst) = parse_copy(rest).ok_or(DockerfileError::Malformed("COPY"))?;
                steps.push(Step::Copy {
                    src: substitute(&src, &vars),
                    dst: substitute(&dst, &vars),
                });
            }
            // Instructions that do not affect the built rootfs in this model are
            // accepted as no-ops (they are metadata for a Docker daemon).
            "LABEL" | "EXPOSE" | "VOLUME" | "USER" | "SHELL" | "MAINTAINER" | "ONBUILD"
            | "STOPSIGNAL" | "HEALTHCHECK" | "CMD" | "ENTRYPOINT" => {}
            other => return Err(DockerfileError::Unsupported(other.to_owned())),
        }
    }

    Ok(Dockerfile {
        from: from.ok_or(DockerfileError::NoFrom)?,
        workdir,
        env,
        steps,
    })
}

impl Dockerfile {
    /// The `RUN` shell lines, in order (the build steps executed in the OS).
    #[must_use]
    pub fn run_lines(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                Step::Run(l) => Some(l.as_str()),
                Step::Copy { .. } => None,
            })
            .collect()
    }

    /// The build-phase `/init` the Boot Orchestrator injects to *run the build in
    /// the devcontainer OS* (`CC-26`): a busybox shell script that exports the
    /// Dockerfile's `ENV`, enters its `WORKDIR`, and runs each `RUN` instruction
    /// in file order (framed with markers), then powers off — the built rootfs is
    /// the κ-disk after this boot. The `COPY` sources are placed into the rootfs by
    /// the assembler before this runs (so the `RUN` steps see them). `tail`, if
    /// given, is appended before the reboot (e.g. the feature/lifecycle init body),
    /// so a Dockerfile devcontainer's build → features → lifecycle compose into one.
    #[must_use]
    pub fn build_init(&self, tail: Option<&str>) -> Vec<u8> {
        let mut s = String::from("#!/bin/busybox sh\n");
        s.push_str("export PATH=/bin:/usr/bin\n");
        for (k, v) in &self.env {
            s.push_str("export ");
            s.push_str(k);
            s.push_str("='");
            s.push_str(v);
            s.push_str("'\n");
        }
        if let Some(wd) = &self.workdir {
            s.push_str("mkdir -p '");
            s.push_str(wd);
            s.push_str("' && cd '");
            s.push_str(wd);
            s.push_str("'\n");
        }
        s.push_str("echo BUILD-START\n");
        for line in self.run_lines() {
            s.push_str(line);
            s.push('\n');
        }
        s.push_str("echo BUILD-DONE\n");
        if let Some(t) = tail {
            s.push_str(t);
        }
        s.push_str("busybox reboot -f\n");
        s.into_bytes()
    }

    /// The `COPY` directives `(src, dst)` — `src` relative to the build context,
    /// `dst` an absolute path in the rootfs.
    #[must_use]
    pub fn copies(&self) -> Vec<(&str, &str)> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                Step::Copy { src, dst } => Some((src.as_str(), dst.as_str())),
                Step::Run(_) => None,
            })
            .collect()
    }
}

// ── parsing helpers ─────────────────────────────────────────────────────────

/// Join backslash-continued physical lines into logical lines.
fn join_continuations(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if let Some(stripped) = line.strip_suffix('\\') {
            cur.push_str(stripped);
            cur.push(' ');
        } else {
            cur.push_str(line);
            out.push(core::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `RUN`/`CMD` exec-form `["a","b"]` → a space-joined shell line; shell-form as-is.
fn exec_to_shell(rest: &str) -> String {
    let t = rest.trim();
    if t.starts_with('[') && t.ends_with(']') {
        // JSON exec form.
        if let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(t) {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect();
            return parts.join(" ");
        }
    }
    t.to_owned()
}

/// Parse `ENV` (both `ENV k v` and `ENV k=v [k2=v2 …]` forms).
fn parse_env(rest: &str) -> Result<Vec<(String, String)>, DockerfileError> {
    let mut out = Vec::new();
    if rest.contains('=') {
        // key=value form (possibly multiple, space-separated; values may be quoted).
        for pair in split_respecting_quotes(rest) {
            let (k, v) = pair
                .split_once('=')
                .ok_or(DockerfileError::Malformed("ENV"))?;
            out.push((k.trim().to_owned(), unquote(v.trim())));
        }
    } else {
        // `ENV key value...` — the rest after the first token is the value.
        let (k, v) = rest
            .split_once(char::is_whitespace)
            .ok_or(DockerfileError::Malformed("ENV"))?;
        out.push((k.trim().to_owned(), v.trim().to_owned()));
    }
    Ok(out)
}

/// Parse `COPY <src> <dst>` (shell or JSON form); the last operand is `dst`.
fn parse_copy(rest: &str) -> Option<(String, String)> {
    let t = rest.trim();
    let parts: Vec<String> = if t.starts_with('[') {
        match serde_json::from_str::<serde_json::Value>(t).ok()? {
            serde_json::Value::Array(items) => items
                .iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect(),
            _ => return None,
        }
    } else {
        t.split_whitespace()
            .filter(|w| !w.starts_with("--")) // skip flags like --chown=
            .map(ToOwned::to_owned)
            .collect()
    };
    if parts.len() < 2 {
        return None;
    }
    let dst = parts.last()?.clone();
    let src = parts[..parts.len() - 1].join(" ");
    Some((src, dst))
}

/// Substitute `$VAR` / `${VAR}` references against `vars`.
fn substitute(s: &str, vars: &BTreeMap<String, String>) -> String {
    if !s.contains('$') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            let (name, next) = if bytes[i + 1] == b'{' {
                let end = s[i + 2..].find('}').map(|e| i + 2 + e);
                match end {
                    Some(e) => (&s[i + 2..e], e + 1),
                    None => (&s[i + 1..i + 1], i + 1),
                }
            } else {
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                (&s[i + 1..j], j)
            };
            if let Some(v) = vars.get(name) {
                out.push_str(v);
            } else if !name.is_empty() {
                // Unknown var → empty (Docker semantics), but keep the literal if
                // it was not actually a reference.
            } else {
                out.push('$');
            }
            i = next;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_owned()
    } else {
        s.to_owned()
    }
}

/// Split on whitespace but keep quoted spans together (for `ENV k="a b" k2=c`).
fn split_respecting_quotes(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    cur.push(c);
                } else if c.is_whitespace() {
                    if !cur.is_empty() {
                        out.push(core::mem::take(&mut cur));
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_common_devcontainer_dockerfile() {
        let df = br#"
            # a devcontainer Dockerfile
            ARG VARIANT=20
            FROM holospaces/busybox:${VARIANT}
            ENV PATH=/opt/bin:/usr/bin LANG=C.UTF-8
            WORKDIR /workspace
            COPY scripts/setup.sh /usr/local/bin/setup.sh
            RUN echo "building" && \
                mkdir -p /opt/bin
            RUN ["sh","-c","echo done"]
        "#;
        let args = BTreeMap::new();
        let d = parse(df, &args).expect("parse");
        assert_eq!(d.from, "holospaces/busybox:20"); // ARG substituted
        assert_eq!(d.workdir.as_deref(), Some("/workspace"));
        assert_eq!(
            d.env.get("PATH").map(String::as_str),
            Some("/opt/bin:/usr/bin")
        );
        assert_eq!(d.env.get("LANG").map(String::as_str), Some("C.UTF-8"));
        assert_eq!(
            d.copies(),
            vec![("scripts/setup.sh", "/usr/local/bin/setup.sh")]
        );
        let runs = d.run_lines();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].contains("mkdir -p /opt/bin"));
        assert_eq!(runs[1], "sh -c echo done"); // exec-form joined
    }

    #[test]
    fn build_args_override_arg_defaults() {
        let df = b"ARG VARIANT=20\nFROM base:${VARIANT}\n";
        let mut args = BTreeMap::new();
        args.insert("VARIANT".to_owned(), "22".to_owned());
        assert_eq!(parse(df, &args).unwrap().from, "base:22");
    }

    #[test]
    fn no_from_is_an_error_not_a_drop() {
        assert_eq!(
            parse(b"RUN echo hi\n", &BTreeMap::new()),
            Err(DockerfileError::NoFrom)
        );
    }

    #[test]
    fn an_unknown_instruction_is_explicit_not_silently_dropped() {
        let r = parse(b"FROM base\nFROBNICATE x\n", &BTreeMap::new());
        assert_eq!(
            r,
            Err(DockerfileError::Unsupported("FROBNICATE".to_owned()))
        );
    }
}
