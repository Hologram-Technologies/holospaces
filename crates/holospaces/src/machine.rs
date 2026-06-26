//! **Boot Orchestrator** — assembles the machine a holospace boots on: it
//! generates the device tree describing the emulator's memory map (RAM, CLINT,
//! PLIC, the `virtio-mmio` block device) and hands the kernel + device tree +
//! κ-disk to the [emulator](crate::emulator).
//!
//! Realizes the *Boot Orchestrator* component of the Boot Layer (arc42 ch.5) and
//! the *Boot orchestration* step of the runtime view (ch.6). The device tree is
//! **generated in-crate** — a real flattened-device-tree blob (the DTB spec /
//! `dtc` is the authority) emitted from the same memory-map constants the
//! emulator decodes (one source of truth, Law L4) — not a pinned artifact. A
//! guest kernel parses it to discover and mount the root filesystem over
//! `/dev/vda` (`CC-14`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::emulator::{
    net, Emulator, CLINT_BASE, PLIC_BASE, VIRTIO9P_BASE, VIRTIO9P_END, VIRTIO9P_IRQ,
    VIRTIONET_BASE, VIRTIONET_END, VIRTIONET_IRQ, VIRTIO_BASE, VIRTIO_END, VIRTIO_IRQ,
};

/// Where the device tree blob is placed in RAM, relative to `base` (clear of the
/// kernel image, which loads at `base + text_offset`).
const DTB_OFFSET: u64 = 0x0700_0000;

/// The `/init` a **deployed, interactive** devcontainer boots (the workbench's OS
/// entrypoint). Unlike the `CC-9`/`CC-14` conformance init — which prints a marker
/// and powers off to make boot-to-userspace deterministic — this one makes the
/// environment a *running dev environment*: it brings up the core pseudo
/// filesystems (`proc`/`sys`/`dev` — so `devtmpfs` mounts), mounts the shared
/// `virtio-9p` workspace at `/workspace`, installs BusyBox's applets so the usual
/// commands work, sets a sensible terminal size, and **execs an interactive shell
/// on a controlling terminal** — so the OS stays up and the holospace terminal is
/// live, instead of shutting down right after boot.
///
/// The shell is started with `setsid -c` so it becomes a *session leader with the
/// console (`/dev/hvc0`) as its controlling terminal*. Without this, PID 1 has no
/// session (`SID 0`) and the console has no foreground process group, so the tty's
/// `^C` produces a SIGINT with nowhere to go — Ctrl-C would not interrupt a running
/// command. `setsid -c` gives the console a foreground process group, so Ctrl-C
/// (and job control, Ctrl-Z) reach the foreground command, exactly as a real
/// terminal. (`exec setsid` is safe from PID 1 here: PID 1 is not a process-group
/// leader, so BusyBox `setsid` does not fork — it cannot orphan init.)
///
/// If the image ships a language server at `/usr/bin/lsp-demo` (the `CC-18`
/// base), it is started as a background TCP service (`--listen 7000`) before the
/// shell, so the workbench's LSP client reaches it over the in-process substrate
/// bridge (ADR-020, `CC-33`) — language intelligence with no Node. The guard
/// (`[ -x … ]`) makes it a no-op for an image without it.
///
/// It also starts the **task-runner agent** (`CC-53`): a tiny `/bin/sh` loop
/// that watches `/workspace/.hs-tasks/` (on the shared `virtio-9p` workspace) for
/// `<id>.cmd` request files, runs each in the devcontainer (`sh <id>.cmd`,
/// stdout+stderr → `<id>.out`, exit code → `<id>.exit`), and cleans up. The
/// workbench's `holospace-tasks` provider drives `tasks.json` tasks through this
/// file channel — a real run in the guest, output + exit captured, no server
/// outside the holospace. Image-agnostic: it needs only `sh` and the share.
///
/// The base image must provide a static `/bin/busybox` with the `setsid`/`stty`
/// applets (the `CC-22` BusyBox base).
pub const DEVCONTAINER_INIT: &[u8] = b"#!/bin/busybox sh\n\
/bin/busybox mkdir -p /proc /sys /dev /tmp /root /workspace /bin /sbin /usr/bin /usr/sbin\n\
/bin/busybox mount -t proc proc /proc\n\
/bin/busybox mount -t sysfs sysfs /sys\n\
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null\n\
/bin/busybox mount -t 9p -o trans=virtio,version=9p2000.L,msize=65536 hsworkspace /workspace 2>/dev/null\n\
/bin/busybox --install -s\n\
export PATH=/bin:/sbin:/usr/bin:/usr/sbin HOME=/root PS1='holospace:$PWD\\$ '\n\
/bin/busybox stty rows 24 cols 80 2>/dev/null\n\
cd /workspace\n\
[ -x /usr/bin/lsp-demo ] && /usr/bin/lsp-demo --listen 7000 &\n\
mkdir -p /workspace/.hs-tasks 2>/dev/null\n\
( while true; do for f in /workspace/.hs-tasks/*.cmd; do [ -e $f ] || continue; b=${f%.cmd}; mv $f $b.run 2>/dev/null || continue; ( cd /workspace 2>/dev/null; sh $b.run > $b.out 2>&1; echo $? > $b.exit ); rm -f $b.run; done; sleep 1; done ) &\n\
/bin/busybox echo 'holospace devcontainer ready \xe2\x80\x94 /workspace is your shared workspace'\n\
exec /bin/busybox setsid -c /bin/busybox sh\n";

