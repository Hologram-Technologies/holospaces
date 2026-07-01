//! NATIVE XFCE **resume** proof — boot the graphical Alpine kernel + XFCE rootfs to first paint, take a
//! self-contained κ-snapshot blob, restore it into a FRESH `Cpu`, and prove the restored machine is the
//! live painted desktop: its framebuffer matches byte-for-byte, it keeps executing, and it still takes
//! input (an injected pointer motion changes the framebuffer). This is the "instant resume" gate — the
//! enabler for "very very fast in any browser" (a tab resumes the painted desktop instead of cold-booting
//! for guest-minutes). Builds on the proven boot path (`xfce_x64_boot.rs`).
//!
//! Run:
//!   cargo test -p holospaces --release --test xfce_x64_resume xfce_resumes_natively -- --ignored --nocapture

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::x64::{Cpu, Halt};

fn web(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../holospaces-web/web").join(rel)
}

fn gunzip(p: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(p).expect("read gz")[..])
        .read_to_end(&mut out)
        .expect("gunzip");
    out
}

// The DECORATED bring-up: everything the proven boot did, PLUS the full desktop scene — panel (seeded
// from default.xml), Adwaita-dark GTK + adwaita-xfce icons, a wallpaper on the real RandR output, and an
// autostart that opens xfce4-terminal running fastfetch. Baking the snapshot AFTER this scene renders
// means resuming reproduces the reference screenshot (themed panel + wallpaper + terminal) instantly.
const INIT: &str = r#"#!/bin/sh
B=/bin/busybox
S=/dev/ttyS0
$B mkdir -p /proc /sys /dev /run /tmp /root /var/run /var/log /var/lib/dbus /etc
$B mount -t proc proc /proc
$B mount -t sysfs sysfs /sys
$B mount -t devtmpfs devtmpfs /dev 2>/dev/null
$B mount -t tmpfs tmpfs /run 2>/dev/null
$B mount -t tmpfs tmpfs /tmp 2>/dev/null
$B --install -s 2>$S
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
$B mkdir -p /run/user/0
$B chmod 700 /run/user/0
export HOME=/root TERM=xterm-256color XDG_RUNTIME_DIR=/run/user/0 DISPLAY=:0
export GTK_THEME=Adwaita:dark NO_AT_BRIDGE=1 GTK_A11Y=none GSETTINGS_SCHEMA_DIR=/usr/share/glib-2.0/schemas
$B echo HOLO-XFCE-INIT mounted > $S
$B mkdir -p /dev/dri
$B mknod /dev/dri/card0 c 226 0 2>$S
$B mknod /dev/fb0 c 29 0 2>$S
$B mknod /dev/tty0 c 4 0 2>$S
$B mknod /dev/tty1 c 4 1 2>$S
$B mknod /dev/tty c 5 0 2>$S
$B echo HOLO-NODES-MADE > $S
$B cat /proc/sys/kernel/random/boot_id 2>/dev/null | $B tr -d - > /etc/machine-id
$B echo shortname > /proc/sys/kernel/hostname
$B mkdir -p /etc/X11/xorg.conf.d
$B printf 'Section "Device"\n Identifier "card0"\n Driver "modesetting"\n Option "kmsdev" "/dev/dri/card0"\n Option "AccelMethod" "none"\nEndSection\nSection "Screen"\n Identifier "scr"\n Device "card0"\nEndSection\nSection "ServerFlags"\n Option "DontVTSwitch" "true"\nEndSection\n' > /etc/X11/xorg.conf.d/10-fbdev.conf
$B rm -f /etc/X11/xorg.conf
# Seed the XFCE scene config so the desktop comes up DECORATED (no first-start dialogs).
$B mkdir -p /root/.config/xfce4/xfconf/xfce-perchannel-xml /root/.config/autostart
$B cp /etc/xdg/xfce4/panel/default.xml /root/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml
$B printf '<?xml version="1.0" encoding="UTF-8"?>\n<channel name="xsettings" version="1.0">\n <property name="Net" type="empty">\n  <property name="ThemeName" type="string" value="Adwaita"/>\n  <property name="IconThemeName" type="string" value="adwaita-xfce"/>\n </property>\n <property name="Gtk" type="empty">\n  <property name="FontName" type="string" value="Sans 10"/>\n  <property name="MonospaceFontName" type="string" value="Monospace 11"/>\n </property>\n</channel>\n' > /root/.config/xfce4/xfconf/xfce-perchannel-xml/xsettings.xml
$B cat > /root/holo-scene.sh <<'SCENE'
#!/bin/sh
exec > /dev/ttyS0 2>&1
echo "=SCENE= start"
sleep 5
echo "=SCENE= xrandr:"; xrandr --listmonitors 2>&1 || echo "=SCENE= xrandr FAILED"
O=$(xrandr --listmonitors 2>/dev/null | awk 'NR==2{print $NF}')
echo "=SCENE= monitor=[$O]"
xfconf-query -c xfce4-desktop -p /backdrop/screen0/monitor$O/workspace0/last-image -n -t string -s /usr/share/backgrounds/xfce/xfce-teal.jpg; echo "=SCENE= wallpaper($O) rc=$?"
xfconf-query -c xfce4-desktop -p /backdrop/screen0/monitor$O/workspace0/image-style -n -t int -s 5
xfconf-query -c xfce4-desktop -p /backdrop/screen0/monitor0/workspace0/last-image -n -t string -s /usr/share/backgrounds/xfce/xfce-teal.jpg; echo "=SCENE= wallpaper(monitor0) rc=$?"
xfdesktop --reload; echo "=SCENE= xfdesktop-reload rc=$?"
sleep 2
echo "=SCENE= launching terminal"
xfce4-terminal --geometry=104x30 --command="sh -c 'fastfetch; exec sh'" &
echo "=SCENE= terminal launched pid=$!"
SCENE
$B chmod +x /root/holo-scene.sh
$B printf '[Desktop Entry]\nType=Application\nName=holo-scene\nExec=/bin/sh /root/holo-scene.sh\nX-GNOME-Autostart-enabled=true\n' > /root/.config/autostart/holo-scene.desktop
/sbin/udevd --daemon > $S 2>&1
udevadm trigger --action=add > $S 2>&1
udevadm settle --timeout=10 > $S 2>&1
$B echo HOLO-UDEV-SETTLED > $S
$B echo HOLO-STARTX > $S
Xorg :0 -keeptty -nolisten tcp > $S 2>&1 &
$B sleep 6
export DISPLAY=:0
$B echo HOLO-XORG-UP > $S
dbus-daemon --session --address=unix:path=/run/user/0/bus --nofork --nopidfile > $S 2>&1 &
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus
$B sleep 2
$B echo HOLO-DBUS-UP > $S
glib-compile-schemas /usr/share/glib-2.0/schemas > $S 2>&1
gtk-update-icon-cache -f -t /usr/share/icons/hicolor > $S 2>&1
gtk-update-icon-cache -f -t /usr/share/icons/adwaita-xfce > $S 2>&1
$B echo HOLO-STARTXFCE4 > $S
exec startxfce4 > $S 2>&1
"#;

