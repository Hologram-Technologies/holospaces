//! `CC-46` — **real-kernel** device-bus parity on the AArch64 core: a real arm64
//! Linux boots over the shared [`emulator::devbus`](holospaces::emulator) and a
//! real guest userspace exercises all three substrate devices at the **same
//! caliber as the RISC-V `CC-15`/`CC-16`/`CC-33` rows** (real-OS boots, not a
//! hand-built MMIO driver):
//!
//!   1. **9p workspace (`CC-15` parity):** the booted guest *mounts* the
//!      holospaces-served `virtio-9p` share through its VFS (`mount -t 9p`),
//!      reads the file holospaces seeded, and writes a file back that holospaces
//!      reads — one shared content (Law L1).
//!   2. **network (`CC-16` parity):** the guest's real TCP/IP stack *opens* an
//!      outbound TCP/HTTP flow over `virtio-net` through the userspace NAT, out
//!      the native egress to a real host server.
//!   3. **bridge (`CC-33` parity):** a real server inside the guest (busybox
//!      `httpd` on `:8080`) is reached from the host over the in-process
//!      bridge — `dial_guest` → `guest_send`/`guest_recv`, no socket, no relay.
//!
//! Law L4: the devices are the *one* shared devbus the RISC-V machine uses; only
//! the MMIO transport differs (AArch64 GIC vs RISC-V PLIC). The differential
//! oracle is `qemu-system-aarch64 -M virt` on the same kernel + rootfs + the
//! `10.0.2.0/24` user-mode NAT model. Fixtures live in `vv/artifacts/cc46`
//! (`build.sh` + `cc46.sha256`, recorded in `vv/PROVENANCE.md`).
//!
//! Heavy (a real arm64 boot), so `#[ignore]`d in the default test set and run by
//! the `CC-46` vv suite. The κ-disk is paged; κ-caching/resume (`CC-30`/`CC-31`,
//! `CC-42`) amortizes the boot so the witness is feasible in a normal budget.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::aarch64::{Cpu, Halt};
use holospaces::emulator::net::{NoIngress, StdEgress};

fn cc46_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc46")
}

fn gunzip(path: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(path).expect("read gz")[..])
        .read_to_end(&mut out)
        .expect("gunzip");
    out
}

/// Assemble the arm64 devbus-parity rootfs: the **stock `linux-arm64` busybox**
/// layer overlaid into an `ext4` image, with the CC-46 `/init` injected (mounts
/// the 9p workspace, fetches over the NAT, and serves a bridge listener).
fn assemble_rootfs() -> Vec<u8> {
    let init = std::fs::read(cc46_dir().join("init.sh")).expect("cc46 init.sh");
    let layer = std::fs::read(cc46_dir().join("rootfs/layer.tar.gz")).expect("cc46 busybox layer");
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
        blob: &layer,
    }];
    assemble_ext4_bootable(&layers, &init, 64 * 1024 * 1024).expect("assemble the arm64 rootfs")
}