/// The `/init` for a **real OCI devcontainer image** (debian/ubuntu/buildpack-deps
/// — the image a repository declares, `CC-42`), which ships its own `/bin/sh` +
/// coreutils + `mount` rather than BusyBox. Unlike [`DEVCONTAINER_INIT`] (which
/// assumes a static `/bin/busybox`), this uses the image's own tools: it mounts
/// the pseudo-filesystems and the shared `/workspace` (virtio-9p, CC-15), sets a
/// sane environment, and execs the image's login shell (`bash` if present, else
/// `sh`) on the console — so the launched holospace is an interactive, real
/// devcontainer. `2>/dev/null` keeps a tool the image happens to lack from
/// aborting the boot.
pub const REAL_IMAGE_INIT: &[u8] = b"#!/bin/sh\n\
mkdir -p /proc /sys /dev /tmp /workspace 2>/dev/null\n\
mount -t proc proc /proc 2>/dev/null\n\
mount -t sysfs sysfs /sys 2>/dev/null\n\
mount -t devtmpfs devtmpfs /dev 2>/dev/null\n\
mount -t tmpfs tmpfs /tmp 2>/dev/null\n\
mount -t 9p -o trans=virtio,version=9p2000.L,msize=65536 hsworkspace /workspace 2>/dev/null\n\
export HOME=/root TERM=xterm PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin PS1='holospace:$PWD\\$ '\n\
cd /workspace 2>/dev/null || cd /root 2>/dev/null || cd /\n\
mkdir -p /workspace/.hs-tasks 2>/dev/null\n\
( while true; do for f in /workspace/.hs-tasks/*.cmd; do [ -e $f ] || continue; b=${f%.cmd}; mv $f $b.run 2>/dev/null || continue; ( cd /workspace 2>/dev/null; sh $b.run > $b.out 2>&1; echo $? > $b.exit ); rm -f $b.run; done; sleep 1; done ) &\n\
echo 'holospace devcontainer ready \xe2\x80\x94 the repository image; /workspace is shared with the editor'\n\
[ -x /bin/bash ] && exec /bin/bash -l || exec /bin/sh\n";

/// The machine a holospace boots on: a single RV64GC hart over `ram_bytes` of
/// RAM mapped at `base`, with the CLINT, the PLIC, and one `virtio-mmio` block
/// device — the device set the [emulator](crate::emulator) implements.
pub struct MachineSpec {
    /// The base physical address of RAM (the reset PC); the device tree's
    /// `memory` node and the kernel load address derive from it.
    pub base: u64,
    /// RAM size in bytes.
    pub ram_bytes: u64,
    /// The kernel command line (the device tree's `chosen/bootargs`).
    pub bootargs: String,
}

