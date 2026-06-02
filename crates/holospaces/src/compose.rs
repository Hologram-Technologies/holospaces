//! **Docker Compose** — resolve the devcontainer's service from a
//! `dockerComposeFile` (`CC-27`).
//!
//! A `devcontainer.json` may declare its container with a Docker Compose file and
//! a `service` (the service that *is* the devcontainer). holospaces honours it by
//! resolving that service's image source from the compose file — its `image` (a
//! prebuilt image, pulled like `CC-20`) or its `build` (a Dockerfile build,
//! `CC-26`) — and provisioning from it, never silently dropping the compose
//! declaration. Multi-service orchestration (the *other* services) is out of
//! scope for a single devcontainer; the devcontainer is the one named `service`.
//!
//! This parses the block-style compose subset a devcontainer uses
//! (`services: <name>: { image | build }`); a `service`'s source that cannot be
//! resolved is an explicit error, never a silent default.

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

/// The image source a compose service declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceSource {
    /// `image: <ref>` — a prebuilt image reference.
    Image(String),
    /// `build:` — a Dockerfile build (short string form, or `context`/`dockerfile`/`args`).
    Build {
        /// The build context directory (default `"."`).
        context: String,
        /// The Dockerfile path within the context (default `"Dockerfile"`).
        dockerfile: String,
        /// The build args.
        args: BTreeMap<String, String>,
    },
}

/// A compose resolution error (never a silent default).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposeError {
    /// The compose file declares no `services`.
    NoServices,
    /// The named (or only) service was not found.
    ServiceNotFound(String),
    /// The compose file declares more than one service and none was selected.
    AmbiguousService,
    /// The service declares neither `image` nor `build`.
    NoImageSource(String),
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ComposeError::NoServices => write!(f, "compose file has no `services`"),
            ComposeError::ServiceNotFound(s) => write!(f, "compose service `{s}` not found"),
            ComposeError::AmbiguousService => {
                write!(
                    f,
                    "compose declares multiple services; none selected by `service`"
                )
            }
            ComposeError::NoImageSource(s) => {
                write!(
                    f,
                    "compose service `{s}` declares neither `image` nor `build`"
                )
            }
        }
    }
}

/// Resolve the image source of the devcontainer's `service` from a compose file.
/// If `service` is `None`, the file must declare exactly one service.
///
/// # Errors
///
/// [`ComposeError`] if there is no `services`, the service is not found or
/// ambiguous, or the service declares no image source.
pub fn resolve_service(
    content: &[u8],
    service: Option<&str>,
) -> Result<ServiceSource, ComposeError> {
    let text = String::from_utf8_lossy(content).into_owned();
    let lines: Vec<(usize, &str)> = text
        .lines()
        .map(|l| (indent(l), strip_comment(l)))
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();

    // Find the `services:` block and the indent of the service names under it.
    let mut i = 0;
    // Find the `services:` mapping (at whatever base indent the file uses).
    let services_indent = loop {
        if i >= lines.len() {
            return Err(ComposeError::NoServices);
        }
        let (ind, l) = lines[i];
        if key_of(l) == Some("services") {
            break ind;
        }
        i += 1;
    };
    i += 1;
    // Collect the service names (the first deeper indent under `services`).
    let svc_indent = lines
        .get(i)
        .map(|(ind, _)| *ind)
        .filter(|ind| *ind > services_indent)
        .ok_or(ComposeError::NoServices)?;
    let mut names = Vec::new();
    let mut j = i;
    while j < lines.len() {
        let (ind, l) = lines[j];
        if ind <= services_indent {
            break; // left the services block
        }
        if ind == svc_indent {
            if let Some(k) = key_of(l) {
                names.push((k.to_owned(), j));
            }
        }
        j += 1;
    }
    if names.is_empty() {
        return Err(ComposeError::NoServices);
    }

    // Select the service: the named one, or the only one.
    let (svc_name, start) = match service {
        Some(s) => names
            .iter()
            .find(|(n, _)| n == s)
            .cloned()
            .ok_or_else(|| ComposeError::ServiceNotFound(s.to_owned()))?,
        None => {
            if names.len() != 1 {
                return Err(ComposeError::AmbiguousService);
            }
            names[0].clone()
        }
    };

    // The service's body: lines after its header, indented deeper than svc_indent.
    let body_end = names
        .iter()
        .map(|(_, idx)| *idx)
        .filter(|idx| *idx > start)
        .min()
        .unwrap_or_else(|| {
            // up to the end of the services block
            let mut e = start + 1;
            while e < lines.len() && lines[e].0 > services_indent {
                e += 1;
            }
            e
        });
    let body = &lines[start + 1..body_end];

    // `image: <ref>` wins; else `build:` (string or map).
    for (ind, l) in body {
        if let Some((k, v)) = kv(l) {
            if *ind > svc_indent && k == "image" && !v.is_empty() {
                return Ok(ServiceSource::Image(unquote(v)));
            }
        }
    }
    // Find a `build:` entry under the service.
    let build_indent = body
        .iter()
        .find(|(ind, l)| *ind > svc_indent && key_of(l) == Some("build"))
        .map(|(ind, _)| *ind);
    if let Some(bi) = build_indent {
        // Short form `build: <context>` or map form with context/dockerfile/args.
        let header = body
            .iter()
            .find(|(ind, l)| *ind == bi && key_of(l) == Some("build"));
        if let Some((_, l)) = header {
            if let Some((_, v)) = kv(l) {
                if !v.is_empty() {
                    return Ok(ServiceSource::Build {
                        context: unquote(v),
                        dockerfile: "Dockerfile".to_owned(),
                        args: BTreeMap::new(),
                    });
                }
            }
        }
        // Map form: collect context / dockerfile / args from the deeper lines.
        let mut context = ".".to_owned();
        let mut dockerfile = "Dockerfile".to_owned();
        let mut args = BTreeMap::new();
        let mut in_args = false;
        let mut args_indent = usize::MAX;
        for (ind, l) in body {
            if *ind <= bi {
                if key_of(l) == Some("build") {
                    continue;
                }
                in_args = false;
                continue;
            }
            if in_args && *ind >= args_indent {
                if let Some((k, v)) = kv(l) {
                    args.insert(k.to_owned(), unquote(v));
                }
                continue;
            }
            in_args = false;
            if let Some((k, v)) = kv(l) {
                match k {
                    "context" => context = unquote(v),
                    "dockerfile" => dockerfile = unquote(v),
                    "args" => {
                        in_args = true;
                        args_indent = ind + 1; // anything deeper than the build sub-keys
                    }
                    _ => {}
                }
            }
        }
        return Ok(ServiceSource::Build {
            context,
            dockerfile,
            args,
        });
    }

    Err(ComposeError::NoImageSource(svc_name))
}

