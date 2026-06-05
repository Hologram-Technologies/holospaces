//! **Import boundary** (ADR-013, `CC-20`) — where holospaces reaches the
//! internet to bring a devcontainer's content *into* the substrate.
//!
//! A user names a devcontainer by a *repository URL*; holospaces fetches the
//! repository's content from its git host, reads its `devcontainer.json` (or
//! falls back to a default image when the repository has none), and pulls the
//! devcontainer's base image from its container registry. The internet is an
//! **untrusted gateway**: every byte is *verified by re-derivation* on the way
//! in (an OCI digest *is* a κ on the `sha256` axis — `CC-10`, Law L5), so a
//! located URL / registry reference becomes content-addressed identity. From the
//! κ inward, everything is uor-native ([`crate::oci`] → [`crate::assembly`] →
//! [`crate::machine`]).
//!
//! This is a host-side surface (the `net` feature): it links an HTTP(S) client.
//! The portable peer core never compiles it; the browser peer imports through
//! the page's own `fetch` at the same verify-on-receipt boundary.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use std::io::Read;

use hologram_substrate_core::KappaStore;

use crate::assembly::{
    assemble_ext4, assemble_ext4_with_files, find_devcontainer_json, read_archive_file, Layer,
};
use crate::boot::devcontainer::{self, ImageSource};
use crate::emulator::Arch;
use crate::oci::{
    ingest_image, is_index, select_platform_manifest, sha256_digest, synth_index, IngestedImage,
    OciError, ACCEPT_MANIFESTS,
};
// The OCI reference parsing + the pull's pure logic live in `crate::oci` (shared
// with the browser peer's page-driven pull); re-exported here as the import
// boundary's public surface (ADR-013).
pub use crate::oci::{parse_image_ref, ImageRef};
use crate::{compose, dockerfile};
use alloc::collections::BTreeMap;

/// The default Dev Container base image (re-exported from the crate root, where
/// it is always compiled so the browser peer shares the same value) — used when
/// a repository declares no `devcontainer.json`. See
/// [`crate::DEFAULT_DEVCONTAINER_IMAGE`].
pub use crate::DEFAULT_DEVCONTAINER_IMAGE;

/// What can go wrong crossing the import boundary.
#[derive(Debug)]
pub enum ImportError {
    /// A network request failed (transport, DNS, TLS, timeout).
    Transport(String),
    /// An HTTP response had a non-success status.
    Http {
        /// The URL that returned the error status.
        url: String,
        /// The HTTP status code.
        status: u16,
    },
    /// The fetched content was malformed (bad JSON, missing fields).
    BadContent(String),
    /// Ingestion / verification by re-derivation failed (Law L5) — including a
    /// malformed reference or no image for the architecture (the OCI pull logic
    /// is shared with the browser peer; [`crate::oci`]).
    Oci(OciError),
}

impl core::fmt::Display for ImportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ImportError::Transport(m) => write!(f, "network transport: {m}"),
            ImportError::Http { url, status } => write!(f, "HTTP {status} for {url}"),
            ImportError::BadContent(m) => write!(f, "bad content: {m}"),
            ImportError::Oci(e) => write!(f, "ingestion: {e}"),
        }
    }
}

impl From<OciError> for ImportError {
    fn from(e: OciError) -> Self {
        ImportError::Oci(e)
    }
}

// ── HTTP(S) transport ──────────────────────────────────────────────────────