impl MachineSpec {
    /// A default devcontainer machine: 512 MiB at `0x8000_0000`, rooting on the
    /// VirtIO block device over the SBI console.
    #[must_use]
    pub fn devcontainer() -> Self {
        MachineSpec {
            base: 0x8000_0000,
            ram_bytes: 512 * 1024 * 1024,
            bootargs: String::from("root=/dev/vda rw console=hvc0 earlycon=sbi init=/init"),
        }
    }

    /// A default **networked** devcontainer machine: like [`Self::devcontainer`]
    /// but with the kernel's built-in DHCP autoconfiguration enabled
    /// (`ip=dhcp`), so the guest brings its `virtio-net` interface up against the
    /// userspace NAT at boot (`CC-16`).
    #[must_use]
    pub fn devcontainer_net() -> Self {
        MachineSpec {
            base: 0x8000_0000,
            ram_bytes: 512 * 1024 * 1024,
            bootargs: String::from("root=/dev/vda rw console=hvc0 earlycon=sbi ip=dhcp init=/init"),
        }
    }

    /// Generate the flattened device tree (DTB) describing this machine — the
    /// blob the guest kernel parses to find its memory, the timer, the interrupt
    /// controller, and the root block device. Emitted from the emulator's own
    /// memory-map constants (one source of truth).
    #[must_use]
    pub fn device_tree(&self) -> Vec<u8> {
        // The full device set (both `virtio-9p` and `virtio-net` slots) — the
        // shape external callers / tests inspect.
        self.device_tree_for(true, true)
    }

