// fb-xfce-x64-boot-worker.mjs — boot the REAL graphical amd64 Alpine kernel into a FULL XFCE 4
// desktop (Xorg on the simpledrm framebuffer), rendered to a <canvas>. The disk-root graphical
// kernel mounts root=/dev/vda (the XFCE κ-disk) and runs a custom /init that mounts, starts dbus,
// and `startx` → startxfce4. The X server draws onto the emulator's linear framebuffer (advertised
// via screen_info), which we poll + project. The desktop twin of fb-rung2-x64-boot-worker.mjs.
import init, { DevcontainerImage, X64Workspace } from "./pkg/holospaces_web.js";

const gunzip = async (buf) =>
  new Uint8Array(await new Response(new Response(buf).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const fbNonZeroCount = (fb) => { let c = 0; for (let i = 0; i < fb.length; i += 16) if (fb[i]) c++; return c; };
const fbNonZero = (fb) => { for (let i = 0; i < fb.length; i += 16) if (fb[i]) return true; return false; };

// PID 1: bring up the system, then start X + XFCE on the framebuffer. Xorg's stderr → /dev/console
// (the serial) so its log reaches the witness. No real input devices yet (rung C) — the desktop
// renders; interactivity comes once kbd/mouse are wired.
// PID 1. Milestones go to /dev/ttyS0 (the serial — `terminal()`/serialTail + the HUD flags track
// these), NOT /dev/console (which is tty0 = the framebuffer). Each step echoes a marker so a hang
// is pinpointed by the LAST line seen. No system dbus-daemon (a fork hazard, and unneeded — XFCE's
// startxfce4 launches its own session bus via dbus-launch); just a machine-id for dbus-launch.
// The PROVEN init (matches the native harness crates/holospaces/tests/xfce_x64_boot.rs that paints +
// takes input): mount, busybox install, device nodes, modesetting xorg.conf.d, udevd (so X's libinput
// binds the virtio-input device — keyboard + pointer), GSettings schemas, hicolor icon cache, a11y
// off, then Xorg + dbus session + startxfce4. The \\n inside printf are escaped so the SHELL (not JS)
// expands them; the shell here only uses $B/$S/$? (no \${} braces, which JS would interpolate).
const INIT = `#!/bin/sh
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
$B printf 'Section "Device"\\n Identifier "card0"\\n Driver "modesetting"\\n Option "kmsdev" "/dev/dri/card0"\\n Option "AccelMethod" "none"\\nEndSection\\nSection "Screen"\\n Identifier "scr"\\n Device "card0"\\nEndSection\\nSection "ServerFlags"\\n Option "DontVTSwitch" "true"\\nEndSection\\n' > /etc/X11/xorg.conf.d/10-fbdev.conf
$B rm -f /etc/X11/xorg.conf
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
export NO_AT_BRIDGE=1
export GTK_A11Y=none
export GSETTINGS_SCHEMA_DIR=/usr/share/glib-2.0/schemas
$B echo HOLO-STARTXFCE4 > $S
exec startxfce4 > $S 2>&1
`;

// Interactive input events (from the page) queued during the drive loop.
const inputQ = [];
let started = false;
function applyInput(ws, e) {
  if (e.kind === "key") ws.feed_key(e.code >>> 0, !!e.down);
  else if (e.kind === "motion") ws.feed_pointer_motion(e.dx | 0, e.dy | 0);
  else if (e.kind === "button") ws.feed_pointer_button(e.button >>> 0, !!e.down);
  else if (e.kind === "wheel") ws.feed_wheel(e.clicks | 0);
}

self.onmessage = async (ev) => {
  const data = (ev && ev.data) || {};
  const post = (m, transfer) => self.postMessage(m, transfer || []);
  if (data.input) { inputQ.push(data.input); return; } // interactive event during the run
  if (started) return;
  started = true;
  const nonce = data.nonce || "x";
  const ROOTFS = `xfce-x64-rootfs-${nonce}`, PACK = `xfce-x64-pack-${nonce}`;
  try {
    await init();

    if (!data.coldboot) {
      // ── RESUME (default): land on the painted desktop INSTANTLY — the tab never cold-boots ──
      post({ phase: "loading painted-desktop snapshot (resume, no cold boot)" });
      const manifest = await bytes("./snap/desktop.manifest");
      const pagesBin = await bytes("./snap/pages.bin");
      const pagesIdx = await (await fetch("./snap/pages.idx")).text();
      let deltaBin = new Uint8Array(0), deltaIdx = "";
      try { deltaBin = await bytes("./snap/disk-delta.bin"); deltaIdx = await (await fetch("./snap/disk-delta.idx")).text(); } catch {}
      post({ phase: "resuming", manifestKiB: manifest.length >> 10, pagesMiB: pagesBin.length >> 20 });
      const t0 = Date.now();
      // Empty disk image: the framebuffer lives in the RESTORED RAM, so the painted desktop renders
      // immediately; the tiny delta covers boot-modified sectors. (Full disk-from-layer — for launching
      // apps in-session — is the next refinement.)
      const ws = X64Workspace.resume_desktop_from_layer(manifest, pagesBin, pagesIdx, new Uint8Array(0), deltaBin, deltaIdx);
      const dims = ws.framebuffer_dims();
      const W = dims[0], H = dims[1];
      { const fb = ws.framebuffer(); const c = fb.slice();
        post({ phase: "resumed", resumeMs: Date.now() - t0, iter: 0, resumed: true, xfce: true,
               pixels: fbNonZeroCount(fb), W, H, fb: c.buffer }, [c.buffer]); }
      // Drive loop: keep the desktop live + apply queued input, YIELDING each iteration so the page's
      // input messages are delivered into inputQ between runs.
      const STEP = 4_000_000;
      for (let i = 1; ; i++) {
        const halted = ws.run(STEP);
        while (inputQ.length) applyInput(ws, inputQ.shift());
        if (i % 3 === 0) {
          const fb = ws.framebuffer(); const c = fb.slice();
          post({ phase: "live", iter: i, resumed: true, xfce: true, pixels: fbNonZeroCount(fb), W, H,
                 serialTail: ws.terminal().split("\n").slice(-16).join("\n"), fb: c.buffer }, [c.buffer]);
        }
        if (halted) { post({ phase: "halted", haltReason: ws.halt_reason() }); break; }
        await new Promise((r) => setTimeout(r));
      }
      return;
    }

    // ── COLD BOOT (debug path: ?coldboot=1) — boots from scratch (guest-minutes) ──
    post({ phase: "loading graphical amd64 kernel + XFCE x86_64 rootfs" });
    const kernel = await gunzip(await bytes("./graphical-x64-kernel.gz"));
    const layer  = await bytes("./alpine-amd64-xfce-layer.tar.gz");   // Xorg + XFCE 4 (~688 MiB unpacked)

    post({ phase: "assembling bootable ext4 (XFCE + startx init) → OPFS κ-disk", layerBytes: layer.length });
    const DISK = 768 * 1024 * 1024;   // lean ~384 MiB rootfs + ext4 overhead + working room
    const root = await navigator.storage.getDirectory();
    const rw = await (await root.getFileHandle(ROOTFS, { create: true })).createSyncAccessHandle();
    rw.truncate(0);
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const imageLen = img.assembleBootableWithInitIntoOpfs(rw, DISK, new TextEncoder().encode(INIT));
    rw.close();

    post({ phase: "booting XFCE (kernel → ext4 root → /init → startx → startxfce4)" });
    const rootfsRead = await (await root.getFileHandle(ROOTFS)).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle(PACK, { create: true })).createSyncAccessHandle();
    packHandle.truncate(0);
    const t0 = Date.now();
    post({ phase: "ctor-start (ingesting 1.3GB κ-disk + ELF load, synchronous)" });
    const ws = X64Workspace.boot_devcontainer_opfs_streamed_graphical(kernel, rootfsRead, packHandle);
    post({ phase: "ctor-done", ctorMs: Date.now() - t0 });

    const dims = ws.framebuffer_dims();
    const [W, H] = [dims[0], dims[1]];
    let fbcon = false, mounted = false, initRan = false, xorg = false, xfce = false, halted = false;
    let firstX = -1, iterRan = 0;
    const STEP = 8_000_000;
    const tRun0 = performance.now();

    for (let i = 0; i < 60000; i++) {
      halted = ws.run(STEP);
      iterRan = i + 1;
      const term = ws.terminal();
      fbcon   = fbcon   || /fb0: .* frame ?buffer|simpledrm/i.test(term);
      mounted = mounted || /EXT4-fs .* mounted|VFS: Mounted root/i.test(term);
      initRan = initRan || /HOLO-XFCE-INIT|HOLO-STARTX/.test(term);
      xorg    = xorg    || /X\.Org X Server|\(EE\)|\(II\) modeset|fbdev|Loading .*libglx|X Protocol/i.test(term);
      xfce    = xfce    || /xfce4-session|xfwm4|xfdesktop|xfce4-panel|HOLO-XFCE-EXIT/i.test(term);
      const fb = ws.framebuffer();
      const px = fbNonZeroCount(fb);
      const nz = px > 0;
      if (xfce && firstX < 0) firstX = i;

      const sendFb = nz && (i % 40 === 0 || (firstX >= 0 && i <= firstX + 12));
      const msg = { phase: "running", iter: i, fbcon, mounted, initRan, xorg, xfce, pixels: px, W, H,
                    serialTail: term.split("\n").slice(-16).join("\n") };
      if (sendFb) { const c = fb.slice(); msg.fb = c.buffer; post(msg, [c.buffer]); }
      else if (i % 15 === 0) post(msg);

      if (halted) break;
      if (firstX >= 0 && i > firstX + 200) break;   // XFCE up + rendered a while → witnessed
    }

    const term = ws.terminal();
    const fb = ws.framebuffer();
    const elapsedSec = (performance.now() - tRun0) / 1000;
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    const c = fb.slice();
    post({ phase: "done", done: true, imageLen, W, H, fbcon, mounted, initRan, xorg, xfce, halted,
           pixels: fbNonZeroCount(fb), haltReason: ws.halt_reason(),
           mips: Math.round((iterRan * STEP) / elapsedSec / 1e5) / 10, elapsedSec: Math.round(elapsedSec),
           serialTail: term.split("\n").slice(-40).join("\n"), fullSerial: term, fb: c.buffer }, [c.buffer]);
  } catch (e) {
    post({ phase: "error", error: String(e && e.stack ? e.stack : e) });
  }
};