/// A fetched HTTP response: status, the `Content-Type`, and the body bytes.
struct Response {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

/// `GET url`, following redirects, with optional `Accept` and bearer token.
/// Returns the response even on non-2xx (the caller may inspect a 401).
fn http_get(
    url: &str,
    accept: Option<&str>,
    bearer: Option<&str>,
) -> Result<Response, ImportError> {
    let mut req = ureq::get(url);
    if let Some(a) = accept {
        req = req.set("Accept", a);
    }
    if let Some(t) = bearer {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    req = req.set("User-Agent", "holospaces-import/0 (ADR-013)");
    match req.call() {
        Ok(resp) => Ok(read_response(resp)),
        // A non-2xx status is returned as Err(Status) by ureq; surface the
        // response so the caller can react (e.g. a 401 auth challenge).
        Err(ureq::Error::Status(_, resp)) => Ok(read_response(resp)),
        Err(ureq::Error::Transport(t)) => Err(ImportError::Transport(t.to_string())),
    }
}

fn read_response(resp: ureq::Response) -> Response {
    let status = resp.status();
    let content_type = resp.content_type().to_string();
    // The read is bounded by the *content's own* declared length — a registry
    // sends each blob's exact `Content-Length`, so a real layer of any size is
    // read in full (no arbitrary cap), while a hostile gateway cannot stream
    // unbounded: it must honour the length it declared, and every blob is then
    // verified by digest re-derivation on ingest (Law L5), so garbage is
    // refused. A chunked response with no declared length (rare for blobs) is
    // bounded generously so an unbounded stream still cannot exhaust memory.
    let declared = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());
    const NO_LENGTH_BOUND: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB, chunked fallback
    let cap = declared.map_or(NO_LENGTH_BOUND, |n| n.saturating_add(4096));
    let mut body = Vec::new();
    let _ = resp.into_reader().take(cap).read_to_end(&mut body);
    Response {
        status,
        content_type,
        body,
    }
}

/// `GET` that requires a 2xx, returning the body.
fn http_get_ok(
    url: &str,
    accept: Option<&str>,
    bearer: Option<&str>,
) -> Result<Vec<u8>, ImportError> {
    let r = http_get(url, accept, bearer)?;
    if (200..300).contains(&r.status) {
        Ok(r.body)
    } else {
        Err(ImportError::Http {
            url: url.to_string(),
            status: r.status,
        })
    }
}

// ── Repository fetch (the git host) ────────────────────────────────────────

/// Fetch a repository's content at `reference` as a `tar.gz` archive over the
/// internet. Supports the common git-host archive convention
/// `<repo>/archive/<ref>.tar.gz` (GitHub, Gitea, and the hermetic test server);
/// an explicit `…​.tar.gz` URL is fetched as-is.
pub fn fetch_repo_archive(repo_url: &str, reference: &str) -> Result<Vec<u8>, ImportError> {
    let archive_url = repo_archive_url(repo_url, reference);
    http_get_ok(&archive_url, None, None)
}

/// Construct the archive URL for a repository URL + reference.
fn repo_archive_url(repo_url: &str, reference: &str) -> String {
    if repo_url.ends_with(".tar.gz") {
        return repo_url.to_string();
    }
    let base = repo_url.trim_end_matches('/').trim_end_matches(".git");
    format!("{base}/archive/{reference}.tar.gz")
}

/// Read a repository archive's Dev Container config, or `None` if it has none.
pub fn repo_devcontainer_config(archive_tar_gz: &[u8]) -> Result<Option<Vec<u8>>, ImportError> {
    let layer = Layer {
        media_type: "application/gzip",
        blob: archive_tar_gz,
    };
    find_devcontainer_json(&layer).map_err(|e| ImportError::BadContent(e.to_string()))
}

// ── OCI image pull (the container registry) ────────────────────────────────

/// Pull an OCI image from its registry and ingest it into `store`, verifying
/// every blob by re-derivation against its digest (`CC-10`, Law L5). Handles
/// the registry's bearer-token auth challenge and multi-architecture image
/// indexes (selecting the holospace's `arch` manifest — `linux/riscv64` or
/// `linux/arm64`, ADR-021).
pub fn pull_image(
    store: &dyn KappaStore,
    image: &ImageRef,
    arch: Arch,
) -> Result<IngestedImage, ImportError> {
    let base = image.base();
    let token = obtain_token(image)?;
    let bearer = token.as_deref();

    // 1) Fetch the top-level manifest (by tag or digest).
    let manifest_url = format!("{base}/manifests/{}", image.reference);
    let top = http_get(&manifest_url, Some(ACCEPT_MANIFESTS), bearer)?;
    if !(200..300).contains(&top.status) {
        return Err(ImportError::Http {
            url: manifest_url,
            status: top.status,
        });
    }

    // 2) If it is a multi-arch index, select the `arch` manifest and fetch it.
    let (manifest_bytes, manifest_digest) = if is_index(&top.content_type, &top.body) {
        let digest = select_platform_manifest(&top.body, arch)?;
        let url = format!("{base}/manifests/{digest}");
        let m = http_get_ok(&url, Some(ACCEPT_MANIFESTS), bearer)?;
        (m, digest)
    } else {
        let digest = sha256_digest(&top.body);
        (top.body, digest)
    };

    // 3) Ingest: synthesize an OCI image-layout index pointing at this manifest,
    //    and serve blobs (manifest from cache, config/layers from the registry).
    //    ingest_image verifies every digest by re-derivation.
    let index = synth_index(&manifest_digest, manifest_bytes.len());
    let layout = br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec();
    let provider = |digest: &str| -> Option<Vec<u8>> {
        if digest == manifest_digest {
            return Some(manifest_bytes.clone());
        }
        let url = format!("{base}/blobs/{digest}");
        http_get_ok(&url, None, bearer).ok()
    };
    ingest_image(store, &layout, &index, arch, provider).map_err(ImportError::Oci)
}

/// Acquire a registry bearer token if the registry challenges for one (the
/// Docker token-auth flow). Returns `None` for registries that need no auth
/// (e.g. the hermetic localhost server).
fn obtain_token(image: &ImageRef) -> Result<Option<String>, ImportError> {
    let probe = format!("{}/manifests/{}", image.base(), image.reference);
    let r = http_get(&probe, Some(ACCEPT_MANIFESTS), None)?;
    if r.status != 401 {
        return Ok(None);
    }
    // Parse WWW-Authenticate is not exposed by ureq's error path here; use the
    // well-known Docker token endpoint derived from the registry.
    if image.registry == "registry-1.docker.io" {
        let url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            image.repository
        );
        let body = http_get_ok(&url, None, None)?;
        let v: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| ImportError::BadContent(e.to_string()))?;
        let tok = v
            .get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| ImportError::BadContent("no token in auth response".into()))?;
        return Ok(Some(tok.to_string()));
    }
    Ok(None)
}