    /// Generate the device tree, declaring the optional `virtio-9p` (workspace)
    /// and `virtio-net` slots only when they are actually attached. An *unattached*
    /// `virtio-mmio` node makes the guest kernel read magic `0` and log a "Wrong
    /// magic value" error (and stall), so a machine booted without a workspace or
    /// network must not advertise those slots.
    fn device_tree_for(&self, has_9p: bool, has_net: bool) -> Vec<u8> {
        let mut fdt = Fdt::new();
        // Phandles referenced by interrupt routing.
        const PH_INTC: u32 = 1; // the hart's local interrupt controller
        const PH_PLIC: u32 = 2; // the PLIC

        fdt.begin_node(""); // root
        fdt.prop_u32("#address-cells", 2);
        fdt.prop_u32("#size-cells", 2);
        fdt.prop_str("compatible", "holospaces,emu");
        fdt.prop_str("model", "holospaces RISC-V emulator");

        fdt.begin_node("chosen");
        fdt.prop_str("bootargs", &self.bootargs);
        fdt.prop_str("stdout-path", "/chosen");
        fdt.end_node();

        fdt.begin_node("cpus");
        fdt.prop_u32("#address-cells", 1);
        fdt.prop_u32("#size-cells", 0);
        fdt.prop_u32("timebase-frequency", 10_000_000);
        fdt.begin_node("cpu@0");
        fdt.prop_str("device_type", "cpu");
        fdt.prop_u32("reg", 0);
        fdt.prop_str("status", "okay");
        fdt.prop_str("compatible", "riscv");
        fdt.prop_str("riscv,isa", "rv64imafdc");
        fdt.prop_str("mmu-type", "riscv,sv57");
        fdt.begin_node("interrupt-controller");
        fdt.prop_u32("#interrupt-cells", 1);
        fdt.prop_empty("interrupt-controller");
        fdt.prop_str("compatible", "riscv,cpu-intc");
        fdt.prop_u32("phandle", PH_INTC);
        fdt.end_node(); // interrupt-controller
        fdt.end_node(); // cpu@0
        fdt.end_node(); // cpus

        fdt.begin_node(&format!("memory@{:x}", self.base));
        fdt.prop_str("device_type", "memory");
        fdt.prop_reg(self.base, self.ram_bytes);
        fdt.end_node();

        fdt.begin_node("soc");
        fdt.prop_u32("#address-cells", 2);
        fdt.prop_u32("#size-cells", 2);
        fdt.prop_str("compatible", "simple-bus");
        fdt.prop_empty("ranges");

        fdt.begin_node(&format!("clint@{CLINT_BASE:x}"));
        fdt.prop_str_list("compatible", &["sifive,clint0", "riscv,clint0"]);
        fdt.prop_reg(CLINT_BASE, 0x10000);
        // interrupts-extended = <&intc 3 &intc 7> (M/S software + timer)
        fdt.prop_cells("interrupts-extended", &[PH_INTC, 3, PH_INTC, 7]);
        fdt.end_node();

        fdt.begin_node(&format!("interrupt-controller@{PLIC_BASE:x}"));
        fdt.prop_str_list("compatible", &["sifive,plic-1.0.0", "riscv,plic0"]);
        fdt.prop_reg(PLIC_BASE, 0x0400_0000);
        fdt.prop_empty("interrupt-controller");
        fdt.prop_u32("#interrupt-cells", 1);
        fdt.prop_u32("#address-cells", 0);
        fdt.prop_u32("riscv,ndev", 31);
        // interrupts-extended = <&intc 11 &intc 9> (M/S external)
        fdt.prop_cells("interrupts-extended", &[PH_INTC, 11, PH_INTC, 9]);
        fdt.prop_u32("phandle", PH_PLIC);
        fdt.end_node();

        fdt.begin_node(&format!("virtio_mmio@{VIRTIO_BASE:x}"));
        fdt.prop_str("compatible", "virtio,mmio");
        fdt.prop_reg(VIRTIO_BASE, VIRTIO_END - VIRTIO_BASE);
        fdt.prop_u32("interrupt-parent", PH_PLIC);
        fdt.prop_u32("interrupts", VIRTIO_IRQ);
        fdt.end_node();

        // The 9P (shared workspace filesystem) virtio-mmio slot (CC-15) — only
        // when a workspace is attached (else the kernel reads magic 0 and errors).
        if has_9p {
            fdt.begin_node(&format!("virtio_mmio@{VIRTIO9P_BASE:x}"));
            fdt.prop_str("compatible", "virtio,mmio");
            fdt.prop_reg(VIRTIO9P_BASE, VIRTIO9P_END - VIRTIO9P_BASE);
            fdt.prop_u32("interrupt-parent", PH_PLIC);
            fdt.prop_u32("interrupts", VIRTIO9P_IRQ);
            fdt.end_node();
        }

        // The network virtio-mmio slot (CC-16) — only when a network egress is
        // attached (else the kernel reads magic 0 and errors + stalls).
        if has_net {
            fdt.begin_node(&format!("virtio_mmio@{VIRTIONET_BASE:x}"));
            fdt.prop_str("compatible", "virtio,mmio");
            fdt.prop_reg(VIRTIONET_BASE, VIRTIONET_END - VIRTIONET_BASE);
            fdt.prop_u32("interrupt-parent", PH_PLIC);
            fdt.prop_u32("interrupts", VIRTIONET_IRQ);
            fdt.end_node();
        }

        fdt.end_node(); // soc
        fdt.end_node(); // root
        fdt.finish()
    }

    /// Build a machine ready to run: an emulator with `ram_bytes` of RAM, the SBI
    /// firmware, the `rootfs` attached to the VirtIO block device, and the kernel
    /// loaded with a freshly generated device tree. The caller drives it with
    /// [`Emulator::run`] (the Boot Layer's Lifecycle).
    pub fn boot(&self, kernel: &[u8], rootfs: Vec<u8>) -> Result<Emulator, crate::emulator::Trap> {
        let mut emu = Emulator::new(self.base, self.ram_bytes as usize);
        emu.enable_sbi();
        emu.attach_disk(rootfs);
        let dtb = self.device_tree_for(false, false);
        emu.boot_kernel(kernel, &dtb, self.base + DTB_OFFSET)?;
        Ok(emu)
    }