/// The flagship `CC-46` real-boot witness: a real arm64 Linux boots over the
/// shared devbus and a real guest userspace exercises 9p + net + the bridge.
#[test]
#[ignore = "boots a real arm64 Linux over the shared devbus (~release) — run by the CC-46 vv suite"]
fn the_aarch64_core_serves_9p_net_and_bridge_to_a_real_arm64_boot() {
    // ── A real host server the guest's outbound flow must reach (CC-16) ──────
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a host server");
    let host_port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf); // the guest's HTTP request line
                                         // busybox wget reads the body; serve the marker as the response body.
            let _ = sock
                .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 22\r\n\r\nHELLO-FROM-HOST-SERVER");
            let _ = sock.flush();
        }
    });

    // The guest fetches http://10.0.2.9:7777/ ; the native egress port-forwards
    // that NAT target to the host server (slirp's `guestfwd`, the CC-16 model).
    let egress = StdEgress::new().redirect([10, 0, 2, 9], 7777, "127.0.0.1", host_port);

    // ── Boot a real arm64 Linux over the shared devbus with all three devices ─
    let kernel = gunzip(&cc46_dir().join("linux/Image.gz"));
    let rootfs = assemble_rootfs();
    let seed: &[(&str, &[u8])] = &[("from-holospaces.txt", b"from-holospaces-9p-share-OK")];
    let mut cpu = Cpu::boot_linux_devbus(
        512 * 1024 * 1024,
        &kernel,
        rootfs,
        seed,
        Box::new(egress),
        Box::new(NoIngress),
        "console=ttyAMA0 root=/dev/vda rw ip=dhcp init=/init",
    );
    // The in-process bridge (CC-33): enabled on the booted net device.
    assert!(
        cpu.enable_loopback(),
        "the net device exposes the in-process loopback bridge (CC-33)"
    );

    // Run until the guest server is listening (9p + net happen on the way there).
    let mut listening = false;
    for _ in 0..4000 {
        if matches!(cpu.run(20_000_000), Halt::Exit(_)) {
            break;
        }
        if String::from_utf8_lossy(cpu.console()).contains("CC46-SERVER-LISTENING") {
            listening = true;
            break;
        }
    }
    let console = String::from_utf8_lossy(cpu.console()).into_owned();
    eprintln!("---- guest console ----\n{console}\n---- end ----");
    let _ = server.join();

    // The arm64 devcontainer booted over the shared devbus.
    assert!(
        console.contains("CC46-DEVCONTAINER-UP") && console.contains("CC46-ARCH:aarch64"),
        "the arm64 devcontainer booted over the shared devbus; console:\n{console}"
    );

    // 1) CC-15 parity: the guest mounted the 9p workspace and round-tripped a file.
    assert!(
        console.contains("CC46-9P-MOUNTED"),
        "the guest mounted the holospaces virtio-9p workspace through its VFS (CC-15); \
         console:\n{console}"
    );
    assert!(
        console.contains("CC46-9P-READ:from-holospaces-9p-share-OK"),
        "the guest read the file holospaces seeded on the 9p share (CC-15); console:\n{console}"
    );
    assert_eq!(
        cpu.workspace_file("from-guest.txt"),
        Some(&b"GUEST-WROTE-THIS\n"[..]),
        "holospaces reads back the file the guest wrote over 9p — one content, Law L1 (CC-15)"
    );

    // 2) CC-16 parity: the guest opened an outbound TCP/HTTP flow through the NAT.
    assert!(
        console.contains("CC46-NET-RECV:HELLO-FROM-HOST-SERVER"),
        "the guest's TCP/IP stack completed an outbound flow over virtio-net through the \
         userspace NAT and read the host server's reply (CC-16); console:\n{console}"
    );

    // 3) CC-33 parity: the guest's real listener is reachable over the bridge.
    assert!(
        listening,
        "the guest's httpd bound and listened (CC-33 setup); console:\n{console}"
    );
    let id = cpu
        .dial_guest(8080)
        .expect("the loopback bridge is enabled, so dialing the guest listener returns an id");
    // Pump so the NAT opens the connection toward the guest (SYN / SYN-ACK).
    for _ in 0..40 {
        cpu.run(2_000_000);
    }
    cpu.guest_send(id, b"GET / HTTP/1.0\r\nHost: app\r\n\r\n");
    let mut resp: Vec<u8> = Vec::new();
    for _ in 0..2000 {
        cpu.run(2_000_000);
        resp.extend(cpu.guest_recv(id));
        if resp.windows(23).any(|w| w == b"HELLO-FROM-GUEST-SERVER") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&resp).into_owned();
    assert!(
        text.contains("HELLO-FROM-GUEST-SERVER"),
        "the host reached the real guest server over the in-process bridge and read its reply \
         (CC-33); got:\n{text:?}\nconsole:\n{console}"
    );
    cpu.guest_close(id);
}
