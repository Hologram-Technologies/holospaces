//! `CC-65` — generate the PID-1 init that runs an arbitrary OCI image's REAL entrypoint.
//!
//! One pre-compiled, libc-agnostic freestanding amd64 init (`vv/artifacts/cc65/image-init`, built from
//! `image-init.c`) serves every image: the host patches the image's run config (argv/env/workdir/uid/gid,
//! distilled from the OCI image config) into the init's CONFIG table, located by its MAGIC. At boot the
//! init mounts the pseudo-filesystems, applies the env, `chdir`'s, drops to the user, and `execve`'s the
//! app DIRECTLY — no `/bin/sh`, so distroless/scratch images run too. This closes the only gap between an
//! ingested image and a running one: the image's own `Entrypoint`/`Cmd` becomes PID 1, fully over the
//! κ-disk (Law L1/L5 — the init is just more content-addressed bytes in the bootable `.holo`).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The container run config distilled from an OCI image config (or supplied directly).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunConfig {
    /// `entrypoint ++ cmd` — the argv to `execve` (must have ≥1 element).
    pub argv: Vec<String>,
    /// Environment, each `"KEY=VALUE"`.
    pub env: Vec<String>,
    /// Working directory; empty ⇒ `/`.
    pub workdir: String,
    /// Numeric uid/gid to drop to; `0` ⇒ stay root.
    pub uid: u32,
    pub gid: u32,
    /// If set, the freestanding init brings `eth0` up itself (static 10.0.2.15/24) via `socket`+`ioctl`
    /// BEFORE `execve` — so a NO-SHELL image (distroless/scratch) still gets networking without a shell
    /// prelude. The runtime sets this; the image's own entrypoint is unchanged. `CC-69`.
    pub net_up: bool,
}

const MAGIC: &[u8; 16] = b"HOLO-CC65-INIT\0\0";
const TABLE_CAP: usize = 16384 - 16;

/// Patch `cfg` into the generic init `template` (the compiled `image-init`), returning a ready `/init`
/// blob for [`crate::assembly::assemble_ext4_bootable`]. Returns `None` if the template lacks the CONFIG
/// table, the config is empty, or it does not fit (≈16 KiB of argv+env).
#[must_use]
pub fn image_init(template: &[u8], cfg: &RunConfig) -> Option<Vec<u8>> {
    if cfg.argv.is_empty() {
        return None;
    }
    // Locate the `.data` CONFIG buffer: the MAGIC occurrence followed by a run of zeros. (The `.rodata`
    // verify-string copy is followed by other rodata, so "followed by zeros" disambiguates reliably.)
    let mut at = None;
    let mut i = 0usize;
    while i + MAGIC.len() <= template.len() {
        if &template[i..i + MAGIC.len()] == MAGIC {
            let end = (i + 16 + 256).min(template.len());
            if template[i + 16..end].iter().all(|&b| b == 0) {
                at = Some(i);
                break;
            }
        }
        i += 1;
    }
    let at = at?;

    let mut t = Vec::new();
    t.extend_from_slice(&(cfg.argv.len() as u32).to_le_bytes());
    t.extend_from_slice(&(cfg.env.len() as u32).to_le_bytes());
    t.extend_from_slice(&cfg.uid.to_le_bytes());
    t.extend_from_slice(&cfg.gid.to_le_bytes());
    t.push(u8::from(cfg.net_up));
    for a in &cfg.argv {
        t.extend_from_slice(a.as_bytes());
        t.push(0);
    }
    for e in &cfg.env {
        t.extend_from_slice(e.as_bytes());
        t.push(0);
    }
    t.extend_from_slice(cfg.workdir.as_bytes());
    t.push(0);
    if t.len() > TABLE_CAP {
        return None;
    }

    let mut out = template.to_vec();
    out[at + 16..at + 16 + t.len()].copy_from_slice(&t);
    Some(out)
}