    /// Boot like [`Self::boot`], additionally attaching a shared **workspace
    /// filesystem** over `virtio-9p` (`CC-15`): `seed` is the files holospaces
    /// places on the share (name → bytes). The guest mounts it (tag
    /// `hsworkspace`) and the editor and the running OS read/write the same
    /// files; observe the guest's writes with [`Emulator::workspace_file`].
    pub fn boot_workspace(
        &self,
        kernel: &[u8],
        rootfs: Vec<u8>,
        seed: &[(&str, &[u8])],
    ) -> Result<Emulator, crate::emulator::Trap> {
        let mut emu = Emulator::new(self.base, self.ram_bytes as usize);
        emu.enable_sbi();
        emu.attach_disk(rootfs);
        emu.attach_workspace(seed);
        let dtb = self.device_tree_for(true, false);
        emu.boot_kernel(kernel, &dtb, self.base + DTB_OFFSET)?;
        Ok(emu)
    }

    /// Boot like [`Self::boot`], additionally attaching a **network** device
    /// bridged to the world over `egress` (`CC-16`). The guest configures its
    /// `virtio-net` interface with DHCP against the userspace [NAT](crate::emulator::net)
    /// and its TCP streams flow out over `egress` (a host socket natively; a
    /// WebSocket tunnel in the browser — ADR-014). Use with
    /// [`Self::devcontainer_net`] so the kernel command line carries `ip=dhcp`.
    pub fn boot_net(
        &self,
        kernel: &[u8],
        rootfs: Vec<u8>,
        egress: alloc::boxed::Box<dyn net::Egress>,
    ) -> Result<Emulator, crate::emulator::Trap> {
        let mut emu = Emulator::new(self.base, self.ram_bytes as usize);
        emu.enable_sbi();
        emu.attach_disk(rootfs);
        emu.attach_net(egress);
        let dtb = self.device_tree_for(false, true);
        emu.boot_kernel(kernel, &dtb, self.base + DTB_OFFSET)?;
        Ok(emu)
    }

    /// Boot like [`Self::boot_net`], but page the disk from a **caller-supplied
    /// [`KappaStore`](hologram_substrate_core::KappaStore)** — the browser peer
    /// passes an OPFS-backed store so the disk's sectors live off the wasm heap
    /// (paged on demand; "the KappaStore IS the memory, RAM is a cache"), which is
    /// how it boots a real image without holding it all in RAM (the paged κ-disk).
    pub fn boot_net_in(
        &self,
        kernel: &[u8],
        rootfs: Vec<u8>,
        egress: alloc::boxed::Box<dyn net::Egress>,
        disk_store: alloc::boxed::Box<dyn hologram_substrate_core::KappaStore>,
    ) -> Result<Emulator, crate::emulator::Trap> {
        let mut emu = Emulator::new(self.base, self.ram_bytes as usize);
        emu.enable_sbi();
        emu.attach_disk_in(disk_store, rootfs);
        emu.attach_net(egress);
        let dtb = self.device_tree_for(false, true);
        emu.boot_kernel(kernel, &dtb, self.base + DTB_OFFSET)?;
        Ok(emu)
    }

    /// Boot like [`Self::boot_net_in`], but **stream** the disk's `sector_count`
    /// sectors from `read` into the supplied store (no full image in RAM) — the
    /// browser peer reads each sector from the OPFS rootfs file straight into the
    /// OPFS-backed store, so a large real image boots without materializing the
    /// whole `Vec` (the paged κ-disk, transient-peak-free).
    pub fn boot_net_streamed<R: FnMut(u64, &mut [u8])>(
        &self,
        kernel: &[u8],
        sector_count: u64,
        read: R,
        egress: alloc::boxed::Box<dyn net::Egress>,
        disk_store: alloc::boxed::Box<dyn hologram_substrate_core::KappaStore>,
    ) -> Result<Emulator, crate::emulator::Trap> {
        let mut emu = Emulator::new(self.base, self.ram_bytes as usize);
        emu.enable_sbi();
        emu.attach_disk_streamed(disk_store, sector_count, read);
        emu.attach_net(egress);
        let dtb = self.device_tree_for(false, true);
        emu.boot_kernel(kernel, &dtb, self.base + DTB_OFFSET)?;
        Ok(emu)
    }

