//! NATIVE XFCE bring-up harness — boots the real graphical amd64 Alpine kernel + the
//! Xfce 4 rootfs over an in-memory κ-disk on the x86-64 core, NATIVELY (no browser/OPFS),
//! and reports exactly where it stops: a `Halt::Undefined(rip)` (a missing instruction —
//! the opcode bytes are dumped so it can be implemented), a clean power-off, or a budget
//! exhaustion (still running). This is the feedback loop for closing the remaining Xorg/
//! pixman SSE2 gaps without round-tripping through a real browser tab.
//!
//! Run:
//!   cargo test -p holospaces --release --test xfce_x64_boot xfce_boots_natively -- --ignored --nocapture

use std::io::Read;
use std::path::{Path, PathBuf};

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

// PID 1 — the same bring-up sequence the browser worker injects: mount the pseudo-fs,
// install busybox applets, make the DRM/fb device nodes, then `startx` → startxfce4.
// Every milestone echoes to /dev/ttyS0 (the serial we read) so a hang/halt is pinpointed
// by the last marker seen.
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
export HOME=/root TERM=linux XDG_RUNTIME_DIR=/run/user/0 DISPLAY=:0
$B echo HOLO-XFCE-INIT mounted > $S
$B ls /sys/class/drm /sys/class/graphics > $S 2>&1
$B mkdir -p /dev/dri
$B mknod /dev/dri/card0 c 226 0 2>$S
$B mknod /dev/fb0 c 29 0 2>$S
$B mknod /dev/tty0 c 4 0 2>$S
$B mknod /dev/tty1 c 4 1 2>$S
$B mknod /dev/tty c 5 0 2>$S
$B echo HOLO-NODES-MADE > $S
$B cat /proc/sys/kernel/random/boot_id 2>/dev/null | $B tr -d - > /etc/machine-id
$B echo localhost > /proc/sys/kernel/hostname
$B mkdir -p /etc/X11/xorg.conf.d
$B printf 'Section "Device"\n Identifier "card0"\n Driver "modesetting"\n Option "kmsdev" "/dev/dri/card0"\n Option "AccelMethod" "none"\nEndSection\nSection "Screen"\n Identifier "scr"\n Device "card0"\nEndSection\nSection "ServerFlags"\n Option "DontVTSwitch" "true"\nEndSection\n' > /etc/X11/xorg.conf.d/10-fbdev.conf
$B echo HOLO-XORGCONF-SET > $S
$B echo ===EXISTING-XORGCONF=== > $S
$B cat /etc/X11/xorg.conf > $S 2>&1
$B rm -f /etc/X11/xorg.conf
# Bring up udev so the X server's libinput backend can use the virtio-input device.
# The kernel's evdev created /dev/input/eventN, but this xf86-input-libinput build
# uses the *udev* backend and refuses a device udev hasn't initialized ("udev device
# never initialized"). Run udevd + trigger + settle to populate the udev db (ID_INPUT*
# properties, libinput device-group), then X auto-adds it (AutoAddDevices defaults on).
$B ls -l /dev/input > $S 2>&1
/sbin/udevd --daemon > $S 2>&1
udevadm trigger --action=add > $S 2>&1
udevadm settle --timeout=10 > $S 2>&1
$B echo HOLO-UDEV-SETTLED > $S
udevadm info /dev/input/event0 > $S 2>&1
# Real component launch (the spinning path). With HOLO_SYSTRACE, [CLONE] lines reveal how many
# threads the components spawn — testing whether the over-fill is a concurrent-insert race.
$B echo HOLO-STARTX > $S
Xorg :0 -keeptty -nolisten tcp > $S 2>&1 &
$B sleep 6
export DISPLAY=:0
$B echo HOLO-XORG-UP > $S
dbus-daemon --session --address=unix:path=/run/user/0/bus --nofork --nopidfile > $S 2>&1 &
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus
$B sleep 2
$B echo HOLO-DBUS-UP > $S
# Compile the GSettings schemas (the layer ships the .xml but no gschemas.compiled → GTK aborts with
# "Cannot get the default GSettingsSchemaSource"), and disable the a11y bus (it crashes, signal 5).
glib-compile-schemas /usr/share/glib-2.0/schemas > $S 2>&1
$B echo HOLO-SCHEMAS-COMPILED > $S
# Build the hicolor icon cache so GTK can resolve named icons (the layer ships hicolor with no
# icon-theme.cache → panel/xfdesktop spam "Unable to find fallback icon"). Quiets the icon errors.
gtk-update-icon-cache -f -t /usr/share/icons/hicolor > $S 2>&1
$B echo HOLO-ICONCACHE-BUILT > $S
export NO_AT_BRIDGE=1
export GTK_A11Y=none
export GSETTINGS_SCHEMA_DIR=/usr/share/glib-2.0/schemas
# DIAGNOSE WHY NO PAINT: run the full session, then dump every process's blocking state (comm + wchan
# = the kernel fn it's sleeping in). poll/ep_poll = waiting for X/dbus events (input/flush); futex =
# a lock; pipe/socket = a stuck handshake. This says whether input is the fix or it's something else.
# Launch the full session and hand control to the harness (no guest-time sleep gate): the harness
# grinds its instruction budget so the components get MAXIMAL real CPU to finish init + paint.
$B echo HOLO-STARTXFCE4 > $S
# Route the session log straight to serial so component errors (panel/xfdesktop) are visible to the
# harness — diagnosing why the panel/wallpaper don't yet show.
exec startxfce4 > $S 2>&1
"#;