/// Distil a [`RunConfig`] from a raw OCI image config blob (the JSON at the image's `config` descriptor):
/// `argv = .config.Entrypoint ++ .config.Cmd`; `env = .config.Env`; `workdir = .config.WorkingDir`;
/// uid/gid parsed from `.config.User` when numeric (`"1000"` or `"1000:1000"`). A named `User` resolves
/// to root (0) in v1 — server images' entrypoints typically drop privileges themselves. Returns `None`
/// if neither `Entrypoint` nor `Cmd` is present (nothing to run).
#[cfg(feature = "std")]
#[must_use]
pub fn run_config_from_oci(config_json: &[u8]) -> Option<RunConfig> {
    use serde_json::Value;
    let v: Value = serde_json::from_slice(config_json).ok()?;
    let c = v.get("config").unwrap_or(&v);
    let strs = |key: &str| -> Vec<String> {
        c.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let mut argv = strs("Entrypoint");
    argv.extend(strs("Cmd"));
    if argv.is_empty() {
        return None;
    }
    let env = strs("Env");
    let workdir = c.get("WorkingDir").and_then(Value::as_str).unwrap_or("").to_string();
    let (mut uid, mut gid) = (0u32, 0u32);
    if let Some(u) = c.get("User").and_then(Value::as_str) {
        let mut it = u.split(':');
        if let Some(a) = it.next() {
            uid = a.parse().unwrap_or(0);
        }
        if let Some(b) = it.next() {
            gid = b.parse().unwrap_or(0);
        }
    }
    Some(RunConfig { argv, env, workdir, uid, gid, net_up: false })
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_oci_config() {
        // An nginx:alpine-shaped config (trimmed to the fields that matter).
        let json = br#"{
            "architecture":"amd64","os":"linux",
            "config":{
                "Entrypoint":["/docker-entrypoint.sh"],
                "Cmd":["nginx","-g","daemon off;"],
                "Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin","NGINX_VERSION=1.27"],
                "WorkingDir":"/app",
                "User":"101:101",
                "ExposedPorts":{"80/tcp":{}}
            }
        }"#;
        let cfg = run_config_from_oci(json).expect("parse");
        assert_eq!(cfg.argv, ["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]);
        assert_eq!(cfg.env.len(), 2);
        assert_eq!(cfg.workdir, "/app");
        assert_eq!((cfg.uid, cfg.gid), (101, 101));
    }

    #[test]
    fn distroless_cmd_only_named_user_is_root() {
        let json = br#"{"config":{"Cmd":["/app/server"],"User":"nonroot"}}"#;
        let cfg = run_config_from_oci(json).expect("parse");
        assert_eq!(cfg.argv, ["/app/server"]);
        assert_eq!((cfg.uid, cfg.gid), (0, 0)); // named user → root in v1
    }

    #[test]
    fn patcher_rejects_oversized_and_empty() {
        // A fake template with one CONFIG buffer (MAGIC + zeros).
        let mut tmpl = Vec::new();
        tmpl.extend_from_slice(b"....rodata....");
        tmpl.extend_from_slice(MAGIC);
        tmpl.extend_from_slice(&[0u8; 16384]);
        assert!(image_init(&tmpl, &RunConfig::default()).is_none(), "empty argv refused");
        let huge = RunConfig {
            argv: alloc::vec!["x".repeat(20000)],
            ..Default::default()
        };
        assert!(image_init(&tmpl, &huge).is_none(), "oversized refused");
        let ok = RunConfig {
            argv: alloc::vec!["/bin/app".to_string(), "--flag".to_string()],
            env: alloc::vec!["K=V".to_string()],
            workdir: "/srv".to_string(),
            uid: 5,
            gid: 6,
            net_up: false,
        };
        let out = image_init(&tmpl, &ok).expect("patch ok");
        assert_eq!(out.len(), tmpl.len(), "patch is in-place, never grows");
        // The argc/envc/uid/gid header landed right after MAGIC.
        let at = out.windows(16).position(|w| w == MAGIC).unwrap();
        assert_eq!(u32::from_le_bytes(out[at + 16..at + 20].try_into().unwrap()), 2); // argc
        assert_eq!(u32::from_le_bytes(out[at + 24..at + 28].try_into().unwrap()), 5); // uid
    }
}