    /// Boot like [`Self::boot_net`], additionally **forwarding ports**: an
    /// `ingress` transport carries inbound connections to a server inside the
    /// devcontainer (`CC-21`) — the running-app preview a Codespace surfaces. Use
    /// with [`Self::devcontainer_net`] (the guest's interface comes up with DHCP).
    pub fn boot_net_forward(
        &self,
        kernel: &[u8],
        rootfs: Vec<u8>,
        egress: alloc::boxed::Box<dyn net::Egress>,
        ingress: alloc::boxed::Box<dyn net::Ingress>,
    ) -> Result<Emulator, crate::emulator::Trap> {
        let mut emu = Emulator::new(self.base, self.ram_bytes as usize);
        emu.enable_sbi();
        emu.attach_disk(rootfs);
        emu.attach_net_forward(egress, ingress);
        let dtb = self.device_tree_for(false, true);
        emu.boot_kernel(kernel, &dtb, self.base + DTB_OFFSET)?;
        Ok(emu)
    }

    /// Boot with **both** the shared `virtio-9p` workspace (`CC-15`) **and** the
    /// network (`CC-16`): the editor and the OS share `/workspace` (so the
    /// `FileSystemProvider` works) *and* the guest has a TCP stack — the
    /// combination the deployed devcontainer needs to run a language server / a
    /// remote extension host reachable over the in-process bridge (ADR-020). Use
    /// with [`Self::devcontainer_net`] (the guest's interface comes up with DHCP).
    /// The caller attaches the loopback ingress with [`Emulator::enable_loopback`].
    pub fn boot_workspace_net(
        &self,
        kernel: &[u8],
        rootfs: Vec<u8>,
        seed: &[(&str, &[u8])],
        egress: alloc::boxed::Box<dyn net::Egress>,
    ) -> Result<Emulator, crate::emulator::Trap> {
        let mut emu = Emulator::new(self.base, self.ram_bytes as usize);
        emu.enable_sbi();
        emu.attach_disk(rootfs);
        emu.attach_workspace(seed);
        emu.attach_net(egress);
        let dtb = self.device_tree_for(true, true);
        emu.boot_kernel(kernel, &dtb, self.base + DTB_OFFSET)?;
        Ok(emu)
    }
}

// ── A minimal flattened-device-tree (DTB) writer ───────────────────────────
//
// The DTB binary format (devicetree.org "Flattened Devicetree" / the format
// `dtc` emits and the Linux kernel parses): a header, an (empty) memory
// reservation block, a structure block of big-endian tokens, and a strings
// block of deduplicated property names.

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;
const FDT_VERSION: u32 = 17;
const FDT_LAST_COMP_VERSION: u32 = 16;

struct Fdt {
    structure: Vec<u8>,
    strings: Vec<u8>,
}

impl Fdt {
    fn new() -> Self {
        Fdt {
            structure: Vec::new(),
            strings: Vec::new(),
        }
    }

    fn token(&mut self, t: u32) {
        self.structure.extend_from_slice(&t.to_be_bytes());
    }

    fn pad(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }

