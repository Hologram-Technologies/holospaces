//! `holo run <image-ref>` — pull ANY OCI image, boot its real entrypoint on the x86-64 κ substrate with
//! networking, and print a URL you can open/curl/share that hits the live app. The container-runtime made
//! real: `holo run nginx:alpine` → real nginx, content-addressed + L5-verified, answering your request.
//!
//! Build: cargo build --release -p holospaces --features net --bin holo-run
//! Run:   holo-run <image-ref> [--port N] [--public]     (e.g. `holo-run nginx:alpine`)

#[cfg(not(feature = "net"))]
fn main() {
    eprintln!("holo-run: build with `--features net` (it needs the registry HTTP client).");
    std::process::exit(2);
}

#[cfg(feature = "net")]
fn main() {
    if let Err(e) = run() {
        eprintln!("holo run: {e}");
        std::process::exit(1);
    }
}

/// Minimal gzip → raw bytes via the crate's own `miniz_oxide` (flate2 is dev-only). Strips the gzip
/// header (honoring the FEXTRA/FNAME/FCOMMENT/FHCRC flags) and inflates the deflate member.
#[cfg(feature = "net")]
fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err("kernel artifact is not gzip".into());
    }
    let flg = data[3];
    let mut p = 10usize;
    if flg & 4 != 0 {
        let xl = data[p] as usize | ((data[p + 1] as usize) << 8);
        p += 2 + xl;
    }
    if flg & 8 != 0 {
        while p < data.len() && data[p] != 0 {
            p += 1;
        }
        p += 1;
    }
    if flg & 16 != 0 {
        while p < data.len() && data[p] != 0 {
            p += 1;
        }
        p += 1;
    }
    if flg & 2 != 0 {
        p += 2;
    }
    let deflate = &data[p..data.len().saturating_sub(8)];
    miniz_oxide::inflate::decompress_to_vec(deflate).map_err(|e| format!("inflate: {:?}", e.status))
}

/// The image's first EXPOSEd port (min), if any.
#[cfg(feature = "net")]
fn exposed_port(config_json: &[u8]) -> Option<u16> {
    let v: serde_json::Value = serde_json::from_slice(config_json).ok()?;
    let ep = v.get("config")?.get("ExposedPorts")?.as_object()?;
    ep.keys()
        .filter_map(|k| k.split('/').next().and_then(|p| p.parse::<u16>().ok()))
        .min()
}

/// Best-effort LAN/public IP of this host (for `--public`).
#[cfg(feature = "net")]
fn host_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            Ok(s.local_addr()?.ip().to_string())
        })
        .unwrap_or_else(|_| "0.0.0.0".into())
}