#[test]
#[ignore = "native: boot the graphical Alpine kernel + Xfce rootfs and report where it stops"]
fn xfce_boots_natively() {
    let kernel = gunzip(&web("graphical-x64-kernel.gz"));
    let layer = std::fs::read(web("alpine-amd64-xfce-layer.tar.gz")).expect("read xfce layer");
    eprintln!("kernel {} KiB, xfce layer {} MiB — assembling 768 MiB ext4…",
        kernel.len() / 1024, layer.len() / (1024 * 1024));

    let rootfs = assemble_ext4_bootable(
        &[Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer }],
        INIT.as_bytes(),
        768 * 1024 * 1024,
    )
    .expect("assemble the bootable Xfce ext4");
    eprintln!("assembled rootfs {} MiB — booting…", rootfs.len() / (1024 * 1024));

    let mut cpu = Cpu::boot_linux_disk(
        2048 * 1024 * 1024,
        &kernel,
        rootfs,
        // Fourth virtio-mmio slot = the virtio-input device (keyboard + pointer) at
        // 0xd0000600, IRQ 5 — the kernel binds CONFIG_VIRTIO_INPUT and evdev exposes it.
        "console=ttyS0 console=tty0 virtio_mmio.device=0x200@0xd0000000:11 virtio_mmio.device=0x200@0xd0000600:5 root=/dev/vda rw init=/init random.trust_cpu=on norandmaps",
    );
    // Attach the virtio-input device before the first run so it is present when the
    // kernel probes the virtio-mmio bus during early boot.
    cpu.attach_virtio_input();

    // Run in chunks; stop on any non-budget halt; cap total work so a stuck boot ends.
    const CHUNK: u64 = 200_000_000;
    const MAX_CHUNKS: u32 = 250; // ~50B guest insns ceiling (bounds wall-clock)
    let mut halt = Halt::OutOfBudget;
    let mut last_len = 0usize;
    let mut stall_page = 0u64;
    let mut stall_count = 0u32;
    let mut max_px = 0usize;
    let mut xfce_seen = false;
    let mut paint_after_input = false;
    let mut paint_chunk: Option<u32> = None;
    for c in 0..MAX_CHUNKS {
        halt = cpu.run(CHUNK);
        let con = String::from_utf8_lossy(cpu.console());
        if con.len() != last_len {
            // only print the freshly-produced serial so the log tracks progress
            eprint!("{}", &con[last_len..]);
            last_len = con.len();
        }
        if !matches!(halt, Halt::OutOfBudget) {
            eprintln!("\n[chunk {c}] HALT: {halt:?}");
            break;
        }
        // Stop when the init's final marker prints, or when the desktop actually PAINTS. Do NOT
        // match "xfce4-session" — it appears in dbus activation logs and would bail mid-startup.
        let con_now = String::from_utf8_lossy(cpu.console());
        if con_now.contains("HOLO-DIAG-END") {
            eprintln!("\n[chunk {c}] reached HOLO-DIAG-END — stopping");
            break;
        }
        // Once the session launches, drive the virtio-input device: wiggle the pointer
        // and tap a key/click each chunk. Delivering events wakes the X server's main
        // loop → its block handler runs the modesetting shadow→scanout flush. If the FB
        // goes nonzero only after this begins, input was the missing flush driver.
        if !xfce_seen && con_now.contains("HOLO-STARTXFCE4") {
            xfce_seen = true;
            eprintln!("[chunk {c}] session launched — beginning virtio-input injection");
        }
        if xfce_seen {
            cpu.input_motion(9, 6);
            cpu.input_motion(-5, -8);
            if c % 3 == 0 {
                cpu.input_key(0x110, true); // BTN_LEFT down
                cpu.input_key(0x110, false); // BTN_LEFT up
            }
            if c % 5 == 0 {
                cpu.input_key(28, true); // KEY_ENTER
                cpu.input_key(28, false);
            }
        }
        // Track the framebuffer every chunk (boot text ~17.8k px first; Xorg clears to ~0; then the
        // desktop fills it). A desktop fills FAR more than the sparse boot text → break on a clear
        // paint (>40k) so we capture it. max_px tracks the peak for the final report.
        let px = cpu.read_framebuffer().iter().step_by(16).filter(|&&b| b != 0).count();
        if px > max_px { max_px = px; }
        if c % 3 == 2 { eprintln!("[chunk {c}] fb_nonzero={px} (max {max_px}) xfce={xfce_seen}"); }
        if xfce_seen && !paint_after_input && px > 2000 {
            paint_after_input = true;
            eprintln!("[chunk {c}] *** FB rose to {px} AFTER input injection began — flush driven by input ***");
        }
        if px > 40_000 && paint_chunk.is_none() {
            paint_chunk = Some(c);
            eprintln!("\n[chunk {c}] *** DESKTOP PAINTED *** fb_nonzero={px} — running 40 more chunks so the panel/wallpaper can render");
        }
        // After first paint, keep running a while so late-mapping components (xfce4-panel) draw, then stop.
        if let Some(pc) = paint_chunk {
            // Dump an intermediate FB every 30 chunks after paint so we can watch the panel appear
            // without waiting for the full window (the desktop inits over guest-minutes).
            if (c - pc) % 30 == 0 && c > pc {
                let fb = cpu.read_framebuffer();
                let mut ppm = format!("P6\n{} {}\n255\n", Cpu::FB_W, Cpu::FB_H).into_bytes();
                for px4 in fb.chunks_exact(4) { ppm.push(px4[2]); ppm.push(px4[1]); ppm.push(px4[0]); }
                let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../xfce-fb-c{c}.ppm"));
                let _ = std::fs::write(&out, &ppm);
                eprintln!("[chunk {c}] intermediate FB dumped (fb_nonzero={px})");
            }
            if c >= pc + 150 {
                eprintln!("\n[chunk {c}] stopping {} chunks after first paint (fb_nonzero={px})", c - pc);
                break;
            }
        }
        // Stall detector: if the guest spins in one 4 KiB code page with a black FB for a
        // long stretch, it's blocked (a poll/futex busy-wait) — dump the loop + serial and stop.
        let rip = cpu.rip();
        let page = rip & !0xfff;
        if page == stall_page { stall_count += 1; } else { stall_page = page; stall_count = 0; }
        if c % 10 == 9 {
            let fb = cpu.read_framebuffer();
            let px = fb.iter().step_by(16).filter(|&&b| b != 0).count();
            let code = cpu.peek_code(rip.wrapping_sub(16), 40);
            let hex: String = code.iter().map(|b| format!("{b:02x} ")).collect();
            eprintln!("\n[chunk {c}] rip={rip:#x} fb={px} stall={stall_count}\n  code[rip-16..+24]: {}", hex.trim_end());
        }
        if stall_count >= 20 {
            let px = cpu.read_framebuffer().iter().step_by(16).filter(|&&b| b != 0).count();
            let code = cpu.peek_code(rip.wrapping_sub(96), 128);
            let hex: String = code.iter().map(|b| format!("{b:02x} ")).collect();
            let r = cpu.regs();
            eprintln!("\n[chunk {c}] STALL — guest spinning in page {page:#x} (fb={px}). code[rip-96..+32]: {}", hex.trim_end());
            eprintln!("  rax={:#x} rcx={:#x} rdx={:#x} rbx={:#x} rsp={:#x} rbp={:#x} rsi={:#x} rdi={:#x}",
                r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]);
            eprintln!("  r8={:#x} r9={:#x} r10={:#x} r11={:#x} r12={:#x} r13={:#x} r14={:#x} r15={:#x}",
                r[8], r[9], r[10], r[11], r[12], r[13], r[14], r[15]);
            // Dump the data structures the loop is walking (heap pointers in the user range), so a
            // full/corrupt hash table (size/mask/used header) is visible.
            for (name, base) in [("r14", r[14]), ("r10", r[10]), ("rdx", r[2]), ("rcx", r[1])] {
                if (0x7ff0_0000_0000..0x8000_0000_0000).contains(&base) {
                    let m = cpu.peek_code(base, 48);
                    let mh: String = m.iter().map(|b| format!("{b:02x} ")).collect();
                    eprintln!("  *{name}({base:#x})[0..48]: {}", mh.trim_end());
                }
            }
            // Walk the stack: candidate RETURN ADDRESSES (values in the executable library range)
            // up the stack reveal the call chain into the spinning hash lookup → which glib API and
            // library, the key to the skipped-resize root cause.
            let rsp = r[4];
            let mut chain: Vec<u64> = Vec::new();
            for off in (0..1024).step_by(8) {
                let b = cpu.peek_code(rsp.wrapping_add(off), 8);
                let v = u64::from_le_bytes(b.try_into().unwrap_or([0; 8]));
                // Code lives in the shared-library / PIE text range; stack/heap data don't look like this.
                if (0x7ffff7000000..0x7fffffffe000).contains(&v) {
                    chain.push(v);
                    if chain.len() >= 24 { break; }
                }
            }
            eprintln!("  stack return-addr chain (rsp={rsp:#x}): {:#x?}", chain);
            break;
        }
    }

    let fb = cpu.read_framebuffer();
    let px = fb.iter().step_by(16).filter(|&&b| b != 0).count();
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let tail: String = con.lines().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");

    eprintln!("\n==== XFCE NATIVE BOOT RESULT ====");
    eprintln!("halt: {halt:?}");
    if let Halt::Undefined(rip) = halt {
        let bytes = cpu.peek_code(rip, 16);
        let hex: String = bytes.iter().map(|b| format!("{b:02x} ")).collect();
        eprintln!("MISSING INSTRUCTION @ rip={rip:#x}: {}", hex.trim_end());
    }
    eprintln!("framebuffer nonzero samples: {px}  (peak {max_px}; fb base {:#x}, {}x{})",
        cpu.fb_phys_base(), Cpu::FB_W, Cpu::FB_H);
    // Dump the framebuffer (XRGB8888 → PPM) so the result is inspectable / shareable.
    {
        let fb = cpu.read_framebuffer();
        let mut ppm = format!("P6\n{} {}\n255\n", Cpu::FB_W, Cpu::FB_H).into_bytes();
        for px4 in fb.chunks_exact(4) {
            ppm.push(px4[2]); ppm.push(px4[1]); ppm.push(px4[0]); // BGRX → RGB
        }
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../xfce-fb.ppm");
        let _ = std::fs::write(&out, &ppm);
        eprintln!("framebuffer dumped to {}", out.display());
    }
    eprintln!("serial tail:\n{tail}\n====");

    // Bring-up signals (informational — the test never fails; it's a diagnostic harness).
    eprintln!("signals: init={} startx={} xorg={} xfce={} px>5000={}",
        con.contains("HOLO-XFCE-INIT"),
        con.contains("HOLO-STARTX"),
        con.contains("X.Org X Server") || con.contains("(EE)") || con.contains("modeset"),
        con.contains("xfce4-session") || con.contains("xfwm4") || con.contains("xfdesktop"),
        px > 5000,
    );
}