    fn begin_node(&mut self, name: &str) {
        self.token(FDT_BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad();
    }

    fn end_node(&mut self) {
        self.token(FDT_END_NODE);
    }

    /// Intern a property name in the strings block, returning its offset.
    fn name_off(&mut self, name: &str) -> u32 {
        // Linear scan for an existing identical, null-terminated entry.
        let needle = name.as_bytes();
        let mut i = 0;
        while i < self.strings.len() {
            let end = self.strings[i..]
                .iter()
                .position(|&b| b == 0)
                .map(|p| i + p)
                .unwrap_or(self.strings.len());
            if &self.strings[i..end] == needle {
                return i as u32;
            }
            i = end + 1;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(needle);
        self.strings.push(0);
        off
    }

    fn prop(&mut self, name: &str, value: &[u8]) {
        let nameoff = self.name_off(name);
        self.token(FDT_PROP);
        self.structure
            .extend_from_slice(&(value.len() as u32).to_be_bytes());
        self.structure.extend_from_slice(&nameoff.to_be_bytes());
        self.structure.extend_from_slice(value);
        self.pad();
    }

    fn prop_empty(&mut self, name: &str) {
        self.prop(name, &[]);
    }

    fn prop_u32(&mut self, name: &str, v: u32) {
        self.prop(name, &v.to_be_bytes());
    }

    fn prop_str(&mut self, name: &str, s: &str) {
        let mut v = Vec::with_capacity(s.len() + 1);
        v.extend_from_slice(s.as_bytes());
        v.push(0);
        self.prop(name, &v);
    }

    fn prop_str_list(&mut self, name: &str, items: &[&str]) {
        let mut v = Vec::new();
        for s in items {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        self.prop(name, &v);
    }

    fn prop_cells(&mut self, name: &str, cells: &[u32]) {
        let mut v = Vec::with_capacity(cells.len() * 4);
        for c in cells {
            v.extend_from_slice(&c.to_be_bytes());
        }
        self.prop(name, &v);
    }

    /// A `reg = <addr_hi addr_lo size_hi size_lo>` (2 address + 2 size cells).
    fn prop_reg(&mut self, addr: u64, size: u64) {
        self.prop_cells(
            "reg",
            &[
                (addr >> 32) as u32,
                addr as u32,
                (size >> 32) as u32,
                size as u32,
            ],
        );
    }

    /// Emit the complete DTB: header + memory reservation block + structure +
    /// strings.
    fn finish(mut self) -> Vec<u8> {
        self.token(FDT_END);

        // Layout: header (40) → mem-rsv block (one terminating entry, 16) →
        // structure block → strings block. All offsets 8-byte aligned for the
        // reservation block, 4-byte for the rest.
        let header_len = 40u32;
        let memrsv_len = 16u32; // a single zero (address, size) terminator
        let off_struct = header_len + memrsv_len;
        let off_strings = off_struct + self.structure.len() as u32;
        let total = off_strings + self.strings.len() as u32;

        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(&FDT_MAGIC.to_be_bytes());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&off_struct.to_be_bytes());
        out.extend_from_slice(&off_strings.to_be_bytes());
        out.extend_from_slice(&header_len.to_be_bytes()); // off_mem_rsvmap
        out.extend_from_slice(&FDT_VERSION.to_be_bytes());
        out.extend_from_slice(&FDT_LAST_COMP_VERSION.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // boot_cpuid_phys
        out.extend_from_slice(&(self.strings.len() as u32).to_be_bytes());
        out.extend_from_slice(&(self.structure.len() as u32).to_be_bytes());
        // Memory reservation block: terminator only.
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&self.structure);
        out.extend_from_slice(&self.strings);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated DTB is internally consistent: the magic, the total size,
    /// and the block offsets/sizes in the header match the emitted bytes (the
    /// structural invariants `dtc` and the kernel rely on).
    #[test]
    fn the_device_tree_header_is_consistent() {
        let dtb = MachineSpec::devcontainer().device_tree();
        let be = |o: usize| u32::from_be_bytes([dtb[o], dtb[o + 1], dtb[o + 2], dtb[o + 3]]);
        assert_eq!(be(0), FDT_MAGIC, "magic");
        assert_eq!(be(4) as usize, dtb.len(), "totalsize == byte length");
        let off_struct = be(8) as usize;
        let off_strings = be(12) as usize;
        let size_strings = be(32) as usize;
        let size_struct = be(36) as usize;
        assert_eq!(be(20), FDT_VERSION);
        assert_eq!(
            off_struct + size_struct,
            off_strings,
            "struct block sits before strings"
        );
        assert_eq!(
            off_strings + size_strings,
            dtb.len(),
            "strings block ends the blob"
        );
        // The structure block ends with FDT_END.
        assert_eq!(be(off_strings - 4), FDT_END, "structure ends with FDT_END");
    }
}