#[cfg(feature = "net")]
fn run() -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::PathBuf;
    use std::time::Duration;

    use hologram_store_mem::MemKappaStore;
    use hologram_substrate_core::KappaStore;
    use holospaces::assembly::{assemble_ext4_bootable, Layer};
    use holospaces::emulator::net::{NoEgress, StdIngress};
    use holospaces::emulator::x64::Cpu;
    use holospaces::image_init::{image_init, run_config_from_oci, RunConfig};
    use holospaces::import::{parse_image_ref, pull_image};

    // ── args: `holo run <image> [--port N] [--public] [--no-cache] [--refresh] [-- <cmd args…>]` ──
    // Everything after a bare `--` overrides the image's entrypoint (docker-run parity) — needed for
    // images whose default entrypoint isn't a server (e.g. python's REPL): `-- python3 -m http.server`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut image, mut port_override, mut public) = (None, None, false);
    let mut cmd_override: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                cmd_override = args[i + 1..].to_vec();
                break;
            }
            "--port" => {
                i += 1;
                port_override = args.get(i).and_then(|s| s.parse::<u16>().ok());
            }
            "--public" => public = true,
            s if !s.starts_with("--") => image = Some(s.to_string()),
            _ => {}
        }
        i += 1;
    }
    let image =
        image.ok_or("usage: holo run <image-ref> [--port N] [--public] [--no-cache] [--refresh] [-- <cmd…>]")?;

    // ── artifacts (kernel + generic init template) ──
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let kernel = gunzip(
        &std::fs::read(root.join("vv/artifacts/cc45/linux/vmlinux.gz")).map_err(|e| format!("kernel: {e}"))?,
    )?;
    let template = std::fs::read(root.join("vv/artifacts/cc65/image-init"))
        .map_err(|e| format!("image-init template (compile vv/artifacts/cc65/image-init.c): {e}"))?;

    // ── warm-snapshot cache: boot ONCE, resume forever (CC-73/CC-74). ──
    // A heavy image (python/glibc) cold-boots for minutes; a warm resume is ~1s. The first run pulls +
    // boots + snapshots the serving machine to a content-keyed `.holo`; every later run RESUMES it with
    // NO pull and NO re-boot. The `.ref` sidecar maps the image-ref+port → {blob-key, guest-port} so the
    // resume path needs neither the registry nor the image config. Flags: `--no-cache` (never cache /
    // always cold), `--refresh` (re-pull + re-cache, ignoring an existing warm .holo).
    let no_cache = args.iter().any(|a| a == "--no-cache");
    let refresh = args.iter().any(|a| a == "--refresh");
    let warm_dir = std::env::var_os("HOLO_WARM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("holo-warm"));
    let _ = std::fs::create_dir_all(&warm_dir);
    let ref_key = {
        let k =
            holospaces::oci::sha256_digest(format!("{image}|{port_override:?}|{cmd_override:?}").as_bytes());
        k.strip_prefix("sha256:").unwrap_or(&k).to_string()
    };
    let ref_path = warm_dir.join(format!("{ref_key}.ref"));

    // ── fast path: resume a cached warm .holo WITHOUT pulling. ──
    let mut cpu = Cpu::new(0x1000);
    let mut warm = false;
    let mut guest_port = 0u16;
    let mut warm_path = PathBuf::new();
    if !no_cache && !refresh {
        if let Some((blob_key, gp)) = std::fs::read_to_string(&ref_path).ok().and_then(|s| {
            let mut it = s.lines();
            let bk = it.next()?.to_string();
            let gp = it.next()?.parse::<u16>().ok()?;
            Some((bk, gp))
        }) {
            let wp = warm_dir.join(format!("{blob_key}.holo"));
            if let Ok(blob) = std::fs::read(&wp) {
                let t = std::time::Instant::now();
                if cpu.restore_kappa_blob(&blob) {
                    eprintln!(
                        "holo run: resumed a warm .holo ({} MiB) in {:?} — no pull, no re-boot",
                        blob.len() / (1024 * 1024),
                        t.elapsed()
                    );
                    warm = true;
                    guest_port = gp;
                    warm_path = wp;
                } else {
                    eprintln!("holo run: cached .holo is stale/incompatible — cold-booting");
                    cpu = Cpu::new(0x1000);
                }
            }
        }
    }

    // ── cold path: pull the image, boot its REAL entrypoint with the network up, remember how to resume. ──
    if !warm {
        eprintln!("holo run: pulling {image} (amd64) from the registry …");
        let store = MemKappaStore::new();
        let iref = parse_image_ref(&image).map_err(|e| format!("bad image ref: {e:?}"))?;
        let img = pull_image(&store, &iref, holospaces::Arch::X64).map_err(|e| format!("pull: {e:?}"))?;
        let cfg_bytes = store
            .get(img.config())
            .map_err(|_| "config get")?
            .ok_or("config blob missing")?
            .as_ref()
            .to_vec();
        let rc = run_config_from_oci(&cfg_bytes).ok_or("image declares no Entrypoint/Cmd (nothing to run)")?;
        // The command to run: the `-- …` override if given, else the image's own entrypoint.
        let argv = if cmd_override.is_empty() { rc.argv.clone() } else { cmd_override.clone() };
        if argv.is_empty() {
            return Err("image declares no Entrypoint/Cmd and no `-- <cmd>` override was given".into());
        }
        guest_port = port_override.or_else(|| exposed_port(&cfg_bytes)).unwrap_or(80);
        eprintln!("holo run: running = {argv:?}  serving guest port {guest_port}");

        let layer_bytes: Vec<Vec<u8>> =
            img.layers().iter().map(|k| store.get(k).unwrap().unwrap().as_ref().to_vec()).collect();
        let media = img.layer_media_types();
        let layers: Vec<Layer> =
            layer_bytes.iter().zip(media.iter()).map(|(b, mt)| Layer { media_type: mt, blob: b }).collect();

        // net-up-in-init: the freestanding init brings eth0 up itself (static 10.0.2.15/24) via
        // socket+ioctl, then DIRECT-execs the image's entrypoint. Image-independent — it does NOT depend
        // on the image shipping `ip`/busybox (a minimal image like nginx:alpine lacks it, so a shell
        // `ip …` prelude silently fails, leaving eth0 down). The proven CC-69 `img_netup_in_init` path.
        let cfg = RunConfig {
            argv,
            env: rc.env.clone(),
            workdir: if rc.workdir.is_empty() { "/".into() } else { rc.workdir.clone() },
            uid: 0,
            gid: 0,
            net_up: true,
        };
        let init = image_init(&template, &cfg).ok_or("image run-config too large for the init table")?;
        let rootfs =
            assemble_ext4_bootable(&layers, &init, 768 * 1024 * 1024).map_err(|e| format!("assemble: {e:?}"))?;

        // Content-key the blob by the image config + port; write the .ref sidecar so the next run resumes
        // with no pull.
        let blob_key = {
            let mut ki = cfg_bytes.clone();
            ki.extend_from_slice(&guest_port.to_le_bytes());
            ki.extend_from_slice(cmd_override.join("\u{0}").as_bytes());
            let k = holospaces::oci::sha256_digest(&ki);
            k.strip_prefix("sha256:").unwrap_or(&k).to_string()
        };
        warm_path = warm_dir.join(format!("{blob_key}.holo"));
        if !no_cache {
            let _ = std::fs::write(&ref_path, format!("{blob_key}\n{guest_port}\n"));
        }

        let cmdline = "virtio_mmio.device=0x200@0xd0000400:12 virtio_mmio.device=0x200@0xd0000000:11 \
                       console=ttyS0 root=/dev/vda rw init=/init random.trust_cpu=on norandmaps \
                       nmi_watchdog=0 nowatchdog tsc=reliable";
        cpu = Cpu::boot_linux_disk(1024 * 1024 * 1024, &kernel, rootfs, cmdline);
    }

    // ── host forward (attach fresh on cold boot; re-attach preserving device state on resume) ──
    let mut ingress = StdIngress::new();
    let bound = port_override.unwrap_or(0);
    let host_port = if public {
        ingress.forward_public(bound, guest_port)
    } else {
        ingress.forward(bound, guest_port)
    }
    .map_err(|e| format!("bind host port: {e}"))?;
    if warm {
        cpu.reattach_net_forward(Box::new(NoEgress), Box::new(ingress));
    } else {
        cpu.attach_net_forward(Box::new(NoEgress), Box::new(ingress));
    }

    // ── readiness probe: confirm a REAL response, not just a TCP accept. ──
    // The κ-NAT SYN-ACKs an inbound connection optimistically, so a bare connect is a false positive
    // (it fires long before a slow server like nginx has actually accept()ed and served). Instead send a
    // protocol-appropriate request and require real response bytes back — only then is the app truly
    // serving, safe to declare live AND to snapshot. Redis speaks RESP (PING→PONG); everything else here
    // is probed with a harmless HTTP GET (any response bytes = a real server answered).
    let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Don't probe until the guest network is up (the net-up init prints `HOLO-NET-UP`; on a warm resume
    // it is already serving). Probing a port before the guest listens leaves half-open connections in the
    // κ-NAT's table that can clog the real request path — so wait, like the CC-69 harness does.
    let net_up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(warm));
    let display_host = if public { host_ip() } else { "127.0.0.1".to_string() };
    let img_name = image.clone();
    let is_redis = guest_port == 6379;
    {
        let ready = ready.clone();
        let net_up = net_up.clone();
        std::thread::spawn(move || {
            let (req, want): (&[u8], &[u8]) = if is_redis {
                (b"PING\r\n", b"PONG")
            } else {
                (b"GET / HTTP/1.0\r\nHost: app\r\n\r\n", b"")
            };
            while !net_up.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
            }
            // Give the app a moment after net-up to bind + listen before the first request.
            std::thread::sleep(Duration::from_millis(500));
            for _ in 0..1200 {
                if let Ok(mut s) = TcpStream::connect(("127.0.0.1", host_port)) {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                    if s.write_all(req).is_ok() {
                        let mut resp = Vec::new();
                        let mut chunk = [0u8; 512];
                        while let Ok(n) = s.read(&mut chunk) {
                            if n == 0 {
                                break;
                            }
                            resp.extend_from_slice(&chunk[..n]);
                            if resp.len() > 4096 {
                                break;
                            }
                        }
                        // A real server returned application bytes (HTTP headers / RESP reply). A bare
                        // NAT accept with no listener returns nothing → keep waiting.
                        let served = if want.is_empty() {
                            !resp.is_empty()
                        } else {
                            resp.windows(want.len()).any(|w| w == want)
                        };
                        if served {
                            ready.store(true, std::sync::atomic::Ordering::Relaxed);
                            println!(
                                "\n  ▶  {img_name} is live:  http://{display_host}:{host_port}\n     \
                                 (content-addressed + L5-verified, running on the κ substrate)\n"
                            );
                            return;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            eprintln!("holo run: the app never returned a real response (see the boot console above).");
        });
    }

    if warm {
        eprintln!("holo run: serving {image} from the warm .holo …  (Ctrl-C to stop)");
    } else {
        eprintln!("holo run: cold-booting {image} on the κ substrate …  (Ctrl-C to stop)");
    }
    // Cold path only: once the app has returned a REAL response, snapshot the serving machine and cache
    // it (a short drain lets the probe's connection fully close so no NAT connection is mid-flight), then
    // keep serving. Warm path is already serving.
    let mut snapshotted = warm || no_cache;
    let mut ready_ticks: u32 = 0;
    let mut con_len = cpu.console().len(); // on resume, don't re-echo the snapshot's console
    let stream_console = std::env::var_os("HOLO_RUN_QUIET").is_none();
    let mut net_up_seen = warm;
    loop {
        cpu.run(5_000_000);
        // Stream the guest's console (kernel boot + the app's own stdout/stderr) like `docker run` —
        // set HOLO_RUN_QUIET to suppress. Also the boot-visibility for diagnosing a stuck image.
        let con = cpu.console();
        if stream_console && con.len() > con_len {
            let _ = std::io::stderr().write_all(&con[con_len..]);
        }
        con_len = con.len();
        // Release the readiness probe once the guest network is up (the net-up init's marker) — before
        // then, probing only clogs the κ-NAT with pre-listener connections.
        if !net_up_seen && String::from_utf8_lossy(con).contains("HOLO-NET-UP") {
            net_up.store(true, std::sync::atomic::Ordering::Relaxed);
            net_up_seen = true;
        }
        if !snapshotted && ready.load(std::sync::atomic::Ordering::Relaxed) {
            ready_ticks += 1;
            // Drain after the first real response so the probe's connection fully closes in the guest —
            // snapshotting a server mid-teardown resumes into a non-serving machine (CC-74 lesson).
            if ready_ticks >= 30 {
                let blob = cpu.snapshot_kappa_blob();
                let tmp = warm_path.with_extension("holo.tmp");
                if std::fs::write(&tmp, &blob).and_then(|()| std::fs::rename(&tmp, &warm_path)).is_ok() {
                    eprintln!(
                        "holo run: cached a warm .holo ({} MiB) → next `holo run {image}` resumes in ~1s",
                        blob.len() / (1024 * 1024)
                    );
                }
                snapshotted = true;
            }
        }
    }
}