// ── The import operation ───────────────────────────────────────────────────

/// The result of importing a devcontainer: its (validated) config bytes and the
/// ingested base image — ready for the Layer Assembler and the Boot Orchestrator.
pub struct ImportedDevcontainer {
    /// The `devcontainer.json` bytes (the repository's, or a synthesized default).
    pub config: Vec<u8>,
    /// Whether the default image was used (the repository declares no
    /// devcontainer). A declared `image`, `build`, or `dockerComposeFile` is
    /// always honoured — never silently defaulted.
    pub used_default: bool,
    /// The ingested, verified base image (the `image`, or a Dockerfile build's
    /// `FROM`).
    pub image: IngestedImage,
    /// The Dockerfile build plan, when the devcontainer is a `build` (`CC-26`):
    /// the build `/init` (its `RUN` steps) and the resolved `COPY` files, injected
    /// over the `FROM` base by [`import_and_assemble`].
    pub build: Option<BuildPlan>,
}

/// A resolved Dockerfile build (`CC-26`): the build `/init` and the `COPY` files
/// (destination path → bytes) to inject into the rootfs over the `FROM` base.
#[derive(Clone, Debug)]
pub struct BuildPlan {
    /// The build `/init` the OS runs (the Dockerfile's `RUN` steps).
    pub init: Vec<u8>,
    /// The `COPY` files: `(destination path in the rootfs, bytes)`.
    pub copy_files: Vec<(String, Vec<u8>)>,
}