fn indent(l: &str) -> usize {
    l.chars().take_while(|c| *c == ' ').count()
}
fn strip_comment(l: &str) -> &str {
    // A `#` not inside a quote starts a comment (compose values rarely quote `#`).
    match l.find(" #") {
        Some(p) => &l[..p],
        None => {
            if l.trim_start().starts_with('#') {
                ""
            } else {
                l
            }
        }
    }
}
fn key_of(l: &str) -> Option<&str> {
    let t = l.trim();
    let k = t.split_once(':').map(|(k, _)| k.trim())?;
    if k.is_empty() {
        None
    } else {
        Some(k)
    }
}
fn kv(l: &str) -> Option<(&str, &str)> {
    let t = l.trim();
    let (k, v) = t.split_once(':')?;
    Some((k.trim(), v.trim()))
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

#[cfg(test)]
mod tests {
    use super::*;

    const COMPOSE: &[u8] = br#"
        services:
          app:
            image: node:20
            ports:
              - "3000:3000"
          db:
            image: postgres:16
    "#;

    #[test]
    fn resolves_the_named_service_image() {
        assert_eq!(
            resolve_service(COMPOSE, Some("db")).unwrap(),
            ServiceSource::Image("postgres:16".to_owned())
        );
        assert_eq!(
            resolve_service(COMPOSE, Some("app")).unwrap(),
            ServiceSource::Image("node:20".to_owned())
        );
    }

    #[test]
    fn a_missing_service_is_an_error_not_a_default() {
        assert_eq!(
            resolve_service(COMPOSE, Some("nope")),
            Err(ComposeError::ServiceNotFound("nope".to_owned()))
        );
    }

    #[test]
    fn multiple_services_without_a_selection_is_ambiguous() {
        assert_eq!(
            resolve_service(COMPOSE, None),
            Err(ComposeError::AmbiguousService)
        );
    }

    #[test]
    fn resolves_a_build_service() {
        let c = br#"
            services:
              dev:
                build:
                  context: ./app
                  dockerfile: Dockerfile.dev
                  args:
                    TAG: "22"
        "#;
        assert_eq!(
            resolve_service(c, Some("dev")).unwrap(),
            ServiceSource::Build {
                context: "./app".to_owned(),
                dockerfile: "Dockerfile.dev".to_owned(),
                args: BTreeMap::from([("TAG".to_owned(), "22".to_owned())]),
            }
        );
    }

    #[test]
    fn resolves_a_single_service_without_selection() {
        let c = b"services:\n  only:\n    image: alpine:3\n";
        assert_eq!(
            resolve_service(c, None).unwrap(),
            ServiceSource::Image("alpine:3".to_owned())
        );
    }
}