fn fb_nonzero(cpu: &Cpu) -> usize {
    cpu.read_framebuffer().iter().step_by(16).filter(|&&b| b != 0).count()
}

fn hexname(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

#[test]
#[ignore = "native: boot XFCE to paint, snapshot, resume into a fresh Cpu, prove it's the live desktop"]
fn xfce_resumes_natively() {
    let kernel = gunzip(&web("graphical-x64-kernel.gz"));
    let layer = std::fs::read(web("alpine-amd64-xfce-layer.tar.gz")).expect("read xfce layer");
    // Overlay layer: fastfetch + adwaita-xfce icon theme (spliced from Alpine 3.21 .apks). Applied on
    // top of the base so the scene has the terminal readout + proper icons without touching the base.
    let extras = std::fs::read(web("xfce-extras-layer.tar.gz")).expect("read xfce extras layer");
    eprintln!("kernel {} KiB, base {} MiB + extras {} KiB — assembling 768 MiB ext4…",
        kernel.len() / 1024, layer.len() / (1024 * 1024), extras.len() / 1024);

    let rootfs = assemble_ext4_bootable(
        &[
            Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer },
            Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &extras },
        ],
        INIT.as_bytes(),
        768 * 1024 * 1024,
    )
    .expect("assemble the bootable Xfce ext4");

    // Keep a copy of the freshly-assembled disk: it's the baseline the BROWSER rebuilds from the
    // shipped layer (assemble is deterministic → identical sector κ). The shippable disk artifact is
    // then only the DELTA — sectors the boot MODIFIED (machine-id, xorg.conf.d, gschemas.compiled, …).
    let rootfs_probe = rootfs.clone();
    // 1 GiB guest RAM (not 2): the browser peer runs in wasm32 with a 4 GiB address-space ceiling, so
    // the resumed guest RAM + emulator + page/disk maps must fit well under 4 GiB. Idle XFCE needs a
    // few hundred MiB, so 1 GiB is ample — and it shrinks the RAM working set the artifact ships.
    let mut cpu = Cpu::boot_linux_disk(
        1024 * 1024 * 1024,
        &kernel,
        rootfs,
        "console=ttyS0 console=tty0 virtio_mmio.device=0x200@0xd0000000:11 virtio_mmio.device=0x200@0xd0000600:5 root=/dev/vda rw init=/init random.trust_cpu=on norandmaps",
    );
    cpu.attach_virtio_input();

    // ── Phase 1: cold-boot to the FULL decorated scene (backdrop → panel → wallpaper → terminal +
    // fastfetch). After first paint, nudge input each chunk so X keeps servicing + flushing while the
    // late components (panel, autostart terminal) map, and dump intermediate frames so the scene is
    // inspectable. Snapshot once the scene has had time to fully build. ───────────────────────────
    const CHUNK: u64 = 200_000_000;
    const MAX_CHUNKS: u32 = 320;
    const AFTER_PAINT: u32 = 160; // chunks to keep running past first paint so the scene completes
    let scene_ppm = |cpu: &Cpu, tag: &str| {
        let fb = cpu.read_framebuffer();
        let mut ppm = format!("P6\n{} {}\n255\n", Cpu::FB_W, Cpu::FB_H).into_bytes();
        for px4 in fb.chunks_exact(4) { ppm.push(px4[2]); ppm.push(px4[1]); ppm.push(px4[0]); }
        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../xfce-scene-{tag}.ppm"));
        let _ = std::fs::write(out, &ppm);
    };
    let mut paint_chunk: Option<u32> = None;
    for c in 0..MAX_CHUNKS {
        let halt = cpu.run(CHUNK);
        if !matches!(halt, Halt::OutOfBudget) {
            eprintln!("[chunk {c}] unexpected HALT: {halt:?}");
            if let Halt::Undefined(rip) = halt {
                let bytes = cpu.peek_code(rip, 16);
                let hex: String = bytes.iter().map(|b| format!("{b:02x} ")).collect();
                eprintln!("MISSING INSTRUCTION @ rip={rip:#x}: {}", hex.trim_end());
            }
            break;
        }
        // Once painted, keep the X event loop busy (drives the shadow→scanout flush as the scene builds).
        if paint_chunk.is_some() {
            cpu.input_motion(3, 2);
            cpu.input_motion(-2, -1);
        }
        let px = fb_nonzero(&cpu);
        if c % 10 == 9 { eprintln!("[chunk {c}] fb_nonzero={px}"); }
        if paint_chunk.is_none() && px > 40_000 {
            eprintln!("[chunk {c}] *** DESKTOP PAINTED *** fb_nonzero={px} — building the scene…");
            paint_chunk = Some(c);
        }
        if let Some(pc) = paint_chunk {
            if (c - pc) % 20 == 0 && c > pc {
                scene_ppm(&cpu, &format!("c{c}"));
                eprintln!("[chunk {c}] scene frame dumped (fb_nonzero={px}, +{} past paint)", c - pc);
            }
            if c >= pc + AFTER_PAINT { eprintln!("[chunk {c}] scene settled — snapshotting"); break; }
        }
    }
    assert!(paint_chunk.is_some(), "desktop must paint before the resume can be proven");
    let saved_fb = cpu.read_framebuffer();
    let saved_px = fb_nonzero(&cpu);
    scene_ppm(&cpu, "final");

    // ── Phase 2: snapshot the painted desktop into a self-contained κ blob ───────────────────────
    let t_snap = Instant::now();
    let blob = cpu.snapshot_kappa_blob();
    eprintln!("snapshot: {} KiB blob in {:?} (RAM dedups → far below 2 GiB)", blob.len() / 1024, t_snap.elapsed());

    // ── Phase 3: resume into a FRESH machine and prove it IS the painted desktop ─────────────────
    let mut b = Cpu::new(64 * 1024); // tiny; restore resizes RAM to match
    let t_resume = Instant::now();
    assert!(b.restore_kappa_blob(&blob), "resume from the κ blob");
    let resume_ms = t_resume.elapsed();
    eprintln!("RESUME took {resume_ms:?}");

    let restored_fb = b.read_framebuffer();
    assert_eq!(restored_fb.len(), saved_fb.len(), "framebuffer dims match");
    assert_eq!(restored_fb, saved_fb, "restored framebuffer == the painted desktop (byte-for-byte)");
    eprintln!("restored FB == painted FB ✓ (fb_nonzero={})", fb_nonzero(&b));

    // ── Phase 4: prove the resumed desktop is LIVE — it keeps executing AND takes input ──────────
    // Run a little, then inject a large pointer motion; a live X session moves the cursor → the FB
    // changes. (A static restored image would never change.)
    let before = b.read_framebuffer();
    for _ in 0..6 {
        let _ = b.run(60_000_000);
        b.input_motion(80, 40);
        b.input_motion(-30, 70);
    }
    let after = b.read_framebuffer();
    let changed = before.iter().zip(after.iter()).filter(|(x, y)| x != y).count();
    eprintln!("after resume + injected motion: {changed} framebuffer bytes changed, fb_nonzero={}", fb_nonzero(&b));
    assert!(changed > 0, "the resumed desktop must keep executing and respond to input (cursor moves)");

    // ── Phase 5: produce the SHIPPABLE streaming artifact (manifest + dedup'd pages by κ) ───────
    // This is what the browser resumes (web/snap/), so a tab never cold-boots. RAM pages dedup into
    // the store (post-boot RAM is mostly zero/dup); only the unique pages are written.
    // Ship ONLY: (1) a streaming-disk manifest (disk = κ index, not inlined → small), (2) unique RAM
    // pages packed into one indexed blob, (3) the disk DELTA (sectors the boot modified, absent from
    // the layer-rebuilt disk). The browser rebuilds the rest of the disk from the shipped layer.
    use hologram_store_mem::MemKappaStore;
    use hologram_substrate_core::KappaStore;
    use std::collections::HashMap;
    use std::io::Write as _;

    let snap_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../holospaces-web/web/snap");
    std::fs::create_dir_all(&snap_dir).expect("mkdir web/snap");

    // (1) Small manifest: disk streamed (κ index only), RAM as a per-page κ list.
    let manifest = cpu.snapshot_kappa_manifest_streaming_disk();
    std::fs::write(snap_dir.join("desktop.manifest"), &manifest).expect("write manifest");

    // (2) RAM pages → pages.bin (concatenated unique pages) + pages.idx (the κ string per page, in
    //     pages.bin order). snapshot_kappa(&store) computes the SAME ram_pages κ list + stores bytes.
    let store = MemKappaStore::new();
    let snap = cpu.snapshot_kappa(&store).expect("κ-snapshot into store");
    let mut ram_map: HashMap<[u8; 71], Vec<u8>> = HashMap::new();
    let mut pages_bin = std::fs::File::create(snap_dir.join("pages.bin")).expect("create pages.bin");
    let mut pages_idx = String::new();
    let mut npages = 0usize;
    let mut page_bytes = 0usize;
    for k in snap.page_kappas() {
        if let std::collections::hash_map::Entry::Vacant(e) = ram_map.entry(*k.as_array()) {
            let page = store.get(k).expect("store.get").expect("page present").to_vec();
            pages_bin.write_all(&page).expect("write page");
            pages_idx.push_str(k.as_str());
            pages_idx.push('\n');
            npages += 1;
            page_bytes += page.len();
            e.insert(page);
        }
    }
    drop(pages_bin);
    std::fs::write(snap_dir.join("pages.idx"), &pages_idx).expect("write pages.idx");

    // (3) Disk DELTA: baseline = a fresh disk from the same rootfs (what the browser rebuilds from the
    //     layer → identical sector κ); ship only sectors whose κ is NOT in that baseline.
    let mut probe = Cpu::new(1 << 20);
    probe.attach_disk(rootfs_probe);
    let orig: HashMap<[u8; 71], Vec<u8>> = probe.disk_unique_sectors().into_iter().collect();
    let current = cpu.disk_unique_sectors();
    let mut delta_bin = std::fs::File::create(snap_dir.join("disk-delta.bin")).expect("create delta");
    let mut delta_idx = String::new();
    let mut delta: HashMap<[u8; 71], Vec<u8>> = HashMap::new();
    let mut dn = 0usize;
    let mut dbytes = 0usize;
    for (k, bytes) in &current {
        if !orig.contains_key(k) {
            delta_bin.write_all(bytes).expect("write delta sector");
            delta_idx.push_str(&format!("{}:{}\n", hexname(k), bytes.len()));
            delta.insert(*k, bytes.clone());
            dn += 1;
            dbytes += bytes.len();
        }
    }
    drop(delta_bin);
    std::fs::write(snap_dir.join("disk-delta.idx"), &delta_idx).expect("write delta idx");

    eprintln!(
        "artifact -> {}\n  manifest {} KiB | RAM {npages} pages ({} MiB) | disk: {} unique sectors, DELTA {dn} ({} KiB)",
        snap_dir.display(), manifest.len() / 1024, page_bytes / (1024 * 1024), current.len(), dbytes / 1024,
    );

    // (4) VERIFY a BROWSER-STYLE resume: RAM from pages, disk from (layer-rebuilt ORIGINAL ∪ DELTA).
    let mut disk_map = orig;
    for (k, v) in &delta {
        disk_map.insert(*k, v.clone());
    }
    let mut c = Cpu::new(64 * 1024);
    let disk_fetch = Box::new(move |k: &[u8; 71]| disk_map.get(k).cloned());
    let ok = c.restore_kappa_streaming_lazy_disk(&manifest, |k| ram_map.get(k).cloned(), disk_fetch);
    assert!(ok, "browser-style streaming resume (RAM pages + layer disk + delta)");
    assert_eq!(c.read_framebuffer(), saved_fb, "browser-style resume == painted desktop");
    // Prove live: run + inject motion → FB changes (also faults disk sectors in through the lazy backing).
    let pre = c.read_framebuffer();
    for _ in 0..6 {
        let _ = c.run(60_000_000);
        c.input_motion(70, 30);
    }
    let moved = pre.iter().zip(c.read_framebuffer().iter()).filter(|(a, b)| a != b).count();
    assert!(moved > 0, "resumed desktop runs + takes input");
    eprintln!("browser-style resume == painted FB ✓; live: {moved} FB bytes changed after input");

    eprintln!("\n==== XFCE RESUME RESULT ====");
    eprintln!("paint fb_nonzero={saved_px}; blob {} KiB; RESUME {resume_ms:?}; live+input ✓", blob.len() / 1024);
    eprintln!("SHIPPABLE artifact: manifest {} KiB + RAM {npages}p/{} MiB + disk-delta {dn}sec/{} KiB (browser rebuilds disk from the layer) ✓",
        manifest.len() / 1024, page_bytes / (1024 * 1024), dbytes / 1024);
    eprintln!("============================");
}