/// Import a devcontainer from a repository URL: fetch the repository, read its
/// `devcontainer.json` (or apply the default image when it has none), and pull
/// the devcontainer's base image into `store` — all verified by re-derivation
/// at the import boundary (ADR-013, `CC-20`).
pub fn import_devcontainer(
    store: &dyn KappaStore,
    repo_url: &str,
    reference: &str,
    arch: Arch,
) -> Result<ImportedDevcontainer, ImportError> {
    let archive = fetch_repo_archive(repo_url, reference)?;
    let pull_default = |store: &dyn KappaStore| {
        pull_image(store, &parse_image_ref(DEFAULT_DEVCONTAINER_IMAGE)?, arch)
    };

    let (config, used_default, image, build) = match repo_devcontainer_config(&archive)? {
        Some(cfg) => {
            // Resolve the declared image *source* — honouring `build` (a Dockerfile
            // build, `CC-26`), never silently falling back to the default.
            let dc = devcontainer::parse(&cfg)
                .map_err(|e| ImportError::BadContent(format!("devcontainer.json: {e}")))?;
            let archive_layer = Layer {
                media_type: "application/gzip",
                blob: &archive,
            };
            let (image, build) = match &dc.image_source {
                ImageSource::Image(r) => (pull_image(store, &parse_image_ref(r)?, arch)?, None),
                ImageSource::Build(bc) => {
                    let (img, plan) = resolve_build(
                        store,
                        &archive_layer,
                        &bc.context,
                        &bc.dockerfile,
                        &bc.args,
                        arch,
                    )?;
                    (img, Some(plan))
                }
                ImageSource::Compose(cc) => {
                    // Read the compose file from the repository and resolve the
                    // devcontainer's `service` to its image / build (`CC-27`).
                    let file = cc.files.first().ok_or_else(|| {
                        ImportError::BadContent("compose file path missing".into())
                    })?;
                    let compose_bytes =
                        read_build_file(&archive_layer, "", file)?.ok_or_else(|| {
                            ImportError::BadContent(format!(
                                "compose file `{file}` not found in the repository"
                            ))
                        })?;
                    match compose::resolve_service(&compose_bytes, cc.service.as_deref())
                        .map_err(|e| ImportError::BadContent(format!("compose: {e}")))?
                    {
                        compose::ServiceSource::Image(r) => {
                            (pull_image(store, &parse_image_ref(&r)?, arch)?, None)
                        }
                        compose::ServiceSource::Build {
                            context,
                            dockerfile,
                            args,
                        } => {
                            let (img, plan) = resolve_build(
                                store,
                                &archive_layer,
                                &context,
                                &dockerfile,
                                &args,
                                arch,
                            )?;
                            (img, Some(plan))
                        }
                    }
                }
                ImageSource::Default => (pull_default(store)?, None),
            };
            let used_default = matches!(dc.image_source, ImageSource::Default);
            (cfg, used_default, image, build)
        }
        None => {
            let cfg = format!(r#"{{"image":"{DEFAULT_DEVCONTAINER_IMAGE}"}}"#).into_bytes();
            (cfg, true, pull_default(store)?, None)
        }
    };
    Ok(ImportedDevcontainer {
        config,
        used_default,
        image,
        build,
    })
}

/// Read a build file (the Dockerfile, or a `COPY` source) from a repository
/// archive: the path is `rel` under the build `context`, which is itself relative
/// to the folder holding `devcontainer.json` (so try both the repository root and
/// a `.devcontainer/` prefix).
fn read_build_file(
    archive: &Layer,
    context: &str,
    rel: &str,
) -> Result<Option<Vec<u8>>, ImportError> {
    let ctx = context.trim_start_matches("./").trim_matches('/');
    let rel = rel.trim_start_matches("./");
    let joined = if ctx.is_empty() {
        rel.to_owned()
    } else {
        format!("{ctx}/{rel}")
    };
    for prefix in ["", ".devcontainer/"] {
        let path = format!("{prefix}{joined}");
        if let Some(b) =
            read_archive_file(archive, &path).map_err(|e| ImportError::BadContent(e.to_string()))?
        {
            return Ok(Some(b));
        }
    }
    Ok(None)
}

/// Resolve a Dockerfile build (`CC-26`) from a repository `archive`: read the
/// Dockerfile from the build `context`, pull its `FROM` as the base image, and
/// resolve its `COPY` sources from the context — a missing Dockerfile or `COPY`
/// source is an explicit error, never a silent drop. Shared by `build` and a
/// compose service whose source is a build (`CC-27`).
fn resolve_build(
    store: &dyn KappaStore,
    archive: &Layer,
    context: &str,
    dockerfile: &str,
    args: &BTreeMap<String, String>,
    arch: Arch,
) -> Result<(IngestedImage, BuildPlan), ImportError> {
    let df_bytes = read_build_file(archive, context, dockerfile)?.ok_or_else(|| {
        ImportError::BadContent(format!(
            "build dockerfile `{dockerfile}` not found in the repository"
        ))
    })?;
    let df = dockerfile::parse(&df_bytes, args)
        .map_err(|e| ImportError::BadContent(format!("Dockerfile: {e}")))?;
    let image = pull_image(store, &parse_image_ref(&df.from)?, arch)?;
    let mut copy_files = Vec::new();
    for (src, dst) in df.copies() {
        let bytes = read_build_file(archive, context, src)?.ok_or_else(|| {
            ImportError::BadContent(format!(
                "COPY source `{src}` not found in the build context"
            ))
        })?;
        copy_files.push((dst.trim_start_matches('/').to_owned(), bytes));
    }
    Ok((
        image,
        BuildPlan {
            init: df.build_init(None),
            copy_files,
        },
    ))
}

/// Import a devcontainer from a repository URL and assemble its bootable `ext4`
/// root filesystem — the full import boundary → Layer Assembler path. The result
/// is the imported devcontainer and the rootfs bytes ready for
/// [`crate::machine::MachineSpec::boot`].
pub fn import_and_assemble(
    store: &dyn KappaStore,
    repo_url: &str,
    reference: &str,
    arch: Arch,
) -> Result<(ImportedDevcontainer, Vec<u8>), ImportError> {
    let imported = import_devcontainer(store, repo_url, reference, arch)?;
    // Resolve the verified layer blobs from the store and assemble them.
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();
    for (k, mt) in imported
        .image
        .layers()
        .iter()
        .zip(imported.image.layer_media_types())
    {
        let bytes = store
            .get(k)
            .map_err(|e| ImportError::BadContent(format!("store get: {e:?}")))?
            .ok_or_else(|| ImportError::BadContent("ingested layer missing from store".into()))?
            .as_ref()
            .to_vec();
        blobs.push((mt.clone(), bytes));
    }
    let layers: Vec<Layer> = blobs
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();
    let rootfs = if let Some(build) = &imported.build {
        // A Dockerfile build (`CC-26`): inject the build `/init` (the `RUN` steps)
        // and the `COPY` files over the `FROM` base, so the build runs in the OS.
        let mut owned: Vec<(String, u16, Vec<u8>)> =
            vec![("init".into(), 0o755, build.init.clone())];
        for (dst, bytes) in &build.copy_files {
            owned.push((dst.clone(), 0o755, bytes.clone()));
        }
        let files: Vec<(&str, u16, &[u8])> = owned
            .iter()
            .map(|(p, m, b)| (p.as_str(), *m, b.as_slice()))
            .collect();
        assemble_ext4_with_files(&layers, &files)
            .map_err(|e| ImportError::BadContent(format!("assemble build: {e}")))?
    } else {
        assemble_ext4(&layers).map_err(|e| ImportError::BadContent(format!("assemble: {e}")))?
    };
    Ok((imported, rootfs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_image_references_like_docker() {
        assert_eq!(
            parse_image_ref("debian:trixie").unwrap(),
            ImageRef {
                registry: "registry-1.docker.io".into(),
                repository: "library/debian".into(),
                reference: "trixie".into(),
            }
        );
        assert_eq!(
            parse_image_ref("ghcr.io/owner/img:1.2").unwrap(),
            ImageRef {
                registry: "ghcr.io".into(),
                repository: "owner/img".into(),
                reference: "1.2".into(),
            }
        );
        assert_eq!(
            parse_image_ref("127.0.0.1:5000/my/img").unwrap(),
            ImageRef {
                registry: "127.0.0.1:5000".into(),
                repository: "my/img".into(),
                reference: "latest".into(),
            }
        );
    }

    #[test]
    fn builds_archive_urls() {
        assert_eq!(
            repo_archive_url("https://github.com/org/repo", "main"),
            "https://github.com/org/repo/archive/main.tar.gz"
        );
        assert_eq!(
            repo_archive_url("https://github.com/org/repo.git", "v1"),
            "https://github.com/org/repo/archive/v1.tar.gz"
        );
    }
}
