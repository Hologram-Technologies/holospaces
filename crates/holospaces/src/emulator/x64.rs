//! **The x86-64 (AMD64 / Intel 64) core** — the system emulator's third ISA
//! target (`CC-43`), alongside the [RISC-V](super) `RV64GC` core and the
//! [`aarch64`](crate::emulator::aarch64) `ARMv8-A` core.
//!
//! x86-64 is the ubiquitous registry architecture: most container images publish
//! a `linux/amd64` variant, so this is the core that lets the browser peer boot
//! the largest share of real devcontainers. Like the other cores it is a CPU over
//! the **shared device bus** (`devbus` (`super::devbus`)) — the κ-disk (`virtio`), the
//! console, the userspace NAT — so the disk, networking, and workspace are not
//! re-implemented per ISA (Law L4). Deterministic: identical image + input yield
//! identical console output and final state (Law L1/L5), so a κ snapshot is
//! reproducible across peers.
//!
//! **Architectural authority.** This core is built to the published x86-64
//! specification — the *Intel® 64 and IA-32 Architectures Software Developer's
//! Manual* (instruction semantics from Vol 2; long-mode paging, the local APIC +
//! APIC timer, the IDT and interrupt/exception delivery, the 8259-via-LINT0
//! virtual-wire mode, and `IA32_TSC` from Vol 3A). The SDM is imported and pinned
//! under `vv/artifacts/intel-sdm/` and, per the V&V governance, paired with an
//! external validator: `qemu-system-x86_64` (an independent SDM implementation) is
//! the differential oracle `CC-44` witnesses the boot against — behaviour is
//! defined by the spec and checked against the reference, never self-referentially.
//!
//! This module is the **long-mode integer core + platform** it boots on: the
//! 64-bit register file and `RFLAGS`, a flat 64-bit address space over guest RAM,
//! the legacy `16550` serial console (port `0x3f8`, the boot console), and the
//! decode/execute of the x86-64 integer instruction set (REX-prefixed,
//! ModRM/SIB-addressed). Conformance is built up instruction-by-instruction (the
//! analogue of the RISC-V core's riscv-tests, `CC-9`); the full real-mode→long-mode
//! Linux boot path composes on this core.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

/// Why the core stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Halt {
    /// The step budget was exhausted (the caller drives more).
    OutOfBudget,
    /// A `hlt` with interrupts disabled — the guest powered off / wedged.
    Halted,
    /// An unsupported or malformed instruction was hit (the core does not yet
    /// implement it) — carries the faulting `rip`.
    Undefined(u64),
}

/// `RFLAGS` bits the integer core maintains.
mod flag {
    pub const CF: u64 = 1 << 0;
    pub const PF: u64 = 1 << 2;
    pub const AF: u64 = 1 << 4;
    pub const ZF: u64 = 1 << 6;
    pub const SF: u64 = 1 << 7;
    pub const OF: u64 = 1 << 11;
}

/// The legacy `16550` UART (the PC serial console at I/O port `0x3f8`) — the
/// boot console (`console=ttyS0`). Output is buffered for [`Cpu::console`]; input
/// is the terminal channel ([`Cpu::feed_console`], `CC-11`).
struct Uart {
    output: Vec<u8>,
    input: Vec<u8>,
    in_cursor: usize,
    /// Line Control Register — bit 7 (`DLAB`) selects the divisor latch over the
    /// data/IER registers; the kernel sets the word length here too.
    lcr: u8,
    /// Interrupt Enable Register (the kernel enables/disables UART IRQs).
    ier: u8,
    /// Modem Control Register — bit 4 (`LOOP`) puts the UART in the loopback the
    /// 8250 autodetection drives; the FIFO/scratch probes also run.
    mcr: u8,
    /// Scratch register (`0x3ff`) — the 8250 detection writes then reads it back
    /// to confirm a real UART is present.
    scratch: u8,
    /// FIFO Control Register shadow (the detection reads it back via IIR).
    fcr: u8,
    /// The divisor latch (the configured baud rate; written when `DLAB` is set).
    divisor: u16,
    /// Transmit-holding-register-empty interrupt pending (16550 THRE). Set on a
    /// THR-empty transition (a `THR` write, or enabling `ETBEI` while empty),
    /// CLEARED when the IIR is read — a one-shot per transition, not a level held
    /// while `ETBEI` is set. (A level THRE re-fires the serial ISR forever.)
    thre_pending: bool,
    /// dev-only access counters: [THR-write, IER-write, IIR-read, LSR-read,
    /// IRQ4-raise]. Used to diagnose the interrupt-driven-TX path (a livelock shows
    /// up as one of these exploding relative to retired instructions).
    dbg: [u64; 5],
}

/// The shared platform the core drives: the console and the substrate devices —
/// the κ-disk, the shared workspace filesystem, and the userspace network — the
/// *same* [`VirtioBlk`](super::VirtioBlk) / [`Virtio9p`](super::Virtio9p) /
/// [`VirtioNet`](super::VirtioNet) the RISC-V and AArch64 machines boot, serviced
/// by the one shared `devbus` (Law L4: devices are shared, not per-ISA).
/// dev-only (`cc44-trace`): set while the CPU is executing inside `__text_poke`,
/// so the CR3-switch / aliased-write trace fires only for the poke sequence.
#[cfg(feature = "cc44-trace")]
static TP_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct Sys {
    uart: Uart,
    /// The `virtio-blk` κ-disk rootfs (`CC-7`), when a disk is attached. The
    /// shared `devbus` services its queue against the κ-disk; the full long-mode
    /// boot path (`#12`) advertises the device to the guest.
    virtio: Option<super::VirtioBlk>,
    /// The `virtio-9p` device serving the shared workspace filesystem, when
    /// attached (`CC-15` parity); `None` otherwise.
    virtio9p: Option<super::Virtio9p>,
    /// The `virtio-net` device + the userspace TCP/IP NAT, when networking is
    /// attached (`CC-16` parity); `None` for an offline machine.
    virtionet: Option<super::VirtioNet>,
    /// The host side of the in-process loopback bridge (ADR-020, `CC-33`
    /// parity), when the workbench dials guest listeners; `None` until
    /// [`Cpu::enable_loopback`] attaches it.
    loopback: Option<super::net::LoopbackHandle>,
    /// The interrupt-descriptor table register (`IDTR`): `(base, limit)` of the
    /// 64-bit IDT the kernel installs with `LIDT` — the long-mode boot path's
    /// interrupt delivery (`#12`).
    idtr: (u64, u16),
    /// The global-descriptor table register (`GDTR`): `(base, limit)`. The loader
    /// installs the boot GDT; the kernel reloads its own with `LGDT`.
    gdtr: (u64, u16),
    /// The task register (`TR`) selector + the TSS base (loaded by `LTR`); the
    /// `RSP0` field of the TSS gives the kernel stack on a ring transition.
    tr_base: u64,
    /// The model-specific registers the boot path touches beyond `EFER`
    /// (`FS_BASE`, `GS_BASE`, `KERNEL_GS_BASE`, `STAR`/`LSTAR`/`SFMASK` for
    /// `syscall`, the APIC base, …), keyed by the MSR number.
    msr: BTreeMap<u32, u64>,
    /// The minimal interrupt controller (legacy 8259 PIC pair + the local APIC
    /// the kernel uses) — enough to vector the PIT timer tick and the `virtio`
    /// IRQs so the boot completes.
    pic: Pic,
    /// The 8254 PIT channel-0 timer: a free-running source whose terminal counts
    /// raise IRQ 0 (the boot time base the kernel calibrates against).
    pit: Pit,
    /// The local APIC: the kernel switches the timer/IPI path to it; only the
    /// registers a UP boot drives are modelled (`SVR`, `EOI`, the LVT timer, the
    /// timer initial/current count + divide).
    lapic: Lapic,
    /// The I/O APIC (MMIO at `0xFEC0_0000`). Advertised via the ACPI MADT so Linux
    /// takes the *symmetric I/O* path — device IRQs route through the redirection
    /// table to the local APIC (not the legacy PIC), which lets the kernel run the
    /// tickless LAPIC timer instead of storming the periodic PIT.
    ioapic: Ioapic,
    /// The architected timestamp counter (`RDTSC`) — advanced in lockstep with the
    /// PIT so the kernel's delay loops and the TSC clocksource make progress.
    tsc: u64,
    /// A free-running step counter that paces the platform timers: they advance
    /// `true` once the machine has halted (a `hlt` with interrupts masked, the
    /// guest power-off) — the run loop then returns [`Halt::Halted`].
    halted: bool,
    /// SplitMix64 state for the hardware RNG (`RDRAND`/`RDSEED`). Seeded from a
    /// constant; each draw also mixes in the TSC so the stream is non-constant
    /// across the boot (the kernel only requires a varying value + `CF=1`).
    rng: u64,
    /// Free-running step counter pacing the PIT/APIC down-counters once per
    /// [`TICK_DIV`] steps (see [`Cpu::sys_tick`]).
    tdiv: u64,
    /// PCI CONFIG_ADDRESS latch (port 0xcf8). Readable so the kernel detects PCI
    /// config mechanism #1; the config-data port then returns all-ones (an empty
    /// bus — this machine's devices are virtio-mmio, not PCI).
    pci_addr: u32,
    /// Modelled L1 data-cache tags (one line number per set; direct-mapped). A miss
    /// charges [`DCACHE_MISS_CYCLES`] to the TSC, giving address-dependent access
    /// timing — the jitter `jitterentropy` needs on an otherwise deterministic core.
    dcache: Vec<u64>,
}

impl Sys {
    fn new() -> Self {
        Sys {
            uart: Uart {
                output: Vec::new(),
                input: Vec::new(),
                in_cursor: 0,
                lcr: 0,
                ier: 0,
                mcr: 0,
                scratch: 0,
                fcr: 0,
                divisor: 1,
                thre_pending: false,
                dbg: [0; 5],
            },
            virtio: None,
            virtio9p: None,
            virtionet: None,
            loopback: None,
            idtr: (0, 0),
            gdtr: (0, 0),
            tr_base: 0,
            msr: BTreeMap::new(),
            pic: Pic::new(),
            pit: Pit::new(),
            lapic: Lapic::new(),
            ioapic: Ioapic::new(),
            tsc: 0,
            halted: false,
            rng: 0x9e37_79b9_7f4a_7c15,
            tdiv: 0,
            pci_addr: 0,
            dcache: vec![u64::MAX; DCACHE_LINES],
        }
    }

    /// Assert an ISA IRQ line. Once Linux has programmed + unmasked the line's
    /// I/O APIC redirection entry (symmetric I/O mode), deliver its configured
    /// vector to the local APIC; until then (early boot, before `setup_IO_APIC`)
    /// fall through to the legacy 8259 PIC. Because Linux masks the PIC at the same
    /// time it unmasks the matching IOAPIC entry, a line is only ever deliverable
    /// through one path. The PIT (ISA IRQ0) maps to GSI 2 per the MADT interrupt
    /// source override; the old cascade IRQ2 swaps to GSI 0 (unused).
    fn raise_irq(&mut self, irq: u8) {
        let gsi = match irq {
            0 => 2,
            2 => 0,
            n => n as usize,
        };
        if gsi < 24 {
            let e = self.ioapic.redir[gsi];
            if e & (1 << 16) == 0 {
                // Unmasked: deliver this entry's vector to the local APIC.
                let level = e & (1 << 15) != 0;
                // A level line whose Remote IRR is still set (not yet EOI'd)
                // coalesces — no second delivery, exactly as the 82093AA.
                if level && e & (1 << 14) != 0 {
                    return;
                }
                self.lapic.set_irr((e & 0xff) as u8);
                if level {
                    self.ioapic.redir[gsi] |= 1 << 14; // latch Remote IRR
                }
                return;
            }
        }
        self.pic.raise(irq);
    }

    /// Acknowledge a local-APIC interrupt (write to the EOI register). Clears the
    /// highest in-service vector, then — for level-triggered I/O APIC sources —
    /// clears Remote IRR on every redirection entry carrying that vector, the
    /// LAPIC↔IOAPIC EOI broadcast a real chipset performs. Edge sources carry no
    /// Remote IRR, so this is a no-op for them.
    fn lapic_eoi(&mut self) {
        let v = Lapic::highest(&self.lapic.isr);
        self.lapic.eoi();
        if let Some(v) = v {
            for e in &mut self.ioapic.redir {
                if *e & ((1 << 15) | (1 << 14)) == ((1 << 15) | (1 << 14)) && (*e & 0xff) as u8 == v
                {
                    *e &= !(1 << 14);
                }
            }
        }
    }
}

/// A segment register's architectural cache: the selector plus the hidden base /
/// limit / attribute the descriptor loaded. In 64-bit mode `CS.L` selects long
/// mode and most bases are ignored (treated as 0) except `FS`/`GS`, whose bases
/// come from the `FS_BASE`/`GS_BASE` MSRs.
#[derive(Clone, Copy, Default)]
struct Seg {
    selector: u16,
    base: u64,
    /// Bit set from the descriptor's `L` (long-mode code) attribute — `CS.L`.
    long: bool,
}

/// The legacy 8259A PIC pair (master + slave). The boot path programs them
/// (ICW1..ICW4, the IRQ masks) before switching to the APIC; modelling the mask +
/// the in-service/request latches lets IRQ 0 (the PIT) and the `virtio` lines
/// vector through the IDT during early boot.
struct Pic {
    /// IRQ mask (1 = masked), master IRQs 0..8 in the low byte, slave 8..16 high.
    mask: u16,
    /// Pending (requested) IRQ lines.
    request: u16,
    /// The vector offset the master/slave were remapped to (ICW2).
    base_master: u8,
    base_slave: u8,
    /// ICW initialization sequence step per chip (0 = idle).
    init_master: u8,
    init_slave: u8,
}

impl Pic {
    fn new() -> Self {
        Pic {
            mask: 0xffff,
            request: 0,
            base_master: 0x08,
            base_slave: 0x70,
            init_master: 0,
            init_slave: 0,
        }
    }
    /// Raise IRQ line `n` (0..16) — latch it as requested.
    fn raise(&mut self, n: u8) {
        if n < 16 {
            self.request |= 1 << n;
        }
    }
    /// The highest-priority unmasked pending IRQ and its vector, or `None`.
    fn pending(&self) -> Option<(u8, u8)> {
        let active = self.request & !self.mask;
        if active == 0 {
            return None;
        }
        let irq = active.trailing_zeros() as u8;
        let vec = if irq < 8 {
            self.base_master.wrapping_add(irq)
        } else {
            self.base_slave.wrapping_add(irq - 8)
        };
        Some((irq, vec))
    }
    fn ack(&mut self, irq: u8) {
        self.request &= !(1 << irq);
    }
}

/// The 8254 PIT channel-0 timer. Only what the kernel's PIT clock-event needs is
/// modelled: the reload value and a step-driven down-counter that raises IRQ 0
/// each time it wraps (the periodic tick the scheduler/clocksource run on).
struct Pit {
    reload: u16,
    counter: u32,
    /// The latched low/high write toggle (the PIT is two byte-wide accesses).
    write_hi: bool,
    enabled: bool,
    /// Channel-0 mode: `true` = periodic (mode 2/3 — auto-reload, the legacy
    /// `HZ` tick), `false` = one-shot (mode 0 — fire once, then idle until the
    /// kernel reprograms a new deadline). The kernel's `clockevent` driver uses
    /// the PIT in one-shot mode and reprograms it from the tick handler; modelling
    /// that (rather than always auto-reloading) is what stops the tick handler's
    /// catch-up loop from re-firing forever and drowning the boot (`#12`).
    ch0_periodic: bool,
    /// Channel 2 — the speaker timer the kernel repurposes to *calibrate the TSC*
    /// (`pit_hpet_ptimer_calibrate_cpu`/`pit_calibrate_tsc`): it is gated on through
    /// port `0x61` bit 0, counts down once, and raises OUT2 (port `0x61` bit 5)
    /// when it reaches zero. The calibration busy-polls that OUT2 bit, so this must
    /// advance for the boot to get past TSC calibration (`#12`).
    ch2_reload: u16,
    ch2_counter: u32,
    ch2_write_hi: bool,
    /// The channel-2 gate (port `0x61` bit 0): counting only runs while it is set.
    ch2_gate: bool,
    /// OUT2 has gone high (the one-shot count expired) — reflected in `IN 0x61`
    /// bit 5, which the calibration loop waits on.
    ch2_out: bool,
}

impl Pit {
    fn new() -> Self {
        Pit {
            reload: 0,
            counter: 0,
            write_hi: false,
            enabled: false,
            ch0_periodic: true,
            ch2_reload: 0,
            ch2_counter: 0,
            ch2_write_hi: false,
            ch2_gate: false,
            ch2_out: false,
        }
    }
}

/// The local APIC (memory-mapped at the default `0xFEE0_0000`). Only the
/// registers a uniprocessor boot drives: the spurious-vector register (enable),
/// `EOI`, and the LVT timer + its initial/current count and divide — enough to
/// deliver the APIC-timer tick once the kernel migrates off the PIT.
struct Lapic {
    /// Spurious-interrupt vector register; bit 8 is the APIC software-enable.
    svr: u32,
    /// LVT timer entry: the vector (low 8 bits), mask (bit 16), mode (bit 17 =
    /// periodic).
    lvt_timer: u32,
    initial_count: u32,
    current_count: u32,
    divide: u32,
    /// The task-priority register — interrupts at or below `TPR` are deferred.
    tpr: u32,
    /// Interrupt Request Register — the 256-bit pending-interrupt bitmap (the LVT
    /// timer, and IPIs delivered through the ICR). A real local-APIC IRR.
    irr: [u64; 4],
    /// In-Service Register — vectors currently being handled, cleared on `EOI`.
    /// Provides interrupt priority/nesting exactly as the hardware LAPIC (and the
    /// AArch64 GIC active-state) do.
    isr: [u64; 4],
}

impl Lapic {
    fn new() -> Self {
        Lapic {
            svr: 0xff, // disabled at reset; vector 0xff
            lvt_timer: 1 << 16,
            initial_count: 0,
            current_count: 0,
            divide: 1,
            tpr: 0,
            irr: [0; 4],
            isr: [0; 4],
        }
    }
    fn enabled(&self) -> bool {
        self.svr & (1 << 8) != 0
    }
    /// Set an Interrupt Request Register bit (a pending interrupt vector).
    fn set_irr(&mut self, vec: u8) {
        self.irr[(vec >> 6) as usize] |= 1u64 << (vec & 63);
    }
    /// The highest set vector in a 256-bit register, or `None`.
    fn highest(reg: &[u64; 4]) -> Option<u8> {
        for w in (0..4).rev() {
            if reg[w] != 0 {
                return Some((w as u32 * 64 + (63 - reg[w].leading_zeros())) as u8);
            }
        }
        None
    }
    /// The highest pending vector deliverable now: its priority class (`vec >> 4`)
    /// must exceed both the TPR class and any in-service vector's class.
    fn deliverable(&self) -> Option<u8> {
        let v = Self::highest(&self.irr)?;
        let isr_class = u32::from(Self::highest(&self.isr).map_or(0, |i| i >> 4));
        if u32::from(v >> 4) > isr_class && u32::from(v >> 4) > (self.tpr >> 4) {
            Some(v)
        } else {
            None
        }
    }
    /// Move a delivered vector IRR -> ISR (in-service until `EOI`).
    fn take(&mut self, vec: u8) {
        self.irr[(vec >> 6) as usize] &= !(1u64 << (vec & 63));
        self.isr[(vec >> 6) as usize] |= 1u64 << (vec & 63);
    }
    /// End-of-interrupt: clear the highest in-service vector.
    fn eoi(&mut self) {
        if let Some(v) = Self::highest(&self.isr) {
            self.isr[(v >> 6) as usize] &= !(1u64 << (v & 63));
        }
    }
}

/// The I/O APIC (Intel 82093AA), MMIO at [`IOAPIC_BASE`]. A UP machine drives it
/// through the indirect IOREGSEL/IOWIN window: the 24-entry redirection table maps
/// each Global System Interrupt (GSI) to a destination vector, which we deliver to
/// the local APIC. Linux discovers it via the MADT and switches to symmetric I/O
/// mode — the path that arms the tickless LAPIC timer (no PIT storm).
struct Ioapic {
    /// IOAPIC ID (bits 27:24 of the ID register).
    id: u32,
    /// IOREGSEL latch — the indirect register index the next IOWIN access targets.
    ioregsel: u32,
    /// The 24 redirection-table entries (64-bit each). Reset masked (bit 16 set).
    redir: [u64; 24],
}

impl Ioapic {
    fn new() -> Self {
        Ioapic {
            id: 0,
            ioregsel: 0,
            // Every entry masked at reset (bit 16), as the 82093AA powers up.
            redir: [1 << 16; 24],
        }
    }
    /// Read the indirect register currently selected by IOREGSEL.
    fn read(&self) -> u32 {
        match self.ioregsel {
            0x00 => (self.id & 0xf) << 24, // ID
            // Version 0x20 in bits 7:0; "max redirection entry" = 23 (24 entries)
            // in bits 23:16 — Linux reads this to size the GSI space (GSI 0-23).
            0x01 => 0x0017_0020,
            0x02 => (self.id & 0xf) << 24, // arbitration ID
            i @ 0x10..=0x3f => {
                let n = ((i - 0x10) / 2) as usize;
                let e = self.redir[n];
                // Delivery Status (bit 12) is read-only-0; the rest reads back.
                let dword = if i & 1 == 0 {
                    e as u32
                } else {
                    (e >> 32) as u32
                };
                if i & 1 == 0 {
                    dword & !(1 << 12)
                } else {
                    dword
                }
            }
            _ => 0,
        }
    }
    /// Write the indirect register currently selected by IOREGSEL.
    fn write(&mut self, val: u32) {
        match self.ioregsel {
            0x00 => self.id = (val >> 24) & 0xf,
            i @ 0x10..=0x3f => {
                let n = ((i - 0x10) / 2) as usize;
                if i & 1 == 0 {
                    self.redir[n] = (self.redir[n] & 0xffff_ffff_0000_0000) | u64::from(val);
                } else {
                    self.redir[n] =
                        (self.redir[n] & 0x0000_0000_ffff_ffff) | (u64::from(val) << 32);
                }
            }
            _ => {} // version / arbitration are read-only
        }
    }
}

// The `virtio-mmio` transport slots the x86-64 core exposes. They sit in a
// dedicated high MMIO window (above any guest RAM the boot core sizes), so a
// physical access there is unambiguously a device, never RAM — the x86-64
// analogue of QEMU `microvm`'s `virtio-mmio` region. Each slot is the standard
// `virtio-mmio` register block; the devices behind them are the shared
// [`devbus`](super::devbus), identical to the other two cores (Law L4). Only the
// transport (this window + the interrupt path) is per-ISA.
const VIRTIO_BLK_BASE: u64 = 0xD000_0000;
const VIRTIO_BLK_END: u64 = 0xD000_0200;
/// The second `virtio-mmio` slot — the VirtIO **9P** device (the shared
/// workspace filesystem, `CC-15`), serviced by the shared `devbus`.
const VIRTIO_9P_BASE: u64 = 0xD000_0200;
const VIRTIO_9P_END: u64 = 0xD000_0400;
/// The third `virtio-mmio` slot — the VirtIO **network** device (`CC-16`): the
/// userspace TCP/IP NAT, serviced by the shared `devbus`.
const VIRTIO_NET_BASE: u64 = 0xD000_0400;
const VIRTIO_NET_END: u64 = 0xD000_0600;

/// The base of guest RAM (a flat physical address space; the boot core runs with
/// paging off / identity-mapped until the kernel installs its own page tables).
const RAM_BASE: u64 = 0x0;

// The legacy ISA IRQ lines the `virtio-mmio` devices vector through (the values
// the kernel learns from the `virtio_mmio.device=<size>@<base>:<irq>` command
// line). They sit on the slave 8259 so the boot path's IRQ delivery reaches the
// device drivers (`CC-45`).
const VIRTIO_BLK_IRQ: u8 = 11;
const VIRTIO_9P_IRQ: u8 = 10;
const VIRTIO_NET_IRQ: u8 = 12;

/// Emulator-steps per PIT/APIC down-counter decrement — one shared rate for every
/// platform timer. With the PIT at its architectural 1.193182 MHz, this fixes the
/// guest-time granularity at `TICK_DIV × 1.193182` steps per microsecond, which is
/// what bounds the cost of a busy-wait delay (`udelay`/`delay_tsc` and absent-device
/// poll timeouts spin in emulator steps proportional to it). Kept small so those
/// delays — and a long device-probe timeout — do not burn billions of steps, while
/// still large enough that the tick period (`reload × TICK_DIV` steps) far exceeds
/// the tick handler (no "timer storm"). One coherent rate keeps the kernel's
/// calibration and its tick in step. (See [`TSC_PER_STEP`] for `cpu_khz`.)
const TICK_DIV: u64 = 16;

/// TSC ticks advanced per emulator step. With [`TICK_DIV`] this sets the frequency
/// the kernel calibrates: `cpu_khz = TSC_PER_STEP × TICK_DIV × 1.193182 MHz`
/// (≈ 2.44 GHz here — a realistic CPU, so `udelay`'s cycle budget is sane). The TSC
/// advances every step for fine `sched_clock` resolution; only the down-counters
/// are paced by `TICK_DIV`, so this factor sets `cpu_khz` independently of the
/// delay-cost granularity above.
const TSC_PER_STEP: u64 = 128;

/// Lines in the modelled L1 data cache (direct-mapped, 64-byte lines → 32 KiB).
/// Smaller than the working sets entropy daemons deliberately stride across (the
/// kernel's `jitterentropy` buffer is 64 KiB), so their walks miss and the access
/// latency varies with the address pattern.
const DCACHE_LINES: usize = 512;
/// Extra TSC cycles charged on a data-cache miss — the microarchitectural timing
/// variance a real CPU exhibits, and that `jitterentropy` harvests as its noise
/// source. Without it the (otherwise perfectly deterministic) emulator gives a
/// constant per-access latency: jitterentropy's health test sees no jitter and the
/// crypto DRBG it seeds never initialises (the boot wedges before userspace).
const DCACHE_MISS_CYCLES: u64 = 32;

/// A direct-mapped software TLB entry: caches a virtual-page → physical-frame
/// translation so a hot loop does not re-walk the 4-level page table on every
/// access. An entry is live when its `gen` matches [`Cpu::tlb_gen`] (the global
/// flush counter, bumped on a `CR0`/`CR4` write), its `pgen` matches
/// [`Cpu::pcid_gen`]`[pcid]` (the per-PCID flush counter, bumped on a flushing
/// `CR3` load / `INVLPG`), and its `pcid` matches the active PCID — so a `CR3`
/// switch between address spaces (the kernel's ASID scheme, and `text_poke`'s
/// poking-mm ping-pong) keeps every PCID's translations instead of cold-flushing.
#[derive(Clone, Copy)]
struct TlbEntry {
    tag: u64,
    frame: u64,
    /// The effective page permissions (the AND of the R/W and U/S bits across the
    /// walked levels), cached so the fast path enforces write-protection (COW) and
    /// user/supervisor access exactly as a fresh walk would.
    writable: bool,
    user_ok: bool,
    /// Cleared by a single-page `INVLPG` (a precise invalidation) without disturbing
    /// the rest of the TLB — so a COW/unmap flush of one page does not cold-flush the
    /// whole address space, which is the difference between a warm and a perpetually
    /// cold TLB under a `fork`+`exec`-heavy userspace.
    valid: bool,
    pcid: u16,
    gen: u64,
    pgen: u64,
}

/// Direct-mapped TLB sets, indexed by the virtual page number.
const TLB_SETS: usize = 1024;

/// MMU instrumentation for the x86-64 core — pure counters, no architected effect.
/// Boot/perf diagnostics: a fault *storm* (a copy-on-write loop from a stale
/// software-TLB entry, a demand-paging loop) is otherwise invisible — the boot is
/// just "slow". These turn it into a number: compare against retired instructions
/// (`Cpu::insns`) to see whether the core is making progress or thrashing the MMU.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MmuStats {
    /// `#PF` with P=1: a write to a read-only page, or a user access to a supervisor
    /// page — copy-on-write, write-protect, and the user/supervisor boundary.
    pub protection_faults: u64,
    /// `#PF` with P=0: demand paging — a level of the walk was not-present.
    pub not_present_faults: u64,
    /// Software-TLB permission re-validations that resolved a stale denial (the
    /// coherence re-walk firing) — high counts mean the kernel is upgrading page
    /// permissions without a flush our TLB sees, which the re-walk then repairs.
    pub tlb_revalidations: u64,
    /// Page-table walks that filled a TLB entry (i.e. a software-TLB miss). If this
    /// approaches the retired-instruction count, the working set is thrashing the
    /// direct-mapped TLB and every access is paying a 4-level walk.
    pub tlb_fills: u64,
}

/// The x86-64 long-mode integer core.
pub struct Cpu {
    /// The 16 general-purpose registers (`rax`,`rcx`,`rdx`,`rbx`,`rsp`,`rbp`,
    /// `rsi`,`rdi`,`r8`..`r15`).
    r: [u64; 16],
    /// The instruction pointer.
    rip: u64,
    /// The flags register (`RFLAGS`).
    rflags: u64,
    /// Retired-instruction counter — a monotonic count of guest instructions the
    /// core has executed (informational; the x86-64 analogue of the RISC-V
    /// `INSTRET` CSR).
    insns: u64,
    /// Delivered-interrupt histogram, indexed by vector — a dev differential
    /// against qemu's `-d int` (e.g. v=0xec LAPIC timer, v=0x24 COM1/IRQ4, v=0x0e
    /// `#PF`). A starved timer count means the scheduler is not ticking.
    int_counts: Box<[u64; 256]>,
    /// MMU instrumentation (counters only — no effect on the architected state).
    /// Permanent boot/perf diagnostics: a fault *storm* (e.g. a copy-on-write loop
    /// from a stale software-TLB entry, or a demand-paging loop) shows up as one of
    /// these counters exploding relative to retired instructions — turning
    /// "the boot is mysteriously slow" into a measured number, not a guess.
    mmu_stats: MmuStats,
    /// Control registers: `cr0` (paging/protection), `cr2` (page-fault address),
    /// `cr3` (the PML4 physical base), `cr4` (PAE et al.).
    cr0: u64,
    cr2: u64,
    cr3: u64,
    cr4: u64,
    /// `IA32_EFER` — `LME`/`LMA` (long mode enabled/active) live here.
    efer: u64,
    /// The debug registers `DR0..DR7`. `cpu_init` zeroes them on every CPU bring-up
    /// (`MOV DRn, r` / `MOV r, DRn` — opcodes `0F 23` / `0F 21`); no hardware
    /// breakpoints are armed during the boot, so they are plain storage that reads
    /// back what was written (DR4/DR5 alias DR6/DR7 architecturally, immaterial here).
    dr: [u64; 8],
    /// The 16 **SSE** registers `XMM0..XMM15` (128-bit each). The x86-64 baseline
    /// ISA mandates SSE2, so every stock `linux-amd64` binary uses them: glibc's
    /// IFUNC-selected string/memory routines (`memcpy`/`memset`/`strlen`/`memchr`
    /// …) are SSE2, and `movd`/`movq`/`movdqa` appear from process startup. The
    /// integer + data-movement SSE/SSE2 instructions operate on these (`sse_0f`).
    xmm: [u128; 16],
    /// The six segment registers (`ES,CS,SS,DS,FS,GS` — indexed by [`SegId`]).
    /// In long mode the bases are 0 except `FS`/`GS`; `CS.long` selects 64-bit.
    seg: [Seg; 6],
    /// The current privilege level (`CPL`, `CS.RPL`): 0 in the kernel, 3 in
    /// userspace — selects the syscall/interrupt stack switch.
    cpl: u8,
    /// The segment override in effect for the instruction being decoded (an `FS`
    /// (`0x64`) / `GS` (`0x65`) prefix), so a memory operand adds that segment's
    /// base — the kernel's per-CPU `%gs:` and the userspace `%fs:` accesses.
    /// Reset at the start of every instruction.
    cur_seg: Option<SegId>,
    /// Whether a `REX` prefix is present on the instruction being decoded. With
    /// no `REX`, a byte register operand `4..=7` selects the **high byte**
    /// (`AH`/`CH`/`DH`/`BH`); with `REX` it selects the low byte of
    /// `RSP`/`RBP`/`RSI`/`RDI` (`SPL`/`BPL`/`SIL`/`DIL`).
    rex_present: bool,
    /// Guest RAM (physical, based at [`RAM_BASE`]).
    ram: Vec<u8>,
    /// A page fault latched mid-instruction by the MMU (a not-present page-table
    /// walk while paging is on). The faulting linear address and the `#PF` error
    /// code; [`Cpu::step`] restores the pre-instruction register state and
    /// vectors `#PF` (the kernel's early `do_early_exception` → `early_make_pgtable`
    /// lazily maps boot data through `early_top_pgt` exactly as on real hardware).
    fault: Option<PageFault>,
    /// A direct-mapped software TLB over [`Cpu::translate_acc`] (see [`TlbEntry`]).
    tlb: Vec<TlbEntry>,
    tlb_gen: u64,
    /// Inline instruction-fetch translation cache. A single instruction decodes
    /// through several `fetch_u8` calls (prefixes, opcode, ModRM/SIB, displacement,
    /// immediate) — almost always on one code page — and every byte would otherwise
    /// re-run [`translate_acc`]'s TLB lookup. This caches the *current code page's*
    /// VA→frame translation so the bytes after the first are a direct RAM read. It is
    /// validated against `tlb_gen` + the active PCID's `pcid_gen` (so any TLB/paging
    /// flush invalidates it for free) and is purely a read-through accelerator over
    /// `translate_acc` — portable (no JIT/`std` dependency). `ifetch_gen == 0` (the
    /// reserved pre-flush generation, real generations start at 1) means "empty".
    ifetch_tag: u64,
    ifetch_frame: u64,
    ifetch_gen: u64,
    ifetch_pgen: u64,
    ifetch_pcid: u16,
    /// Per-PCID TLB generations (`CR4.PCIDE`). A `CR3` load without the no-flush
    /// hint bumps only the loaded PCID's generation, so the other address spaces'
    /// cached translations survive — the kernel reuses a handful of ASIDs and
    /// `text_poke` ping-pongs between the kernel mm and a temporary poking-mm.
    /// Indexed by the 12-bit PCID.
    pcid_gen: Vec<u64>,
    sys: Option<Box<Sys>>,
}

/// A page fault latched by the MMU during an instruction's memory access (`#12`):
/// the faulting linear address (→ `CR2`) and the architectural error code.
#[derive(Clone, Copy)]
struct PageFault {
    addr: u64,
    error: u64,
}

/// `#PF` (page fault) — IDT vector 14.
const VEC_PAGE_FAULT: u8 = 14;
/// `#PF` error-code bits: P (the fault was a protection violation, not
/// not-present), W/R (write), U/S (user-mode access).
const PF_ERR_PRESENT: u64 = 1 << 0;
const PF_ERR_WRITE: u64 = 1 << 1;
const PF_ERR_USER: u64 = 1 << 2;

// Register indices.
const RAX: usize = 0;
const RCX: usize = 1;
const RDX: usize = 2;
const RBX: usize = 3;
const RSP: usize = 4;
const RBP: usize = 5;
const RSI: usize = 6;
const RDI: usize = 7;

/// Segment-register indices (the SReg field encoding: ES,CS,SS,DS,FS,GS).
#[derive(Clone, Copy)]
enum SegId {
    Es = 0,
    Cs = 1,
    Ss = 2,
    Ds = 3,
    Fs = 4,
    Gs = 5,
}

/// `RFLAGS.IF` — the interrupt-enable flag (`STI`/`CLI`).
const RFLAGS_IF: u64 = 1 << 9;
/// `CR4.PCIDE` — process-context identifiers enable.
const CR4_PCIDE: u64 = 1 << 17;
/// `CR0.WP` — write protect. When set, even a supervisor (CPL 0) write to a
/// read-only page faults; the kernel sets it so copy-on-write is enforced for its
/// own `copy_*_user` accesses to user pages, not only for CPL 3.
const CR0_WP: u64 = 1 << 16;
/// `RFLAGS.DF` — the direction flag (string-op increment/decrement).
const RFLAGS_DF: u64 = 1 << 10;
/// `RFLAGS.AC` — the alignment-check / access-control flag. Under `CR4.SMAP`, a
/// supervisor-mode access to a user page is permitted only while `AC = 1`; the
/// kernel sets it with `STAC` around deliberate user accesses (`copy_*_user`,
/// `clear_user`/`padzero`, …) and clears it with `CLAC`. Honouring `STAC`/`CLAC`
/// is required for any SMAP-enabled kernel to load and run a userspace binary.
const RFLAGS_AC: u64 = 1 << 18;

// The model-specific registers the long-mode boot path drives.
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_FS_BASE: u32 = 0xC000_0100;
const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;
const MSR_APIC_BASE: u32 = 0x1B;
/// `IA32_ARCH_CAPABILITIES` — the read-only MSR reporting which speculative-execution
/// vulnerability classes the CPU is immune to. A faithful, non-speculative emulator
/// is immune to all of them; reporting that lets the kernel skip the mitigations
/// (notably PTI), matching the qemu oracle.
const MSR_IA32_ARCH_CAPABILITIES: u32 = 0x10A;
const RDCL_NO: u64 = 1 << 0; // not vulnerable to Rogue Data Cache Load (Meltdown) → no PTI
const IBRS_ALL: u64 = 1 << 1; // enhanced IBRS always on (no retpoline needed)
const SSB_NO: u64 = 1 << 4; // not vulnerable to Speculative Store Bypass
const MDS_NO: u64 = 1 << 5; // not vulnerable to Microarchitectural Data Sampling
const IF_PSCHANGE_MC_NO: u64 = 1 << 6; // no instruction-fetch page-size-change MCE
const TAA_NO: u64 = 1 << 8; // not vulnerable to TSX Async Abort

// The local-APIC MMIO window (the architectural default base).
const LAPIC_BASE: u64 = 0xFEE0_0000;
const LAPIC_END: u64 = 0xFEE0_1000;
/// The I/O APIC MMIO page (IOREGSEL at +0x00, IOWIN at +0x10).
const IOAPIC_BASE: u64 = 0xFEC0_0000;
const IOAPIC_END: u64 = 0xFEC0_1000;

impl Cpu {
    /// A fresh core with `ram_bytes` of zeroed RAM and `rip`/`rsp` reset.
    #[must_use]
    pub fn new(ram_bytes: usize) -> Self {
        Cpu {
            r: [0; 16],
            rip: RAM_BASE,
            rflags: 0x2, // bit 1 is reserved-1
            insns: 0,
            int_counts: Box::new([0; 256]),
            mmu_stats: MmuStats::default(),
            dr: [0; 8],
            xmm: [0; 16],
            cr0: 0,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            efer: 0,
            seg: [Seg::default(); 6],
            cpl: 0,
            cur_seg: None,
            rex_present: false,
            ram: vec![0u8; ram_bytes],
            fault: None,
            tlb: vec![
                TlbEntry {
                    tag: 0,
                    frame: 0,
                    writable: false,
                    user_ok: false,
                    valid: false,
                    pcid: 0,
                    gen: 0,
                    pgen: 0,
                };
                TLB_SETS
            ],
            tlb_gen: 1,
            ifetch_tag: 0,
            ifetch_frame: 0,
            ifetch_gen: 0,
            ifetch_pgen: 0,
            ifetch_pcid: 0,
            pcid_gen: vec![1; 4096],
            sys: Some(Box::new(Sys::new())),
        }
    }

    /// Whether 4-level paging is active (long mode: `CR0.PG` & `CR4.PAE` &
    /// `EFER.LME`). When off, virtual addresses are physical (the boot core runs
    /// identity-mapped until the kernel installs `CR3`).
    fn paging(&self) -> bool {
        const PG: u64 = 1 << 31;
        const PAE: u64 = 1 << 5;
        const LME: u64 = 1 << 8;
        self.cr0 & PG != 0 && self.cr4 & PAE != 0 && self.efer & LME != 0
    }

    /// Translate a linear address to a physical one through the 4-level page
    /// tables (`PML4 → PDPT → PD → PT`), honouring 2 MiB and 1 GiB large pages.
    /// Returns the linear address unchanged when paging is off; a not-present
    /// entry falls back to the linear address. A non-faulting view for tooling
    /// and tests ([`Cpu::vv_dbg`], the paging unit test); the executing core
    /// translates through [`Cpu::translate_acc`], which delivers a `#PF` instead.
    fn translate(&self, vaddr: u64) -> u64 {
        if !self.paging() {
            return vaddr;
        }
        self.walk(vaddr).map_or(vaddr, |(pa, _, _, _)| pa)
    }

    /// Walk the 4-level page tables for `vaddr`, returning the physical address or
    /// the `#PF` error code for the first not-present level (the page-fault path a
    /// real long-mode boot takes — the kernel maps boot data lazily on `#PF`).
    /// `write`/`user` shape the error code; when paging is off the address is
    /// physical (the identity-mapped boot core before it installs `CR3`).
    fn walk(&self, vaddr: u64) -> Result<(u64, bool, bool, u64), ()> {
        // The caller (`translate_acc`/`translate`) checks paging is on. The returned
        // tuple is `(physical address, writable, user-accessible, leaf-entry phys
        // addr)` — the effective R/W and U/S permissions are the AND of those bits
        // across every walked level (so the caller can enforce write-protection and
        // the user/supervisor boundary), and the leaf address lets it set the
        // Accessed/Dirty bits like real hardware. `Err(())` means a level was
        // not-present.
        let pml4 = self.cr3 & 0x000f_ffff_ffff_f000;
        let idx = |lvl: u32| ((vaddr >> (12 + 9 * lvl)) & 0x1ff) * 8;
        let ent = |base: u64, i: u64| self.rd_phys(base + i, 8);
        let present = |e: u64| e & 1 != 0;
        let next = |e: u64| e & 0x000f_ffff_ffff_f000;
        let rw = |e: u64| (e >> 1) & 1 != 0;
        let us = |e: u64| (e >> 2) & 1 != 0;

        let e4 = ent(pml4, idx(3));
        if !present(e4) {
            return Err(());
        }
        let (mut w, mut u) = (rw(e4), us(e4));
        let a3 = next(e4) + idx(2);
        let e3 = ent(next(e4), idx(2));
        if !present(e3) {
            return Err(());
        }
        w &= rw(e3);
        u &= us(e3);
        if e3 & (1 << 7) != 0 {
            // 1 GiB page
            return Ok((
                (e3 & 0x000f_ffff_c000_0000) | (vaddr & 0x3fff_ffff),
                w,
                u,
                a3,
            ));
        }
        let a2 = next(e3) + idx(1);
        let e2 = ent(next(e3), idx(1));
        if !present(e2) {
            return Err(());
        }
        w &= rw(e2);
        u &= us(e2);
        if e2 & (1 << 7) != 0 {
            // 2 MiB page
            return Ok(((e2 & 0x000f_ffff_ffe0_0000) | (vaddr & 0x1f_ffff), w, u, a2));
        }
        let a1 = next(e2) + idx(0);
        let e1 = ent(next(e2), idx(0));
        if !present(e1) {
            return Err(());
        }
        w &= rw(e1);
        u &= us(e1);
        Ok(((e1 & 0x000f_ffff_ffff_f000) | (vaddr & 0xfff), w, u, a1))
    }

    /// Set the Accessed (and, for a write, Dirty) bit in a leaf paging-structure
    /// entry at physical `leaf` — what x86 hardware does on a translation. Linux
    /// depends on the Dirty bit; without it, it falls back to write-protect dirty
    /// tracking (re-marking a page read-only after each write to catch the next),
    /// which makes every write a `#PF` — a fault storm that stalls a real boot. Done
    /// only when a translation fills the TLB (not per access), so it adds no
    /// hot-path cost: a physical read + conditional 8-byte write.
    fn set_accessed_dirty(&mut self, leaf: u64, write: bool) {
        let pte = self.rd_phys(leaf, 8);
        let want = pte | (1 << 5) | if write { 1 << 6 } else { 0 };
        if want != pte {
            let a = leaf as usize;
            if a + 8 <= self.ram.len() {
                self.ram[a..a + 8].copy_from_slice(&want.to_le_bytes());
            }
        }
    }

    /// Translate a linear address for the executing core, latching a [`PageFault`]
    /// (the first level that was not-present) when the walk fails so [`Cpu::step`]
    /// can roll the instruction back and vector `#PF`. Until the fault is taken,
    /// returns a benign physical address (`0`) so the in-flight access reads/writes
    /// harmlessly; the instruction is discarded and restarted after the handler
    /// maps the page. `write` and `user` set the error-code bits.
    ///
    /// Physical `0` is deliberately below every device MMIO window
    /// (`VIRTIO_BLK_BASE` = `0xD000_0000`, the local APIC at `0xFEE0_0000`), so a
    /// faulting access never resolves to a device — `rd`/`wr` take no MMIO side
    /// effect on a fault, only a harmless phys-0 scratch read/write that the
    /// instruction restart overwrites. This phys-0 scratch is load-bearing for the
    /// early boot's demand-paging: removing it (returning early from `rd`/`wr`)
    /// regresses the boot to a hang, so the scratch access is kept, not elided.
    fn translate_acc(&mut self, vaddr: u64, write: bool, user: bool) -> u64 {
        if self.fault.is_some() {
            return 0; // a fault is already pending; do not double-latch
        }
        // Fast path: the software TLB caches present translations so a hot loop
        // does not re-walk the 4-level page table on every access (the dominant
        // interpreter cost). It holds only successful (present) walks; a miss or a
        // not-present page falls through to the full walk below.
        if !self.paging() {
            return vaddr;
        }
        let pcid = self.active_pcid();
        let page = vaddr & !0xfff;
        let set = (page >> 12) as usize & (TLB_SETS - 1);
        let wp = self.cr0 & CR0_WP != 0;
        // The access is denied if a user access hits a supervisor page, or a write
        // hits a read-only page (when CPL=3 or CR0.WP=1). This is what makes
        // copy-on-write (and so `fork`) work.
        let denied = |writable: bool, user_ok: bool| {
            (user && !user_ok) || (write && !writable && (user || wp))
        };
        // Resolve the frame + its effective permissions, from the TLB or a walk. A
        // fresh walk also yields the leaf-entry address; a TLB hit yields `None`, so
        // the A/D bits are touched only when a translation *fills* the TLB (the hot
        // path adds no cost).
        let mut from_tlb = false;
        let (frame, writable, user_ok, walk_leaf) = {
            let e = self.tlb[set];
            if e.valid
                && e.gen == self.tlb_gen
                && e.pcid as usize == pcid
                && e.pgen == self.pcid_gen[pcid]
                && e.tag == page
            {
                from_tlb = true;
                (e.frame, e.writable, e.user_ok, None)
            } else {
                match self.walk(vaddr) {
                    Ok((pa, w, u, lf)) => {
                        self.mmu_stats.tlb_fills += 1;
                        self.tlb[set] = TlbEntry {
                            tag: page,
                            frame: pa & !0xfff,
                            writable: w,
                            user_ok: u,
                            valid: true,
                            pcid: pcid as u16,
                            gen: self.tlb_gen,
                            pgen: self.pcid_gen[pcid],
                        };
                        (pa & !0xfff, w, u, Some(lf))
                    }
                    Err(()) => {
                        // Not-present (P=0): the kernel's #PF handler maps the page.
                        self.mmu_stats.not_present_faults += 1;
                        let error = (if write { PF_ERR_WRITE } else { 0 })
                            | (if user { PF_ERR_USER } else { 0 });
                        self.fault = Some(PageFault { addr: vaddr, error });
                        return 0;
                    }
                }
            }
        };
        if denied(writable, user_ok) {
            // A TLB-cached permission can be stale: the kernel may upgrade a page
            // RO→RW (COW / dirty-bit / access-flag set) and then — seeing the live
            // PTE already permits the access — never re-flush, so a stale software-TLB
            // entry would fault forever. The page tables are the source of truth: on a
            // denial sourced from the TLB, re-walk to confirm before faulting (qemu's
            // hardware TLB is always coherent; ours must be made so on the fault edge).
            if from_tlb {
                if let Ok((pa, w, u, lf)) = self.walk(vaddr) {
                    self.tlb[set] = TlbEntry {
                        tag: page,
                        frame: pa & !0xfff,
                        writable: w,
                        user_ok: u,
                        valid: true,
                        pcid: pcid as u16,
                        gen: self.tlb_gen,
                        pgen: self.pcid_gen[pcid],
                    };
                    if !denied(w, u) {
                        self.mmu_stats.tlb_revalidations += 1;
                        self.set_accessed_dirty(lf, write);
                        return (pa & !0xfff) | (vaddr & 0xfff);
                    }
                }
            }
            self.mmu_stats.protection_faults += 1;
            let error = PF_ERR_PRESENT
                | (if write { PF_ERR_WRITE } else { 0 })
                | (if user { PF_ERR_USER } else { 0 });
            self.fault = Some(PageFault { addr: vaddr, error });
            return 0;
        }
        // The access is permitted. On a TLB fill (a fresh walk), set the Accessed
        // (and Dirty, for a write) bit in the leaf entry — like real hardware — so
        // Linux's dirty/aging machinery works and it does not fall back to the
        // write-protect dirty-tracking fault storm. TLB hits skip this (no cost).
        if let Some(lf) = walk_leaf {
            self.set_accessed_dirty(lf, write);
        }
        frame | (vaddr & 0xfff)
    }

    /// Flush the whole software TLB (bump the global generation) — the architected
    /// flush on a `CR0`/`CR4` write (a change of paging regime).
    fn flush_tlb(&mut self) {
        self.tlb_gen = self.tlb_gen.wrapping_add(1);
    }

    /// Flush the software TLB entries for a single PCID — a `CR3` load without the
    /// no-flush hint, or `INVLPG` for the active address space.
    fn flush_pcid(&mut self, pcid: usize) {
        self.pcid_gen[pcid] = self.pcid_gen[pcid].wrapping_add(1);
    }

    /// The active PCID: `CR3[11:0]` when `CR4.PCIDE` is enabled, else 0.
    fn active_pcid(&self) -> usize {
        if self.cr4 & CR4_PCIDE != 0 {
            (self.cr3 & 0xfff) as usize
        } else {
            0
        }
    }

    /// Account a RAM access in the modelled L1 data cache: a miss installs the line
    /// (evicting its set) and adds [`DCACHE_MISS_CYCLES`] to the TSC, so the access
    /// latency varies with the address pattern — real microarchitectural jitter.
    fn dcache_touch(&mut self, pa: u64) {
        let line = pa >> 6;
        let idx = (line as usize) & (DCACHE_LINES - 1);
        if let Some(sys) = self.sys.as_mut() {
            if sys.dcache[idx] != line {
                sys.dcache[idx] = line;
                sys.tsc = sys.tsc.wrapping_add(DCACHE_MISS_CYCLES);
            }
        }
    }

    /// Read control register `idx` (0/2/3/4; others read 0).
    fn cr(&self, idx: usize) -> u64 {
        match idx {
            0 => self.cr0,
            2 => self.cr2,
            3 => self.cr3,
            4 => self.cr4,
            _ => 0,
        }
    }

    /// Write control register `idx`.
    fn set_cr(&mut self, idx: usize, val: u64) {
        #[cfg(feature = "cc44-trace")]
        if idx == 3 && self.cr3 != val && TP_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[cc44-trace] CR3-SWITCH {:#x} -> {val:#x} rip={:#x}",
                self.cr3,
                self.rip,
            );
        }
        match idx {
            0 => self.cr0 = val,
            2 => self.cr2 = val,
            // Bit 63 of a CR3 load is the transient no-flush hint, not retained.
            3 => self.cr3 = val & !(1u64 << 63),
            4 => self.cr4 = val,
            _ => {}
        }
        match idx {
            // CR0 (PG/WP) and CR4 (PAE/PGE/PCIDE/LA57) change the paging regime.
            0 | 4 => self.flush_tlb(),
            // CR3: with PCID a no-flush load (bit 63) keeps every PCID's entries;
            // otherwise it flushes only the PCID being loaded. Without PCIDE a CR3
            // load flushes the whole TLB, as on real hardware.
            3 => {
                if self.cr4 & CR4_PCIDE != 0 {
                    if val & (1u64 << 63) == 0 {
                        self.flush_pcid((val & 0xfff) as usize);
                    }
                } else {
                    self.flush_tlb();
                }
            }
            _ => {}
        }
    }

    /// A raw physical read (the page-table walk reads physical memory directly).
    fn rd_phys(&self, addr: u64, size: u8) -> u64 {
        let a = addr as usize;
        let mut v = 0u64;
        for i in 0..size as usize {
            v |= u64::from(*self.ram.get(a + i).unwrap_or(&0)) << (8 * i);
        }
        v
    }

    /// Read `size` bytes from a *linear* (virtual) address, walking the page
    /// tables — the descriptor-table reads (`IDT`, `TSS`) the CPU does on its own
    /// behalf during interrupt delivery. The `IDTR`/`GDTR`/`TR` bases the kernel
    /// loads are kernel virtual addresses (e.g. `0xffffffff8347e000`), so reading
    /// them as raw physical offsets would return zero and vector every fault to
    /// `RIP 0`. A non-faulting walk (the CPU's own table access never page-faults
    /// against the kernel's permanently-mapped descriptor tables).
    fn rd_virt(&self, vaddr: u64, size: u8) -> u64 {
        let pa = self.sys.as_ref().map_or(vaddr, |_| self.translate(vaddr));
        self.rd_phys(pa, size)
    }

    /// Load a flat code/image blob at guest physical `addr` and set `rip` to it —
    /// the instruction-conformance entry (the analogue of loading a riscv-test).
    pub fn load_at(&mut self, addr: u64, image: &[u8]) {
        let a = addr as usize;
        let n = image.len().min(self.ram.len().saturating_sub(a));
        self.ram[a..a + n].copy_from_slice(&image[..n]);
        self.rip = addr;
        self.r[RSP] = self.ram.len() as u64; // a stack at the top of RAM
    }

    /// The console bytes the guest has written to the serial port.
    #[must_use]
    pub fn console(&self) -> &[u8] {
        self.sys.as_ref().map_or(&[], |s| &s.uart.output)
    }

    /// Feed terminal input to the guest's serial console (readable at `0x3f8`).
    pub fn feed_console(&mut self, bytes: &[u8]) {
        if let Some(sys) = self.sys.as_mut() {
            sys.uart.input.extend_from_slice(bytes);
            // Raise the receive interrupt if the driver runs input interrupt-driven.
            if sys.uart.ier & 0x01 != 0 {
                sys.raise_irq(4);
            }
        }
    }

    /// Read a general-purpose register (for tests / introspection).
    #[must_use]
    pub fn reg(&self, i: usize) -> u64 {
        self.r[i & 15]
    }

    /// `RFLAGS` (for tests / introspection).
    #[must_use]
    pub fn rflags(&self) -> u64 {
        self.rflags
    }

    /// MMU instrumentation snapshot (protection / not-present faults, TLB
    /// re-validations) — boot/perf diagnostics; see [`MmuStats`].
    #[must_use]
    pub fn mmu_stats(&self) -> MmuStats {
        self.mmu_stats
    }

    /// dev-only UART access counters `[THR-write, IER-write, IIR-read, LSR-read,
    /// IRQ4-raise]` — diagnoses the interrupt-driven console TX path.
    #[must_use]
    pub fn uart_dbg(&self) -> [u64; 5] {
        self.sys.as_ref().map_or([0; 5], |s| s.uart.dbg)
    }

    /// Count of interrupts/exceptions delivered for a given IDT vector — a
    /// differential against qemu's `-d int` histogram, read from the per-vector
    /// `int_counts` table.
    #[must_use]
    pub fn int_count(&self, vector: u8) -> u64 {
        self.int_counts[vector as usize]
    }

    /// `rip` (for tests / introspection).
    #[must_use]
    pub fn rip(&self) -> u64 {
        self.rip
    }

    /// Read `n` bytes of guest virtual memory — a debug peek (e.g. to dump the bytes
    /// of a faulting instruction during emulator bring-up).
    #[must_use]
    pub fn peek(&self, vaddr: u64, n: usize) -> Vec<u8> {
        (0..n as u64)
            .map(|i| self.rd_virt(vaddr + i, 1) as u8)
            .collect()
    }

    /// Retired guest-instruction count — a monotonic tally of executed
    /// instructions (the x86-64 analogue of RISC-V `INSTRET`); informational.
    #[must_use]
    pub fn insns(&self) -> u64 {
        self.insns
    }

    /// Advance the PIT (channel 0 + the channel-2 calibration gate) and the LAPIC
    /// timer down-counters by one tick, raising the PIC/LAPIC interrupt when a
    /// counter expires (reloading periodic timers). The per-`TICK_DIV` body of
    /// [`sys_tick`](Self::sys_tick) — the platform timer cadence a real Linux boot
    /// calibrates against and uses for jiffies.
    fn tick_down_counters(&mut self) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        if sys.pit.ch2_gate && !sys.pit.ch2_out && sys.pit.ch2_counter > 0 {
            sys.pit.ch2_counter = sys.pit.ch2_counter.saturating_sub(1);
            if sys.pit.ch2_counter == 0 {
                sys.pit.ch2_out = true;
            }
        }
        if sys.pit.enabled && sys.pit.reload != 0 {
            if sys.pit.counter == 0 {
                sys.pit.counter = u32::from(sys.pit.reload);
            }
            sys.pit.counter = sys.pit.counter.saturating_sub(1);
            if sys.pit.counter == 0 {
                sys.raise_irq(0);
                if sys.pit.ch0_periodic {
                    sys.pit.counter = u32::from(sys.pit.reload);
                } else {
                    sys.pit.enabled = false;
                }
            }
        }
        // The local-APIC timer counts down whenever it is armed (current_count !=
        // 0), independent of the LVT mask — Linux calibrates it MASKED (arms a
        // count, masks the LVT to suppress interrupts, then measures the decrement),
        // so gating the counter on the mask made calibration read zero counts and
        // print "APIC frequency too slow, disabling apic timer", forcing the PIT.
        // The mask gates only interrupt DELIVERY. Reload on expiry only in periodic
        // mode (bit 17); one-shot stays stopped until the kernel re-arms it (0x380),
        // which is the tickless regime that replaces the periodic PIT tick.
        if sys.lapic.enabled() && sys.lapic.current_count != 0 {
            sys.lapic.current_count -= 1;
            if sys.lapic.current_count == 0 {
                if sys.lapic.lvt_timer & (1 << 16) == 0 {
                    sys.lapic.set_irr((sys.lapic.lvt_timer & 0xff) as u8);
                }
                if sys.lapic.lvt_timer & (1 << 17) != 0 {
                    sys.lapic.current_count = sys.lapic.initial_count;
                }
            }
        }
    }

    // ── Memory ───────────────────────────────────────────────────────────────
    fn rd(&mut self, addr: u64, size: u8) -> u64 {
        let user = self.cpl == 3;
        let pa = self.translate_acc(addr, false, user);
        if (VIRTIO_BLK_BASE..VIRTIO_NET_END).contains(&pa) {
            return self.mmio_read(pa, size as usize);
        }
        if (LAPIC_BASE..LAPIC_END).contains(&pa) {
            return u64::from(self.lapic_read((pa - LAPIC_BASE) as u32));
        }
        if (IOAPIC_BASE..IOAPIC_END).contains(&pa) {
            return u64::from(self.ioapic_read((pa - IOAPIC_BASE) as u32));
        }
        self.dcache_touch(pa);
        // Same-page accesses (the common case) map to contiguous physical bytes
        // `pa..pa+size`; the first-byte translate above filled the TLB + A/D bits, so
        // the rest need no re-translation — byte-identical to the per-byte loop, one
        // translation instead of `size`. Only a page crossing falls back to per-byte.
        let last = addr.wrapping_add(u64::from(size).saturating_sub(1));
        let same_page = (addr >> 12) == (last >> 12);
        let mut v = 0u64;
        if same_page {
            for i in 0..u64::from(size) {
                v |= u64::from(*self.ram.get((pa + i) as usize).unwrap_or(&0)) << (8 * i);
            }
        } else {
            for i in 0..u64::from(size) {
                let p = self.translate_acc(addr.wrapping_add(i), false, user) as usize;
                v |= u64::from(*self.ram.get(p).unwrap_or(&0)) << (8 * i);
            }
        }
        #[cfg(feature = "cc44-trace")]
        if addr == 0xffff_ffff_827b_b1e8 && TP_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[cc44-trace] RD vmemmap_base va={addr:#x} -> pa={pa:#x} val={v:#x} rip={:#x}",
                self.rip,
            );
        }
        v
    }

    fn wr(&mut self, addr: u64, size: u8, val: u64) {
        let user = self.cpl == 3;
        let pa = self.translate_acc(addr, true, user);
        if (VIRTIO_BLK_BASE..VIRTIO_NET_END).contains(&pa) {
            self.mmio_write(pa, size as usize, val);
            return;
        }
        if (LAPIC_BASE..LAPIC_END).contains(&pa) {
            self.lapic_write((pa - LAPIC_BASE) as u32, val as u32);
            return;
        }
        if (IOAPIC_BASE..IOAPIC_END).contains(&pa) {
            self.ioapic_write((pa - IOAPIC_BASE) as u32, val as u32);
            return;
        }
        self.dcache_touch(pa);
        #[cfg(feature = "cc44-trace")]
        if TP_ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
            && (0xffff_ffff_82a0_3e38..=0xffff_ffff_82a0_3e48).contains(&addr)
        {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[cc44-trace] POKE-WR va={addr:#x} -> pa={pa:#x} size={size} val={val:#x} cr3={:#x} rip={:#x}",
                self.cr3,
                self.rip,
            );
            let _ = std::io::stderr().flush();
        }
        let last = addr.wrapping_add(u64::from(size).saturating_sub(1));
        if (addr >> 12) == (last >> 12) {
            // Same-page store: contiguous physical bytes, one translation (above).
            for i in 0..u64::from(size) {
                if let Some(b) = self.ram.get_mut((pa + i) as usize) {
                    *b = (val >> (8 * i)) as u8;
                }
            }
        } else {
            for i in 0..u64::from(size) {
                let p = self.translate_acc(addr.wrapping_add(i), true, user) as usize;
                if let Some(b) = self.ram.get_mut(p) {
                    *b = (val >> (8 * i)) as u8;
                }
            }
        }
    }

    fn fetch_u8(&mut self) -> u8 {
        let vaddr = self.rip;
        // Inline code-page cache: the bytes after the first in an instruction are on
        // the same code page, so reuse its VA→frame translation (validated against the
        // TLB/PCID generations) instead of re-running `translate_acc` per byte. A
        // pending fault disables it (don't read past a fault). Byte-identical to
        // `translate_acc` on a hit (same frame the TLB holds), and any flush bumps a
        // generation so a stale entry can never be used.
        if self.fault.is_none() {
            let page = vaddr & !0xfff;
            let pcid = self.active_pcid();
            if self.ifetch_gen == self.tlb_gen
                && self.ifetch_tag == page
                && self.ifetch_pcid as usize == pcid
                && self.ifetch_pgen == self.pcid_gen[pcid]
            {
                let p = (self.ifetch_frame | (vaddr & 0xfff)) as usize;
                let b = *self.ram.get(p).unwrap_or(&0);
                self.rip = vaddr.wrapping_add(1);
                return b;
            }
        }
        let user = self.cpl == 3;
        let pa = self.translate_acc(vaddr, false, user);
        // Refresh the cache only on a successful (present) paged translation — exactly
        // the condition under which `translate_acc` filled the TLB.
        if self.fault.is_none() && self.paging() {
            self.ifetch_tag = vaddr & !0xfff;
            self.ifetch_frame = pa & !0xfff;
            self.ifetch_gen = self.tlb_gen;
            let pcid = self.active_pcid();
            self.ifetch_pcid = pcid as u16;
            self.ifetch_pgen = self.pcid_gen[pcid];
        }
        let b = *self.ram.get(pa as usize).unwrap_or(&0);
        self.rip = vaddr.wrapping_add(1);
        b
    }

    fn fetch(&mut self, n: u8) -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            v |= u64::from(self.fetch_u8()) << (8 * i);
        }
        v
    }

    /// Fetch the operand-size immediate (`imm16`/`imm32`, sign-extended to the
    /// operand `size`): 2 bytes under a `0x66` prefix, otherwise 4 bytes (a
    /// 64-bit operand takes a sign-extended `imm32`). The common immediate form of
    /// `MOV`/`ADD`/`TEST`/… r/m, imm.
    fn fetch_imm_z(&mut self, size: u8) -> u64 {
        if size == 2 {
            self.fetch(2)
        } else {
            let i = self.fetch(4);
            if size == 8 {
                i as i32 as i64 as u64
            } else {
                i
            }
        }
    }

    // ── Flags ────────────────────────────────────────────────────────────────
    fn set(&mut self, bit: u64, on: bool) {
        if on {
            self.rflags |= bit;
        } else {
            self.rflags &= !bit;
        }
    }

    fn mask(size: u8) -> u64 {
        if size >= 8 {
            u64::MAX
        } else {
            (1u64 << (8 * size as u32)) - 1
        }
    }

    fn sign(v: u64, size: u8) -> bool {
        (v >> (8 * size as u32 - 1)) & 1 == 1
    }

    /// Set ZF/SF/PF (and clear CF/OF) from a logical result.
    fn flags_logic(&mut self, res: u64, size: u8) {
        let m = Self::mask(size);
        let r = res & m;
        self.set(flag::ZF, r == 0);
        self.set(flag::SF, Self::sign(r, size));
        self.set(flag::PF, (r as u8).count_ones().is_multiple_of(2));
        self.set(flag::CF, false);
        self.set(flag::OF, false);
    }

    /// Set all arithmetic flags for `a (+/-) b = res` (`sub` selects subtraction).
    fn flags_arith(&mut self, a: u64, b: u64, res: u64, size: u8, sub: bool) {
        let m = Self::mask(size);
        let r = res & m;
        self.set(flag::ZF, r == 0);
        self.set(flag::SF, Self::sign(r, size));
        self.set(flag::PF, (r as u8).count_ones().is_multiple_of(2));
        // AF (auxiliary carry) = carry/borrow into bit 4 = (a ^ b ^ result) bit 4,
        // the same identity for add and subtract.
        self.set(flag::AF, (a ^ b ^ res) & 0x10 != 0);
        if sub {
            self.set(flag::CF, (a & m) < (b & m));
            let of = (Self::sign(a, size) != Self::sign(b, size))
                && (Self::sign(a, size) != Self::sign(r, size));
            self.set(flag::OF, of);
        } else {
            self.set(flag::CF, r < (a & m));
            let of = (Self::sign(a, size) == Self::sign(b, size))
                && (Self::sign(a, size) != Self::sign(r, size));
            self.set(flag::OF, of);
        }
    }

    /// Run up to `max_steps` instructions; returns why it stopped.
    pub fn run(&mut self, max_steps: u64) -> Halt {
        for i in 0..max_steps {
            #[cfg(feature = "cc44-trace")]
            if i & 0x3ff_ffff == 0 {
                use std::io::Write as _;
                let tsc = self.sys.as_ref().map(|s| s.tsc).unwrap_or(0);
                let _ = writeln!(
                    std::io::stderr(),
                    "\n[cc44-trace] step={i} rip={:#x} tsc={tsc} if={}",
                    self.rip,
                    self.rflags & RFLAGS_IF != 0
                );
            }
            // Pump the network periodically so host-side data and connection
            // events reach the guest without it having to transmit first (the
            // `virtio-net` receive path; `CC-16` parity, `CC-46`) — the same
            // shared `devbus` pump the other cores drive from their run loops.
            if i & 0x3ff == 0 && self.sys.as_ref().is_some_and(|s| s.virtionet.is_some()) {
                self.virtio_net_pump();
            }
            // Advance the platform timers (PIT + APIC timer + TSC) and latch a
            // tick when one expires, then deliver any pending interrupt through
            // the IDT — the interrupt path a real Linux boot needs (`#12`). The
            // periodic timers advance unconditionally (like the riscv64/aarch64
            // cores); a latched tick is delivered only when `RFLAGS.IF` is set.
            self.sys_tick();
            if self.sys.as_ref().is_some_and(|s| s.halted) {
                return Halt::Halted;
            }
            // THRE is a LEVEL condition: while the TX-empty interrupt is enabled
            // (ETBEI) and the holding register is empty (always — we emit instantly),
            // a real 16550 keeps asserting IRQ4 so the driver is re-interrupted to
            // send the NEXT FIFO batch. Without re-asserting, a write longer than the
            // FIFO's `tx_loadsz` (16) deadlocks: the driver writes one FIFO-full,
            // exits with data still queued, and waits for a drained-FIFO interrupt
            // that a one-shot THRE never delivers (observed: console froze after
            // exactly 16 chars). The driver clears ETBEI once its ring empties, so
            // this cannot re-fire forever. `thre_pending` gates it to one in-flight
            // interrupt at a time (re-armed after the driver reads the IIR).
            if let Some(sys) = self.sys.as_mut() {
                if sys.uart.ier & 0x02 != 0 && !sys.uart.thre_pending {
                    sys.uart.thre_pending = true;
                    sys.uart.dbg[4] += 1;
                    sys.raise_irq(4);
                }
            }
            self.take_pending_interrupt();
            #[cfg(feature = "cc44-trace")]
            let prev_rip = self.rip;
            #[cfg(feature = "cc44-trace")]
            if (0xffff_ffff_8103_6970..0xffff_ffff_8103_6e00).contains(&self.rip) {
                TP_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            #[cfg(feature = "cc44-trace")]
            if matches!(
                self.rip,
                0xffff_ffff_8103_69fe
                    | 0xffff_ffff_8103_6a04
                    | 0xffff_ffff_8103_6a53
                    | 0xffff_ffff_8103_6a65
                    | 0xffff_ffff_8103_6a7d
                    | 0xffff_ffff_8103_6a82
                    | 0xffff_ffff_8103_6a92
            ) {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "[cc44-trace] BR rip={:#x} rax={:#x} rcx={:#x} rsp={:#x}",
                    self.rip,
                    self.r[RAX],
                    self.r[RCX],
                    self.r[RSP],
                );
            }
            match self.step() {
                Ok(()) => self.insns = self.insns.wrapping_add(1),
                Err(h) => return h,
            }
            #[cfg(feature = "cc44-trace")]
            if self.rip < 0x1000 && prev_rip >= 0x1000 {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "\n[cc44-trace] CONTROL→{:#x} from rip={prev_rip:#x} at step={i} \
                     cr2={:#x} regs: rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x} rsi={:#x} \
                     rdi={:#x} rbp={:#x} rsp={:#x} cpl={} if={}",
                    self.rip,
                    self.cr2,
                    self.r[0],
                    self.r[3],
                    self.r[1],
                    self.r[2],
                    self.r[6],
                    self.r[7],
                    self.r[RBP],
                    self.r[RSP],
                    self.cpl,
                    self.rflags & RFLAGS_IF != 0,
                );
                let idtr = self.sys.as_ref().map(|s| s.idtr).unwrap_or((0, 0));
                let _ = writeln!(
                    std::io::stderr(),
                    "[cc44-trace] idtr.base={:#x} idtr.limit={:#x} cr3={:#x}",
                    idtr.0,
                    idtr.1,
                    self.cr3,
                );
                panic!("[cc44-trace] control transferred to low memory (rip<0x1000)");
            }
        }
        Halt::OutOfBudget
    }

    // ── ModRM / effective-address decode ──────────────────────────────────────
    /// Decode the ModRM (and SIB/displacement), returning `(reg, rm)` where `reg`
    /// is the register field (REX.R-extended) and `rm` is the operand location.
    fn modrm(&mut self, rex: u8) -> (usize, Rm) {
        let modrm = self.fetch_u8();
        let md = modrm >> 6;
        let reg = ((modrm >> 3) & 7) as usize | (((rex >> 2) & 1) as usize) << 3; // REX.R
        let rm_field = (modrm & 7) as usize;
        if md == 3 {
            let r = rm_field | (((rex & 1) as usize) << 3); // REX.B
            return (reg, Rm::Reg(r));
        }
        // Memory operand.
        let mut base_disp: i64;
        let mut addr: u64;
        if rm_field == 4 {
            // SIB.
            let sib = self.fetch_u8();
            let scale = sib >> 6;
            let index = ((sib >> 3) & 7) as usize | (((rex >> 1) & 1) as usize) << 3; // REX.X
            let base = (sib & 7) as usize | (((rex & 1) as usize) << 3); // REX.B
            let idx_val = if index == 4 {
                0
            } else {
                self.r[index] << scale
            };
            if (sib & 7) == 5 && md == 0 {
                addr = idx_val; // disp32, no base
                base_disp = i64::from(self.fetch(4) as i32);
            } else {
                addr = self.r[base].wrapping_add(idx_val);
                base_disp = 0;
            }
        } else if rm_field == 5 && md == 0 {
            // RIP-relative: disp32 relative to the address of the *next*
            // instruction. The end-of-instruction rip is only known once any
            // trailing immediate is consumed, so this is resolved lazily (the
            // operand is accessed after the full instruction has decoded).
            let disp = i64::from(self.fetch(4) as i32);
            let seg = self.seg_base();
            return (reg, Rm::RipRel(disp, seg));
        } else {
            addr = self.r[rm_field | (((rex & 1) as usize) << 3)];
            base_disp = 0;
        }
        match md {
            1 => base_disp = base_disp.wrapping_add(i64::from(self.fetch(1) as i8)),
            2 => base_disp = base_disp.wrapping_add(i64::from(self.fetch(4) as i32)),
            _ => {}
        }
        addr = addr
            .wrapping_add(base_disp as u64)
            .wrapping_add(self.seg_base());
        (reg, Rm::Mem(addr))
    }

    /// The base of the segment overriding this instruction's memory operand (the
    /// `FS`/`GS` base from the corresponding MSR), or 0 with no override / in the
    /// flat segments. The kernel's per-CPU `%gs:` and userspace `%fs:` accesses
    /// need this; every other segment is flat (base 0) in long mode.
    fn seg_base(&self) -> u64 {
        match self.cur_seg {
            Some(SegId::Fs) => self.seg[SegId::Fs as usize].base,
            Some(SegId::Gs) => self.seg[SegId::Gs as usize].base,
            _ => 0,
        }
    }

    /// Resolve a decoded r/m operand to its effective address (for memory forms);
    /// `None` for a register operand. Used by `LEA` and the string/atomic ops.
    fn rm_addr(&self, rm: Rm) -> Option<u64> {
        match rm {
            Rm::Reg(_) => None,
            Rm::Mem(a) => Some(a),
            Rm::RipRel(disp, seg) => Some(self.rip.wrapping_add(disp as u64).wrapping_add(seg)),
        }
    }

    /// Read an 8-bit register operand named by the ModRM `reg` field, honouring
    /// the AH/CH/DH/BH high-byte encoding when no `REX` prefix is present.
    fn reg8(&self, reg: usize) -> u64 {
        if !self.rex_present && (4..8).contains(&reg) {
            (self.r[reg - 4] >> 8) & 0xff
        } else {
            self.r[reg] & 0xff
        }
    }

    fn load_rm(&mut self, rm: Rm, size: u8) -> u64 {
        match rm {
            Rm::Reg(i) => {
                if size == 1 && !self.rex_present && (4..8).contains(&i) {
                    // AH/CH/DH/BH — bits[15:8] of RAX/RCX/RDX/RBX.
                    (self.r[i - 4] >> 8) & 0xff
                } else {
                    self.r[i] & Self::mask(size)
                }
            }
            Rm::Mem(a) => self.rd(a, size),
            Rm::RipRel(disp, seg) => {
                let a = self.rip.wrapping_add(disp as u64).wrapping_add(seg);
                self.rd(a, size)
            }
        }
    }

    fn store_rm(&mut self, rm: Rm, size: u8, val: u64) {
        match rm {
            Rm::Reg(i) => {
                if size == 1 && !self.rex_present && (4..8).contains(&i) {
                    // AH/CH/DH/BH — bits[15:8] of RAX/RCX/RDX/RBX.
                    let r = i - 4;
                    self.r[r] = (self.r[r] & !0xff00) | ((val & 0xff) << 8);
                } else if size >= 4 {
                    // 32-bit writes zero the upper 32 bits; 64-bit is full.
                    self.r[i] = val & Self::mask(size);
                } else {
                    let m = Self::mask(size);
                    self.r[i] = (self.r[i] & !m) | (val & m);
                }
            }
            Rm::Mem(a) => self.wr(a, size, val),
            Rm::RipRel(disp, seg) => {
                let a = self.rip.wrapping_add(disp as u64).wrapping_add(seg);
                self.wr(a, size, val);
            }
        }
    }

    fn push(&mut self, val: u64) {
        self.r[RSP] = self.r[RSP].wrapping_sub(8);
        let sp = self.r[RSP];
        self.wr(sp, 8, val);
    }

    fn pop(&mut self) -> u64 {
        let sp = self.r[RSP];
        let v = self.rd(sp, 8);
        self.r[RSP] = sp.wrapping_add(8);
        v
    }

    /// Pop `size` bytes off the stack (the far-return slot width).
    fn pop_sized(&mut self, size: u8) -> u64 {
        let sp = self.r[RSP];
        let v = self.rd(sp, size);
        self.r[RSP] = sp.wrapping_add(u64::from(size));
        v
    }

    /// One of the eight ALU group-1 operations on `(a, b)` → result, setting flags.
    fn alu(&mut self, op: u8, a: u64, b: u64, size: u8) -> u64 {
        let m = Self::mask(size);
        match op {
            0 => {
                let r = a.wrapping_add(b);
                self.flags_arith(a, b, r, size, false);
                r & m
            } // ADD
            1 => {
                let r = a | b;
                self.flags_logic(r, size);
                r & m
            } // OR
            4 => {
                let r = a & b;
                self.flags_logic(r, size);
                r & m
            } // AND
            5 => {
                let r = a.wrapping_sub(b);
                self.flags_arith(a, b, r, size, true);
                r & m
            } // SUB
            6 => {
                let r = a ^ b;
                self.flags_logic(r, size);
                r & m
            } // XOR
            7 => {
                let r = a.wrapping_sub(b);
                self.flags_arith(a, b, r, size, true);
                a & m
            } // CMP (discards result)
            2 => {
                // ADC: a + b + CF, with carry/overflow accounting for the carry-in.
                let cin = u64::from(self.rflags & flag::CF != 0);
                let r = a.wrapping_add(b).wrapping_add(cin);
                let rm = r & m;
                self.set(flag::ZF, rm == 0);
                self.set(flag::SF, Self::sign(rm, size));
                self.set(flag::PF, (rm as u8).count_ones().is_multiple_of(2));
                self.set(flag::CF, rm < (a & m) || (cin == 1 && rm == (a & m)));
                let of = (Self::sign(a, size) == Self::sign(b, size))
                    && (Self::sign(a, size) != Self::sign(rm, size));
                self.set(flag::OF, of);
                rm
            }
            3 => {
                // SBB: a - b - CF.
                let cin = u64::from(self.rflags & flag::CF != 0);
                let r = a.wrapping_sub(b).wrapping_sub(cin);
                let rm = r & m;
                self.set(flag::ZF, rm == 0);
                self.set(flag::SF, Self::sign(rm, size));
                self.set(flag::PF, (rm as u8).count_ones().is_multiple_of(2));
                self.set(
                    flag::CF,
                    (a & m) < (b & m).wrapping_add(cin) || (b & m) == m && cin == 1,
                );
                let of = (Self::sign(a, size) != Self::sign(b, size))
                    && (Self::sign(a, size) != Self::sign(rm, size));
                self.set(flag::OF, of);
                rm
            }
            _ => unreachable!("alu op {op} out of range"),
        }
    }

    /// Evaluate a condition code (the low nibble of a `Jcc`/`SETcc` opcode).
    fn cond(&self, cc: u8) -> bool {
        let f = self.rflags;
        let zf = f & flag::ZF != 0;
        let cf = f & flag::CF != 0;
        let sf = f & flag::SF != 0;
        let of = f & flag::OF != 0;
        let pf = f & flag::PF != 0;
        let base = match cc >> 1 {
            0 => of,               // O
            1 => cf,               // B/C
            2 => zf,               // E/Z
            3 => cf || zf,         // BE
            4 => sf,               // S
            5 => pf,               // P
            6 => sf != of,         // L
            _ => (sf != of) || zf, // LE
        };
        if cc & 1 == 1 {
            !base
        } else {
            base
        }
    }

    /// Port output. The boot path drives the `16550` serial console (`0x3f8`),
    /// the 8259 PIC pair (`0x20`/`0x21`, `0xa0`/`0xa1`), and the 8254 PIT
    /// (`0x40`/`0x43`); other ports are ignored (the kernel probes many).
    fn port_out(&mut self, port: u16, val: u8) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        let dlab = sys.uart.lcr & 0x80 != 0;
        match port {
            0x3f8 if dlab => sys.uart.divisor = (sys.uart.divisor & 0xff00) | u16::from(val),
            0x3f8 if sys.uart.mcr & 0x10 != 0 => {
                // Loopback (the 8250 autodetection): the transmitted byte loops
                // back to the receive register rather than reaching the console.
                sys.uart.input.push(val);
            }
            0x3f8 => {
                sys.uart.dbg[0] += 1;
                sys.uart.output.push(val);
                #[cfg(feature = "cc44-trace")]
                {
                    use std::io::Write as _;
                    let mut o = std::io::stderr();
                    let _ = o.write_all(&[val]);
                    let _ = o.flush();
                }
                // The TX holding register is empty again immediately (we emit at
                // once); if the driver runs the console interrupt-driven (THRE
                // enabled, IER bit 1), signal it can send the next byte. COM1 =
                // IRQ4. Without this an interrupt-driven userspace tty write blocks
                // forever waiting to transmit (the idle<->init boot livelock).
                //
                // COALESCE the THRE interrupt: raise IRQ4 only on the false→true
                // transition of `thre_pending`, never on every byte. A real 16550
                // raises THRE once when the FIFO drains, and the driver's
                // `serial8250_tx_chars` loop — gated on our always-set LSR THRE —
                // then writes a whole batch (`tx_loadsz` bytes) under that single
                // interrupt. Pulsing IRQ4 per byte instead makes each character
                // cost a full interrupt entry/IRET round-trip (~1000× slowdown:
                // the boot crawls in `serial8250_tx_chars`). Once the kernel reads
                // the IIR (clearing `thre_pending`), the next byte re-arms it.
                if sys.uart.ier & 0x02 != 0 && !sys.uart.thre_pending {
                    sys.uart.thre_pending = true;
                    sys.uart.dbg[4] += 1;
                    sys.raise_irq(4);
                }
            }
            0x3f9 if dlab => {
                sys.uart.divisor = (sys.uart.divisor & 0x00ff) | (u16::from(val) << 8);
            }
            0x3f9 => {
                sys.uart.dbg[1] += 1;
                sys.uart.ier = val;
                // Enabling ETBEI (bit 1) with an empty THR (always — we emit at
                // once) asserts the one-shot THRE interrupt; enabling ERBFI (bit 0)
                // with a byte waiting asserts RX. COM1 = IRQ4.
                let dr = sys.uart.in_cursor < sys.uart.input.len();
                if val & 0x02 != 0 {
                    sys.uart.thre_pending = true;
                }
                if val & 0x02 != 0 || (val & 0x01 != 0 && dr) {
                    sys.raise_irq(4);
                }
            }
            0x3fa => sys.uart.fcr = val, // FCR (write side of IIR/FCR)
            0x3fb => sys.uart.lcr = val,
            0x3fc => sys.uart.mcr = val,
            0x3ff => sys.uart.scratch = val,
            // 8259 master/slave command + data ports (ICW/OCW programming).
            0x20 | 0xa0 => Self::pic_command(&mut sys.pic, port == 0xa0, val),
            0x21 | 0xa1 => Self::pic_data(&mut sys.pic, port == 0xa1, val),
            // 8254 PIT: 0x43 = mode/command, 0x40 = channel-0, 0x42 = channel-2.
            0x43 => {
                // bits 7:6 select the channel: 00 = ch0 (periodic tick), 10 = ch2
                // (the TSC-calibration one-shot). A new mode/command resets that
                // channel's byte toggle (and re-arms channel 2).
                match val >> 6 {
                    0 => {
                        sys.pit.write_hi = false;
                        // bits 3:1 = mode. Mode 2/3 = periodic (rate generator /
                        // square wave); anything else (mode 0) = one-shot.
                        let mode = (val >> 1) & 0x7;
                        sys.pit.ch0_periodic = mode == 2 || mode == 3 || mode == 6 || mode == 7;
                    }
                    2 => {
                        sys.pit.ch2_write_hi = false;
                        sys.pit.ch2_out = false;
                    }
                    _ => {}
                }
            }
            0x40 => {
                if sys.pit.write_hi {
                    sys.pit.reload = (sys.pit.reload & 0x00ff) | (u16::from(val) << 8);
                    sys.pit.enabled = true;
                    // The high byte completes the count — (re)arm the down-counter.
                    // For one-shot this is the new deadline the tick handler set.
                    sys.pit.counter = u32::from(sys.pit.reload);
                } else {
                    sys.pit.reload = (sys.pit.reload & 0xff00) | u16::from(val);
                }
                sys.pit.write_hi = !sys.pit.write_hi;
            }
            0x42 => {
                if sys.pit.ch2_write_hi {
                    sys.pit.ch2_reload = (sys.pit.ch2_reload & 0x00ff) | (u16::from(val) << 8);
                } else {
                    sys.pit.ch2_reload = (sys.pit.ch2_reload & 0xff00) | u16::from(val);
                }
                sys.pit.ch2_write_hi = !sys.pit.ch2_write_hi;
                // The high byte completes the reload → (re)load the one-shot.
                if !sys.pit.ch2_write_hi {
                    sys.pit.ch2_counter = u32::from(sys.pit.ch2_reload);
                    sys.pit.ch2_out = false;
                }
            }
            // NMI status/control (port 0x61): bit 0 gates channel 2, bit 1 the
            // speaker. A gate rising edge re-arms the one-shot from its reload.
            0x61 => {
                let gate = val & 1 != 0;
                if gate && !sys.pit.ch2_gate {
                    sys.pit.ch2_counter = u32::from(sys.pit.ch2_reload);
                    sys.pit.ch2_out = false;
                }
                sys.pit.ch2_gate = gate;
            }
            _ => {}
        }
    }

    /// Program an 8259 command port (`0x20`/`0xa0`): ICW1 begins init; an `EOI`
    /// (`0x20`) ends the in-service interrupt.
    fn pic_command(pic: &mut Pic, slave: bool, val: u8) {
        if val & 0x10 != 0 {
            // ICW1 — start the init sequence (ICW2 next).
            if slave {
                pic.init_slave = 1;
            } else {
                pic.init_master = 1;
            }
        }
        // 0x20 = non-specific EOI (handled by the ack in take_pending_interrupt).
    }

    /// Program an 8259 data port (`0x21`/`0xa1`): ICW2 (vector base), ICW3/ICW4
    /// during init, otherwise the IRQ mask (OCW1).
    fn pic_data(pic: &mut Pic, slave: bool, val: u8) {
        let init = if slave {
            &mut pic.init_slave
        } else {
            &mut pic.init_master
        };
        if *init == 1 {
            // ICW2: the vector base (the IRQ → vector remap).
            if slave {
                pic.base_slave = val & 0xf8;
            } else {
                pic.base_master = val & 0xf8;
            }
            *init = 2; // ICW3 next
        } else if *init >= 2 && *init <= 3 {
            *init += 1;
            if *init > 3 {
                *init = 0; // ICW4 consumed → init done
            }
        } else {
            // OCW1: the IRQ mask.
            if slave {
                pic.mask = (pic.mask & 0x00ff) | (u16::from(val) << 8);
            } else {
                pic.mask = (pic.mask & 0xff00) | u16::from(val);
            }
        }
    }

    fn port_in(&mut self, port: u16) -> u8 {
        if let Some(sys) = self.sys.as_mut() {
            let dlab = sys.uart.lcr & 0x80 != 0;
            match port {
                0x3f8 if dlab => return (sys.uart.divisor & 0xff) as u8, // DLL
                0x3f8 if sys.uart.in_cursor < sys.uart.input.len() => {
                    let b = sys.uart.input[sys.uart.in_cursor];
                    sys.uart.in_cursor += 1;
                    return b;
                }
                0x3f8 => return 0, // RBR, no data
                0x3f9 if dlab => return (sys.uart.divisor >> 8) as u8, // DLM
                0x3f9 => return sys.uart.ier,
                0x3fa => {
                    sys.uart.dbg[2] += 1;
                    // IIR: report the pending UART interrupt by priority — RX-data
                    // (id 0x04) when receive ints are enabled and a byte waits,
                    // else transmit-holding-empty (id 0x02) while THRE ints are
                    // enabled (the TX register is always empty — we emit at once),
                    // else none (bit0 = 1). FIFO bits reflect FCR.
                    let dr = sys.uart.in_cursor < sys.uart.input.len();
                    let id = if sys.uart.ier & 0x01 != 0 && dr {
                        0x04
                    } else if sys.uart.ier & 0x02 != 0 && sys.uart.thre_pending {
                        // Reading the IIR clears the THRE interrupt (16550): it is a
                        // one-shot per THR-empty transition, NOT a level held while
                        // ETBEI is set — otherwise the serial ISR re-fires forever.
                        sys.uart.thre_pending = false;
                        0x02
                    } else {
                        0x01
                    };
                    return id | (sys.uart.fcr & 0xc0);
                }
                0x3fb => return sys.uart.lcr,
                0x3fc => return sys.uart.mcr,
                0x3fd => {
                    sys.uart.dbg[3] += 1;
                    // Line Status Register: THR-empty (0x20) + transmitter-empty
                    // (0x40) always set; data-ready (0x01) when input is pending.
                    let dr = u8::from(sys.uart.in_cursor < sys.uart.input.len());
                    return 0x60 | dr;
                }
                0x3fe => {
                    // Modem Status Register. In loopback (MCR bit4) the modem
                    // inputs reflect the MCR outputs — the 8250 autodetection test.
                    if sys.uart.mcr & 0x10 != 0 {
                        let m = sys.uart.mcr;
                        // CTS<-RTS(bit1->4), DSR<-DTR(bit0->5), RI<-OUT1(bit2->6),
                        // DCD<-OUT2(bit3->7).
                        return ((m & 0x02) << 3)
                            | ((m & 0x01) << 5)
                            | ((m & 0x04) << 4)
                            | ((m & 0x08) << 4);
                    }
                    return 0xb0; // CTS|DSR|DCD asserted (a present, ready line)
                }
                0x3ff => return sys.uart.scratch,
                0x21 => return (sys.pic.mask & 0xff) as u8,
                0xa1 => return (sys.pic.mask >> 8) as u8,
                // NMI status/control: bit 5 mirrors OUT2 (channel-2 expiry) — the
                // bit the TSC-calibration loop spins on; the low bits echo the
                // gate/speaker state we were last told to set.
                0x61 => {
                    let mut v = 0u8;
                    if sys.pit.ch2_gate {
                        v |= 0x01;
                    }
                    if sys.pit.ch2_out {
                        v |= 0x20;
                    }
                    return v;
                }
                // Channel-2 counter readback (latched low-then-high) — some
                // calibration paths read the residual count.
                0x42 => {
                    let lo = sys.pit.ch2_write_hi;
                    sys.pit.ch2_write_hi = !sys.pit.ch2_write_hi;
                    return if lo {
                        (sys.pit.ch2_counter & 0xff) as u8
                    } else {
                        (sys.pit.ch2_counter >> 8) as u8
                    };
                }
                _ => {}
            }
        }
        0
    }

    /// The 32-bit PCI configuration register addressed by `pci_addr` (mechanism
    /// #1). Only bus 0 / device 0 / function 0 exists — a minimal Intel i440FX-style
    /// host bridge, so the kernel's type-1 sanity check finds a host bridge and uses
    /// mechanism #1; every other function reads all-ones (absent). The machine's
    /// real devices are virtio-mmio, not PCI.
    fn pci_config_dword(pci_addr: u32) -> u32 {
        let bus = (pci_addr >> 16) & 0xff;
        let dev = (pci_addr >> 11) & 0x1f;
        let func = (pci_addr >> 8) & 0x7;
        if bus != 0 || dev != 0 || func != 0 {
            return 0xffff_ffff; // no device
        }
        match pci_addr & 0xfc {
            0x00 => 0x7190_8086, // vendor 0x8086 (Intel), device 0x7190 (i440FX)
            0x08 => 0x0600_0000, // class 0x06 host bridge, subclass 0x00, rev 0
            _ => 0,
        }
    }

    /// A word/dword (`size` 2/4) port read. The PCI config-data port
    /// (`0xcfc`..`0xcff`) returns all-ones — no PCI host bridge is present, so the
    /// kernel's PCI scan finds no devices (it uses `virtio-mmio`, not PCI).
    /// Other ports compose from byte reads.
    fn port_in_wide(&mut self, port: u16, size: u8) -> u64 {
        if port == 0xcf8 {
            return u64::from(self.sys().pci_addr); // CONFIG_ADDRESS read-back
        }
        if (0xcfc..=0xcff).contains(&port) {
            // Config-data read (mechanism #1): the latched address selects the
            // device/register; the port offset within 0xcfc..0xcff selects the byte
            // within the register dword.
            let dword = Self::pci_config_dword(self.sys().pci_addr);
            let shift = 8 * u32::from(port - 0xcfc);
            return u64::from(dword >> shift) & Self::mask(size);
        }
        let mut v = 0u64;
        for i in 0..u64::from(size) {
            v |= u64::from(self.port_in(port.wrapping_add(i as u16))) << (8 * i);
        }
        v
    }

    /// A word/dword port write. The PCI config-address port (`0xcf8`) and config
    /// data are accepted and discarded (no PCI bridge); other ports compose into
    /// byte writes.
    fn port_out_wide(&mut self, port: u16, size: u8, val: u64) {
        if port == 0xcf8 {
            // CONFIG_ADDRESS — latch it so the kernel's mechanism-#1 detection
            // (write 0x80000000, read it back) succeeds; the config-data scan then
            // returns all-ones (no devices) and completes, instead of falling back
            // to the unhandled mechanism #2 (which wedged the boot).
            self.sys_mut().pci_addr = val as u32;
            return;
        }
        if (0xcfc..=0xcff).contains(&port) {
            return; // PCI config data — no host bridge / no devices
        }
        for i in 0..u64::from(size) {
            self.port_out(port.wrapping_add(i as u16), (val >> (8 * i)) as u8);
        }
    }

    // ── local APIC MMIO (the long-mode boot path; #12) ─────────────────────────

    /// Read a local-APIC register at byte offset `off` (the MMIO window at
    /// [`LAPIC_BASE`]). Only the registers a UP boot reads are modelled.
    fn lapic_read(&mut self, off: u32) -> u32 {
        let l = &self.sys().lapic;
        match off {
            0x020 => 0,               // APIC ID (CPU 0)
            0x030 => 0x0005_0014,     // version
            0x080 => l.tpr,           // TPR
            0x0f0 => l.svr,           // spurious vector
            0x320 => l.lvt_timer,     // LVT timer
            0x380 => l.initial_count, // timer initial count
            0x390 => l.current_count, // timer current count
            0x3e0 => l.divide,        // divide config
            0x100..=0x170 if off & 0xf == 0 => {
                let r = ((off - 0x100) / 0x10) as usize; // 8 x 32-bit ISR regs
                (l.isr[r / 2] >> (32 * (r & 1))) as u32
            }
            0x200..=0x270 if off & 0xf == 0 => {
                let r = ((off - 0x200) / 0x10) as usize; // 8 x 32-bit IRR regs
                (l.irr[r / 2] >> (32 * (r & 1))) as u32
            }
            _ => 0,
        }
    }

    /// Write a local-APIC register. Drives the spurious-vector (enable), the LVT
    /// timer, the timer count/divide, the TPR, and `EOI` (offset `0xb0`).
    /// IOREGSEL (`+0x00`) / IOWIN (`+0x10`) — the I/O APIC's indirect register
    /// window (see [`Ioapic`]).
    fn ioapic_read(&mut self, off: u32) -> u32 {
        match off {
            0x00 => self.sys().ioapic.ioregsel,
            0x10 => self.sys().ioapic.read(),
            _ => 0,
        }
    }
    fn ioapic_write(&mut self, off: u32, val: u32) {
        match off {
            0x00 => self.sys_mut().ioapic.ioregsel = val & 0xff,
            0x10 => self.sys_mut().ioapic.write(val),
            _ => {}
        }
    }

    fn lapic_write(&mut self, off: u32, val: u32) {
        // EOI must reach the I/O APIC (Remote IRR), so it can't borrow only `lapic`.
        if off == 0x0b0 {
            self.sys_mut().lapic_eoi();
            return;
        }
        let l = &mut self.sys_mut().lapic;
        match off {
            0x080 => l.tpr = val,
            0x0f0 => l.svr = val,
            0x320 => {
                l.lvt_timer = val;
            }
            0x380 => {
                l.initial_count = val;
                l.current_count = val;
            }
            0x3e0 => l.divide = val,
            0x300 => {
                // Interrupt Command Register (low): send an IPI. Bits 8-10 =
                // delivery mode (0 = fixed), bits 18-19 = destination shorthand
                // (1 = self, 2 = all-incl-self, 3 = all-excl-self). On this
                // uniprocessor the only target is CPU 0, so a fixed IPI to self,
                // all-incl-self, or a directed destination all deliver the vector
                // locally — the path the kernel's reschedule and `irq_work`
                // self-IPIs take. (NMI/INIT/SIPI and all-excl-self: no local
                // effect here.)
                let fixed = (val >> 8) & 7 == 0;
                let not_excl_self = (val >> 18) & 3 != 3;
                if fixed && not_excl_self {
                    l.set_irr((val & 0xff) as u8);
                }
            }
            _ => {}
        }
    }

    // ── shared device bus (virtio-mmio transport; CC-46) ─────────────────────

    /// Read a `virtio-mmio` register of one of the shared `devbus` devices at
    /// guest-physical `pa` — the x86-64 transport's side of the *same* devbus the
    /// RISC-V and AArch64 cores drive (Law L4; only the MMIO window differs).
    fn mmio_read(&mut self, pa: u64, _width: usize) -> u64 {
        if (VIRTIO_BLK_BASE..VIRTIO_BLK_END).contains(&pa) {
            return super::devbus::blk_mmio_read(self.sys().virtio.as_ref(), pa - VIRTIO_BLK_BASE);
        }
        if (VIRTIO_9P_BASE..VIRTIO_9P_END).contains(&pa) {
            return super::devbus::p9_mmio_read(self.sys().virtio9p.as_ref(), pa - VIRTIO_9P_BASE);
        }
        if (VIRTIO_NET_BASE..VIRTIO_NET_END).contains(&pa) {
            return super::devbus::net_mmio_read(
                self.sys().virtionet.as_ref(),
                pa - VIRTIO_NET_BASE,
            );
        }
        0
    }

    /// Write a `virtio-mmio` register of one of the shared `devbus` devices. A
    /// `QueueNotify` services the queue through the shared `devbus` over a
    /// [`GuestRam`](super::devbus::GuestRam) view of x86-64 RAM. Interrupt
    /// delivery is deferred to the long-mode boot path (`#12`, the APIC); the
    /// device-level witness drains the used ring directly (the same way the
    /// AArch64 witness reads the device's reply without taking the SPI).
    fn mmio_write(&mut self, pa: u64, _width: usize, value: u64) {
        if (VIRTIO_BLK_BASE..VIRTIO_BLK_END).contains(&pa) {
            self.virtio_blk_write(pa - VIRTIO_BLK_BASE, value as u32);
        } else if (VIRTIO_9P_BASE..VIRTIO_9P_END).contains(&pa) {
            self.virtio_9p_write(pa - VIRTIO_9P_BASE, value as u32);
        } else if (VIRTIO_NET_BASE..VIRTIO_NET_END).contains(&pa) {
            self.virtio_net_write(pa - VIRTIO_NET_BASE, value as u32);
        }
    }

    #[cfg(feature = "std")]
    #[doc(hidden)]
    pub fn vv_dbg(&self, vaddr: u64) {
        let pml4 = self.cr3 & 0x000f_ffff_ffff_f000;
        let idx = |lvl: u32| ((vaddr >> (12 + 9 * lvl)) & 0x1ff) * 8;
        let e4 = self.rd_phys(pml4 + idx(3), 8);
        std::eprintln!(
            "cr3={:#x} PML4[{}]={:#x}",
            self.cr3,
            (vaddr >> 39) & 0x1ff,
            e4
        );
        if e4 & 1 == 0 {
            std::eprintln!("  PML4 not present");
            return;
        }
        let e3 = self.rd_phys((e4 & 0x000f_ffff_ffff_f000) + idx(2), 8);
        std::eprintln!("  PDPT[{}]={:#x}", (vaddr >> 30) & 0x1ff, e3);
    }

    /// A guest-RAM view for the shared devbus to walk virtqueues over (x86-64 RAM
    /// is based at [`RAM_BASE`] = 0).
    fn guest_ram(&mut self) -> super::devbus::GuestRam<'_> {
        super::devbus::GuestRam {
            ram: &mut self.ram,
            base: RAM_BASE,
        }
    }

    /// A `virtio-blk` MMIO register write; a `QueueNotify` services the queue
    /// against the κ-disk through the shared `devbus` and raises the device's IRQ
    /// (`VIRTIO_BLK_IRQ`) so the guest's driver completes the request (`CC-45`).
    fn virtio_blk_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio.take() else {
            return;
        };
        let mut raise = false;
        if super::devbus::blk_mmio_write(&mut dev, off, value) {
            let mut mem = self.guest_ram();
            raise = super::devbus::blk_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio = Some(dev);
        if raise {
            self.sys_mut().raise_irq(VIRTIO_BLK_IRQ);
        }
    }

    /// A `virtio-9p` MMIO register write; a `QueueNotify` services the workspace
    /// filesystem queue through the shared `devbus` — the same servicing the
    /// other cores drive (`CC-46`).
    fn virtio_9p_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio9p.take() else {
            return;
        };
        let mut raise = false;
        if super::devbus::p9_mmio_write(&mut dev, off, value) {
            let mut mem = self.guest_ram();
            raise = super::devbus::p9_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio9p = Some(dev);
        if raise {
            self.sys_mut().raise_irq(VIRTIO_9P_IRQ);
        }
    }

    /// A `virtio-net` MMIO register write; a `QueueNotify` services the transmit
    /// queue or pumps the NAT through the shared `devbus` (`CC-46`).
    fn virtio_net_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtionet.take() else {
            return;
        };
        let mut raise = false;
        match super::devbus::net_mmio_write(&mut dev, off, value) {
            super::devbus::NetNotify::Transmit => {
                let mut mem = self.guest_ram();
                raise |= super::devbus::net_service_tx(&mut mem, &mut dev);
                raise |= super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::Receive => {
                let mut mem = self.guest_ram();
                raise |= super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::None => {}
        }
        self.sys_mut().virtionet = Some(dev);
        if raise {
            self.sys_mut().raise_irq(VIRTIO_NET_IRQ);
        }
    }

    /// Pump the NAT and deliver pending receive frames into the guest's receive
    /// queue — called periodically from the run loop so host-side data arrives
    /// without the guest having to transmit first (the same shared `devbus` pump
    /// the other cores drive).
    fn virtio_net_pump(&mut self) {
        let Some(mut dev) = self.sys_mut().virtionet.take() else {
            return;
        };
        let mut mem = self.guest_ram();
        let raise = super::devbus::net_pump(&mut mem, &mut dev);
        self.sys_mut().virtionet = Some(dev);
        if raise {
            self.sys_mut().raise_irq(VIRTIO_NET_IRQ);
        }
    }

    #[inline]
    fn sys(&self) -> &Sys {
        self.sys.as_ref().expect("system mode")
    }
    #[inline]
    fn sys_mut(&mut self) -> &mut Sys {
        self.sys.as_mut().expect("system mode")
    }

    // ── device attach + the shared workspace / network surface (CC-46) ───────

    /// Attach a `virtio-blk` κ-disk rootfs (`CC-7`) — the same κ-disk the other
    /// cores boot, serviced by the shared `devbus`.
    pub fn attach_disk(&mut self, rootfs: Vec<u8>) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtio = Some(super::VirtioBlk::new(rootfs));
        }
    }

    /// Attach a shared **workspace filesystem** to the `virtio-9p` device
    /// (`CC-15` parity). `seed` is the files holospaces places on the share; the
    /// guest mounts it (tag `hsworkspace`) and the editor and the running OS read
    /// and write the *same* files over the shared `devbus`.
    pub fn attach_workspace(&mut self, seed: &[(&str, &[u8])]) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        let mut fs = super::ninep::Fs9p::new();
        for (name, data) in seed {
            fs.seed_file(name, data);
        }
        sys.virtio9p = Some(super::Virtio9p::new(fs, super::WORKSPACE_TAG));
    }

    /// Read a file from the shared workspace filesystem — how holospaces observes
    /// the edits the guest made over 9P (`CC-15`).
    #[must_use]
    pub fn workspace_file(&self, name: &str) -> Option<&[u8]> {
        self.sys
            .as_ref()
            .and_then(|s| s.virtio9p.as_ref())
            .and_then(|d| d.fs.read_file(name))
    }

    /// Write a file into the shared workspace — the editor saving content the
    /// running OS reads over `virtio-9p` (one content, Law L1; `CC-17`).
    pub fn workspace_write(&mut self, name: &str, data: &[u8]) {
        if let Some(d) = self.sys.as_mut().and_then(|s| s.virtio9p.as_mut()) {
            d.fs.write_file(name, data);
        }
    }

    /// Attach the **VirtIO network device** + the userspace TCP/IP NAT (`CC-16`
    /// parity): the guest drives a real NIC, its frames terminate in the shared
    /// [`net`](super::net) NAT and stream out over `egress`.
    pub fn attach_net(&mut self, egress: Box<dyn super::net::Egress>) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtionet = Some(super::VirtioNet::new(
                egress,
                Box::new(super::net::NoIngress),
            ));
        }
    }

    /// Enable the **in-process loopback bridge** (ADR-020, `CC-33` parity) on the
    /// already-attached network device: the workbench (same process) can
    /// `dial`/`send`/`recv`/`close` a connection to a server *inside* the guest.
    /// Returns `false` if no network device is attached.
    pub fn enable_loopback(&mut self) -> bool {
        let Some(net) = self.sys.as_mut().and_then(|s| s.virtionet.as_mut()) else {
            return false;
        };
        let (ingress, handle) = super::net::LoopbackIngress::new();
        net.ingress = Box::new(ingress);
        self.sys_mut().loopback = Some(handle);
        true
    }

    /// Dial an in-process connection to the guest's listening `guest_port` over
    /// the loopback bridge (`CC-33`). Returns the connection id, or `None` if the
    /// loopback ingress is not enabled.
    pub fn dial_guest(&mut self, guest_port: u16) -> Option<u32> {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .map(|h| h.dial(guest_port))
    }

    /// Write host bytes toward the guest server on a loopback connection.
    pub fn guest_send(&mut self, id: u32, data: &[u8]) {
        if let Some(h) = self.sys.as_ref().and_then(|s| s.loopback.as_ref()) {
            h.send(id, data);
        }
    }

    /// Drain the guest server's reply bytes on a loopback connection.
    #[must_use]
    pub fn guest_recv(&mut self, id: u32) -> Vec<u8> {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .map(|h| h.recv(id))
            .unwrap_or_default()
    }

    /// Close the host side of a loopback connection.
    pub fn guest_close(&mut self, id: u32) {
        if let Some(h) = self.sys.as_ref().and_then(|s| s.loopback.as_ref()) {
            h.close(id);
        }
    }

    /// Whether a loopback connection is still usable.
    #[must_use]
    pub fn guest_is_open(&self, id: u32) -> bool {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .is_some_and(|h| h.is_open(id))
    }

    // ── V&V device-driver hooks (CC-46) ──────────────────────────────────────

    /// **V&V device-driver hook** (`CC-46`): perform a device-MMIO store at the
    /// guest-physical address `pa`, exactly as a guest's `virtio` driver would —
    /// the same `mmio_write` the executing CPU routes device stores through. A
    /// conformance witness uses it to drive the shared `devbus` devices over the
    /// x86-64 MMIO transport without booting a full guest kernel.
    #[doc(hidden)]
    pub fn vv_mmio_write(&mut self, pa: u64, width: usize, value: u64) {
        self.mmio_write(pa, width, value);
    }

    /// **V&V device-driver hook** (`CC-46`): perform a device-MMIO load at `pa` —
    /// the same path the executing CPU routes device loads through.
    #[doc(hidden)]
    pub fn vv_mmio_read(&mut self, pa: u64, width: usize) -> u64 {
        self.mmio_read(pa, width)
    }

    /// **V&V hook** (`CC-46`): write `bytes` into guest RAM at guest-physical
    /// `pa` — a witness lays out the virtqueue and the T-message buffers a guest
    /// driver would build in RAM.
    #[doc(hidden)]
    pub fn vv_ram_write(&mut self, pa: u64, bytes: &[u8]) {
        let o = (pa - RAM_BASE) as usize;
        self.ram[o..o + bytes.len()].copy_from_slice(bytes);
    }

    /// **V&V hook** (`CC-46`): read `len` bytes of guest RAM at guest-physical
    /// `pa` — a witness reads back the R-message the device scattered and the
    /// used-ring the device updated.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_ram_read(&self, pa: u64, len: usize) -> Vec<u8> {
        let o = (pa - RAM_BASE) as usize;
        self.ram[o..o + len].to_vec()
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of the `virtio-9p` MMIO
    /// slot — a witness drives the device at the same address the x86-64 platform
    /// advertises.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_virtio_9p_base() -> u64 {
        VIRTIO_9P_BASE
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of the `virtio-net` slot.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_virtio_net_base() -> u64 {
        VIRTIO_NET_BASE
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of the `virtio-blk` slot.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_virtio_blk_base() -> u64 {
        VIRTIO_BLK_BASE
    }

    /// Decode + execute one instruction.
    #[allow(clippy::too_many_lines)]
    fn step(&mut self) -> Result<(), Halt> {
        let start = self.rip;
        // Snapshot the architectural register state so a `#PF` latched mid-access
        // can discard this instruction's partial effects and restart it after the
        // handler maps the page (RAM is not snapshotted — early boot's faulting
        // accesses touch a fresh page, so any bytes written before the fault are
        // re-written identically on restart).
        let snap = self.reg_snapshot();
        let mut rex = 0u8;
        let mut opsz = false; // 0x66 operand-size override
        let mut rep = RepKind::None; // F3 (REP/REPE) / F2 (REPNE)
        self.cur_seg = None;
        self.rex_present = false;
        // An x86 instruction is at most 15 bytes. Bounding the prefix scan is a
        // hard correctness/robustness limit: without it, executing a region of
        // repeated prefix bytes (e.g. a wild jump into garbage) spins this loop
        // forever inside one step() — wedging the whole emulator with no interrupt
        // or budget escape. Past the limit, stop consuming prefixes and let the
        // opcode decode (which raises #UD on the over-long encoding, surfacing the
        // problem) — exactly as a real CPU's #GP(0) on a >15-byte instruction.
        for _ in 0..15 {
            let user = self.cpl == 3;
            let p = self.translate_acc(self.rip, false, user) as usize;
            let b = *self.ram.get(p).unwrap_or(&0);
            match b {
                0x66 => opsz = true,
                0xf3 => rep = RepKind::Rep,
                0xf2 => rep = RepKind::Repne,
                0x64 => self.cur_seg = Some(SegId::Fs),
                0x65 => self.cur_seg = Some(SegId::Gs),
                0x67 | 0xf0 | 0x2e | 0x36 | 0x3e | 0x26 => {}
                0x40..=0x4f => {
                    rex = b; // REX (last prefix)
                    self.rex_present = true;
                }
                _ => break,
            }
            self.rip = self.rip.wrapping_add(1);
            if (0x40..=0x4f).contains(&b) {
                break; // REX must be the final prefix
            }
        }
        let op = self.fetch_u8();
        let size: u8 = if rex & 8 != 0 {
            8
        } else if opsz {
            2
        } else {
            4
        };
        // If a prefix/opcode fetch itself faulted (a demand-paged *code* page — the
        // common case once a real userspace binary runs from disk), `translate_acc`
        // has latched the `#PF` and the fetch returned a fallback byte. Do NOT decode
        // that garbage: it can spuriously be an "undefined" opcode and abort the
        // machine. Vector the page fault now and restart the instruction once the
        // kernel maps the code page (operand faults *inside* an arm are handled by
        // the end-of-step vectoring below, which restores the snapshot).
        if let Some(pf) = self.fault.take() {
            self.restore_snapshot(snap);
            self.rip = start;
            self.cr2 = pf.addr;
            self.raise_exception(VEC_PAGE_FAULT, pf.error, true);
            return Ok(());
        }
        match op {
            // ── ALU group (add/or/adc/sbb/and/sub/xor/cmp), all six forms ──
            0x00 | 0x08 | 0x10 | 0x18 | 0x20 | 0x28 | 0x30 | 0x38 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, 1), self.reg8(reg));
                let res = self.alu(op >> 3, a, b, 1);
                if op >> 3 != 7 {
                    self.store_rm(rm, 1, res);
                }
            }
            0x01 | 0x09 | 0x11 | 0x19 | 0x21 | 0x29 | 0x31 | 0x39 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, size), self.r[reg] & Self::mask(size));
                let res = self.alu(op >> 3, a, b, size);
                if op >> 3 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x02 | 0x0a | 0x12 | 0x1a | 0x22 | 0x2a | 0x32 | 0x3a => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.reg8(reg), self.load_rm(rm, 1));
                let res = self.alu(op >> 3, a, b, 1);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(reg), 1, res);
                }
            }
            0x03 | 0x0b | 0x13 | 0x1b | 0x23 | 0x2b | 0x33 | 0x3b => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.r[reg] & Self::mask(size), self.load_rm(rm, size));
                let res = self.alu(op >> 3, a, b, size);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(reg), size, res);
                }
            }
            0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c => {
                let (a, b) = (self.r[0] & 0xff, self.fetch(1));
                let res = self.alu(op >> 3, a, b, 1);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(0), 1, res);
                }
            }
            0x05 | 0x0d | 0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d => {
                let b = self.fetch_imm_z(size);
                let a = self.r[0] & Self::mask(size);
                let res = self.alu(op >> 3, a, b, size);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(0), size, res);
                }
            }
            0x50..=0x57 => {
                let r = (op - 0x50) as usize | (((rex & 1) as usize) << 3);
                let v = self.r[r];
                self.push(v);
            }
            0x58..=0x5f => {
                let r = (op - 0x58) as usize | (((rex & 1) as usize) << 3);
                let v = self.pop();
                self.r[r] = v;
            }
            0x68 => {
                let imm = self.fetch(4) as i32 as i64 as u64;
                self.push(imm);
            }
            0x6a => {
                let imm = self.fetch(1) as i8 as i64 as u64;
                self.push(imm);
            }
            0x70..=0x7f => {
                let rel = self.fetch(1) as i8 as i64;
                if self.cond(op - 0x70) {
                    self.rip = self.rip.wrapping_add(rel as u64);
                }
            }
            0x80 => {
                // The immediate is fetched *before* the operand is touched so a
                // RIP-relative `rm` resolves against the instruction-end `rip`
                // (the same address for the load and the store).
                let (ext, rm) = self.modrm(rex);
                let b = self.fetch(1);
                let a = self.load_rm(rm, 1);
                let res = self.alu((ext & 7) as u8, a, b, 1);
                if ext & 7 != 7 {
                    self.store_rm(rm, 1, res);
                }
            }
            0x81 => {
                let (ext, rm) = self.modrm(rex);
                let b = self.fetch_imm_z(size);
                let a = self.load_rm(rm, size);
                let res = self.alu((ext & 7) as u8, a, b, size);
                if ext & 7 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x83 => {
                let (ext, rm) = self.modrm(rex);
                let b = self.fetch(1) as i8 as i64 as u64;
                let a = self.load_rm(rm, size);
                let res = self.alu((ext & 7) as u8, a, b, size);
                if ext & 7 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x84 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, 1), self.reg8(reg));
                self.flags_logic(a & b, 1);
            }
            0x85 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, size), self.r[reg] & Self::mask(size));
                self.flags_logic(a & b, size);
            }
            0x88 => {
                let (reg, rm) = self.modrm(rex);
                let v = self.reg8(reg);
                self.store_rm(rm, 1, v);
            }
            0x89 => {
                let (reg, rm) = self.modrm(rex);
                let v = self.r[reg] & Self::mask(size);
                self.store_rm(rm, size, v);
            }
            0x8a => {
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, 1);
                self.store_rm(Rm::Reg(reg), 1, v);
            }
            0x8b => {
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, size);
                self.store_rm(Rm::Reg(reg), size, v);
            }
            0x8c => {
                // MOV r/m16, Sreg — store a segment selector.
                let (reg, rm) = self.modrm(rex);
                let sel = u64::from(self.seg.get(reg).map_or(0, |s| s.selector));
                self.store_rm(rm, 2, sel);
            }
            0x8d => {
                let (reg, rm) = self.modrm(rex);
                if let Some(a) = self.rm_addr(rm) {
                    self.store_rm(Rm::Reg(reg), size, a & Self::mask(size));
                }
            }
            0x8e => {
                // MOV Sreg, r/m16 — load a segment selector. In long mode the
                // descriptor base/limit are ignored for DS/ES/SS/CS (flat); only
                // the selector is recorded (FS/GS bases come from their MSRs).
                let (reg, rm) = self.modrm(rex);
                let sel = self.load_rm(rm, 2) as u16;
                if let Some(s) = self.seg.get_mut(reg) {
                    s.selector = sel;
                }
            }
            0x90..=0x97 => {
                // XCHG rAX, r (0x90 with no REX.B is the canonical NOP). REX.B
                // extends the register; the swap is at the operand size.
                let g = (op as usize - 0x90) | (((rex & 1) as usize) << 3);
                if g != RAX {
                    let a = self.r[RAX] & Self::mask(size);
                    let b = self.r[g] & Self::mask(size);
                    self.store_rm(Rm::Reg(RAX), size, b);
                    self.store_rm(Rm::Reg(g), size, a);
                }
            }
            0xa8 => {
                let (a, b) = (self.r[0] & 0xff, self.fetch(1));
                self.flags_logic(a & b, 1);
            }
            0xa9 => {
                let a = self.r[0] & Self::mask(size);
                let b = self.fetch_imm_z(size);
                self.flags_logic(a & b, size);
            }
            0xb0..=0xb7 => {
                let r = (op - 0xb0) as usize | (((rex & 1) as usize) << 3);
                let imm = self.fetch(1);
                self.store_rm(Rm::Reg(r), 1, imm);
            }
            0xb8..=0xbf => {
                let r = (op - 0xb8) as usize | (((rex & 1) as usize) << 3);
                let imm = self.fetch(size); // imm16 / imm32 / imm64 by operand size
                self.store_rm(Rm::Reg(r), size, imm);
            }
            0x63 => {
                // MOVSXD r64, r/m32 — sign-extend a 32-bit operand into 64 bits.
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, 4) as u32 as i32 as i64 as u64;
                self.store_rm(Rm::Reg(reg), size, v & Self::mask(size));
            }
            0x69 => {
                // IMUL r, r/m, imm (imm16 under 0x66, else imm32 sign-extended).
                let (reg, rm) = self.modrm(rex);
                let imm = if size == 2 {
                    self.fetch(2) as i16 as i64
                } else {
                    self.fetch(4) as i32 as i64
                };
                let a = i128::from(sign_extend(self.load_rm(rm, size), size));
                let full = a * i128::from(imm);
                let r = full as u64;
                self.store_rm(Rm::Reg(reg), size, r & Self::mask(size));
                self.set_imul_flags(full, r, size);
            }
            0x6b => {
                // IMUL r, r/m, imm8.
                let (reg, rm) = self.modrm(rex);
                let imm = self.fetch(1) as i8 as i64;
                let a = i128::from(sign_extend(self.load_rm(rm, size), size));
                let full = a * i128::from(imm);
                let r = full as u64;
                self.store_rm(Rm::Reg(reg), size, r & Self::mask(size));
                self.set_imul_flags(full, r, size);
            }
            0x86 => {
                // XCHG r/m8, r8.
                let (reg, rm) = self.modrm(rex);
                let a = self.load_rm(rm, 1);
                let b = self.reg8(reg);
                self.store_rm(rm, 1, b);
                self.store_rm(Rm::Reg(reg), 1, a);
            }
            0x87 => {
                // XCHG r/m, r.
                let (reg, rm) = self.modrm(rex);
                let a = self.load_rm(rm, size);
                let b = self.r[reg] & Self::mask(size);
                self.store_rm(rm, size, b);
                self.store_rm(Rm::Reg(reg), size, a);
            }
            0x8f => {
                // POP r/m64. Per the SDM, when rSP is the base of the
                // destination's effective address, the address is computed
                // *after* rSP is incremented by the pop — so pop first, then
                // decode the ModRM address (e.g. `pop 0x20(%rsp)`).
                let v = self.pop();
                let (_e, rm) = self.modrm(rex);
                self.store_rm(rm, 8, v);
            }
            0x98 => {
                // CBW/CWDE/CDQE — sign-extend AL/AX/EAX to the operand size.
                let v = match size {
                    8 => self.r[RAX] as i32 as i64 as u64,
                    2 => (self.r[RAX] as i8 as i16 as u16) as u64,
                    _ => self.r[RAX] as i16 as i32 as u32 as u64,
                };
                self.store_rm(Rm::Reg(RAX), size, v);
            }
            0x99 => {
                // CWD/CDQ/CQO — sign-extend rax into rdx:rax.
                let neg = Self::sign(self.r[RAX], size);
                let ext = if neg { Self::mask(size) } else { 0 };
                self.store_rm(Rm::Reg(RDX), size, ext);
            }
            0x9c => {
                // PUSHFQ.
                self.push(self.rflags);
            }
            0x9d => {
                // POPFQ — restore the settable flags (CF/PF/AF/ZF/SF/TF/IF/DF/
                // OF/NT/AC/ID). The mask must include IF (bit 9, 0x200): the
                // kernel's `local_irq_restore` is `push; popfq`, so dropping IF
                // here would leave interrupts disabled across every
                // `local_irq_save/restore` region (the `irqs disabled` WARNs).
                // Same settable mask as IRETQ below.
                self.rflags = (self.pop() & 0x0024_4fd5) | 0x2;
            }
            0xa4 => self.string_op(StringOp::Movs, 1, rep, start),
            0xa5 => self.string_op(StringOp::Movs, size, rep, start),
            0xa6 => self.string_op(StringOp::Cmps, 1, rep, start),
            0xa7 => self.string_op(StringOp::Cmps, size, rep, start),
            0xaa => self.string_op(StringOp::Stos, 1, rep, start),
            0xab => self.string_op(StringOp::Stos, size, rep, start),
            0xac => self.string_op(StringOp::Lods, 1, rep, start),
            0xad => self.string_op(StringOp::Lods, size, rep, start),
            0xae => self.string_op(StringOp::Scas, 1, rep, start),
            0xaf => self.string_op(StringOp::Scas, size, rep, start),
            0xc0 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = self.fetch(1) as u8;
                self.shift_rotate((ext & 7) as u8, rm, 1, cnt);
            }
            0xc1 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = self.fetch(1) as u8;
                self.shift_rotate((ext & 7) as u8, rm, size, cnt);
            }
            0xd0 => {
                let (ext, rm) = self.modrm(rex);
                self.shift_rotate((ext & 7) as u8, rm, 1, 1);
            }
            0xd1 => {
                let (ext, rm) = self.modrm(rex);
                self.shift_rotate((ext & 7) as u8, rm, size, 1);
            }
            0xd2 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = (self.r[RCX] & 0xff) as u8;
                self.shift_rotate((ext & 7) as u8, rm, 1, cnt);
            }
            0xd3 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = (self.r[RCX] & 0xff) as u8;
                self.shift_rotate((ext & 7) as u8, rm, size, cnt);
            }
            0xc2 => {
                // RET imm16 — pop the return address, then pop `imm16` arg bytes.
                let n = self.fetch(2);
                let v = self.pop();
                self.rip = v;
                self.r[RSP] = self.r[RSP].wrapping_add(n);
            }
            0xc9 => {
                // LEAVE: rsp = rbp; rbp = pop().
                self.r[RSP] = self.r[RBP];
                let v = self.pop();
                self.r[RBP] = v;
            }
            0xcf => {
                // IRETQ — return from an interrupt: pop RIP, CS, RFLAGS, RSP, SS.
                let rip = self.pop();
                let cs = self.pop();
                let rflags = self.pop();
                let rsp = self.pop();
                let ss = self.pop();
                self.rip = rip;
                self.seg[SegId::Cs as usize].selector = cs as u16;
                self.seg[SegId::Cs as usize].long = true;
                self.cpl = (cs & 3) as u8;
                // Restore the saved RFLAGS. The mask keeps the user-settable
                // status/control bits — crucially `IF` (bit 9): the timer/`virtio`
                // IRQ handlers `IRETQ` back into code that must run with interrupts
                // *enabled* (e.g. `calibrate_delay` waiting on `jiffies`), so
                // dropping `IF` here would wedge the boot in that wait. Bit 1 is the
                // architecturally-always-set reserved bit.
                self.rflags = (rflags & 0x0024_4fd5) | 0x2;
                self.r[RSP] = rsp;
                self.seg[SegId::Ss as usize].selector = ss as u16;
            }
            0xe3 => {
                // JRCXZ rel8.
                let rel = self.fetch(1) as i8 as i64;
                if self.r[RCX] == 0 {
                    self.rip = self.rip.wrapping_add(rel as u64);
                }
            }
            0xf5 => self.set(flag::CF, self.rflags & flag::CF == 0), // CMC
            0xf6 | 0xf7 => {
                let osz = if op == 0xf6 { 1 } else { size };
                self.group3(rex, osz, start)?;
            }
            0xf8 => self.set(flag::CF, false), // CLC
            0xf9 => self.set(flag::CF, true),  // STC
            0xfa => self.rflags &= !RFLAGS_IF, // CLI
            0xfb => self.rflags |= RFLAGS_IF,  // STI
            0xfc => self.rflags &= !RFLAGS_DF, // CLD
            0xfd => self.rflags |= RFLAGS_DF,  // STD
            0xc3 => {
                let v = self.pop();
                self.rip = v;
            }
            0xcb => {
                // LRET (far return): pop RIP then CS. The operand size (REX.W /
                // default) sets the slot width; the selector is the low 16 bits.
                let osz = if rex & 8 != 0 { 8 } else { 4 };
                let rip = self.pop_sized(osz);
                let cs = self.pop_sized(osz);
                self.rip = rip;
                self.seg[SegId::Cs as usize].selector = cs as u16;
                self.seg[SegId::Cs as usize].long = true;
                self.cpl = (cs & 3) as u8;
            }
            0xca => {
                // LRET imm16 (far return, freeing imm16 stack bytes).
                let n = self.fetch(2);
                let osz = if rex & 8 != 0 { 8 } else { 4 };
                let rip = self.pop_sized(osz);
                let cs = self.pop_sized(osz);
                self.rip = rip;
                self.seg[SegId::Cs as usize].selector = cs as u16;
                self.seg[SegId::Cs as usize].long = true;
                self.r[RSP] = self.r[RSP].wrapping_add(n);
            }
            0xc6 => {
                let (_e, rm) = self.modrm(rex);
                let imm = self.fetch(1);
                self.store_rm(rm, 1, imm);
            }
            0xc7 => {
                let (_e, rm) = self.modrm(rex);
                let v = self.fetch_imm_z(size);
                self.store_rm(rm, size, v);
            }
            0xe4 => {
                let port = self.fetch(1) as u16;
                let v = self.port_in(port);
                self.store_rm(Rm::Reg(0), 1, u64::from(v));
            }
            0xe6 => {
                let port = self.fetch(1) as u16;
                let v = (self.r[0] & 0xff) as u8;
                self.port_out(port, v);
            }
            0xe5 => {
                // IN eAX, imm8 — a word/dword port read (operand size).
                let port = self.fetch(1) as u16;
                let v = self.port_in_wide(port, size);
                self.store_rm(Rm::Reg(0), size, v);
            }
            0xe7 => {
                // OUT imm8, eAX — a word/dword port write.
                let port = self.fetch(1) as u16;
                let v = self.r[0] & Self::mask(size);
                self.port_out_wide(port, size, v);
            }
            0xed => {
                // IN eAX, dx — a word/dword port read.
                let port = (self.r[RDX] & 0xffff) as u16;
                let v = self.port_in_wide(port, size);
                self.store_rm(Rm::Reg(0), size, v);
            }
            0xef => {
                // OUT dx, eAX — a word/dword port write.
                let port = (self.r[RDX] & 0xffff) as u16;
                let v = self.r[0] & Self::mask(size);
                self.port_out_wide(port, size, v);
            }
            0xe8 => {
                let rel = self.fetch(4) as i32 as i64;
                let ret = self.rip;
                self.push(ret);
                self.rip = self.rip.wrapping_add(rel as u64);
            }
            0xe9 => {
                let rel = self.fetch(4) as i32 as i64;
                self.rip = self.rip.wrapping_add(rel as u64);
            }
            0xeb => {
                let rel = self.fetch(1) as i8 as i64;
                self.rip = self.rip.wrapping_add(rel as u64);
            }
            0xec => {
                let port = (self.r[2] & 0xffff) as u16;
                let v = self.port_in(port);
                self.store_rm(Rm::Reg(0), 1, u64::from(v));
            }
            0xee => {
                let port = (self.r[2] & 0xffff) as u16;
                let v = (self.r[0] & 0xff) as u8;
                self.port_out(port, v);
            }
            0xf4 => {
                // HLT: with interrupts enabled the CPU idles until the next
                // interrupt — fast-forward the platform timers so a tick is
                // delivered rather than busy-spinning a step at a time. With
                // interrupts masked it is a true stop (the guest power-off).
                if self.rflags & RFLAGS_IF == 0 {
                    if let Some(sys) = self.sys.as_mut() {
                        sys.halted = true;
                    }
                    return Err(Halt::Halted);
                }
                // Leave `rip` *past* the HLT — the architectural state a wakeup
                // interrupt resumes from. The interrupt that ends the halt pushes
                // this address, so `IRET` returns to the instruction after HLT
                // (e.g. the `cli; ret` tail of `default_idle`), letting the idle
                // loop re-check `need_resched` and run a task woken by the timer
                // IRQ. Resetting `rip` back onto HLT would make `IRET` re-enter the
                // halt forever, stranding the woken task — the NO_HZ idle deadlock.
                self.idle_until_interrupt();
            }
            0xfe => {
                let (ext, rm) = self.modrm(rex);
                let a = self.load_rm(rm, 1);
                let sub = ext & 7 == 1;
                let r = if sub {
                    a.wrapping_sub(1)
                } else {
                    a.wrapping_add(1)
                };
                let cf = self.rflags & flag::CF;
                self.flags_arith(a, 1, r, 1, sub);
                self.rflags = (self.rflags & !flag::CF) | cf; // inc/dec preserve CF
                self.store_rm(rm, 1, r);
            }
            0xff => {
                let (ext, rm) = self.modrm(rex);
                match ext & 7 {
                    0 | 1 => {
                        let a = self.load_rm(rm, size);
                        let sub = ext & 7 == 1;
                        let r = if sub {
                            a.wrapping_sub(1)
                        } else {
                            a.wrapping_add(1)
                        };
                        let cf = self.rflags & flag::CF;
                        self.flags_arith(a, 1, r, size, sub);
                        self.rflags = (self.rflags & !flag::CF) | cf;
                        self.store_rm(rm, size, r);
                    }
                    2 => {
                        let t = self.load_rm(rm, 8);
                        let ret = self.rip;
                        self.push(ret);
                        self.rip = t;
                    }
                    4 => self.rip = self.load_rm(rm, 8),
                    6 => {
                        let v = self.load_rm(rm, 8);
                        self.push(v);
                    }
                    _ => return Err(Halt::Undefined(start)),
                }
            }
            0x0f => {
                let op2 = self.fetch_u8();
                match op2 {
                    0x80..=0x8f => {
                        let rel = self.fetch(4) as i32 as i64;
                        if self.cond(op2 - 0x80) {
                            self.rip = self.rip.wrapping_add(rel as u64);
                        }
                    }
                    0x90..=0x9f => {
                        let cc = op2 - 0x90;
                        let (_e, rm) = self.modrm(rex);
                        let v = u64::from(self.cond(cc));
                        self.store_rm(rm, 1, v);
                    }
                    0xb6 => {
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 1);
                        self.store_rm(Rm::Reg(reg), size, v);
                    }
                    0xb7 => {
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 2);
                        self.store_rm(Rm::Reg(reg), size, v);
                    }
                    0xbe => {
                        // MOVSX r, r/m8.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 1) as i8 as i64 as u64;
                        self.store_rm(Rm::Reg(reg), size, v & Self::mask(size));
                    }
                    0xbf => {
                        // MOVSX r, r/m16.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 2) as i16 as i64 as u64;
                        self.store_rm(Rm::Reg(reg), size, v & Self::mask(size));
                    }
                    0xaf => {
                        // IMUL r, r/m (signed two-operand).
                        let (reg, rm) = self.modrm(rex);
                        let a = i128::from(sign_extend(self.r[reg] & Self::mask(size), size));
                        let b = i128::from(sign_extend(self.load_rm(rm, size), size));
                        let full = a * b;
                        let r = full as u64;
                        self.store_rm(Rm::Reg(reg), size, r & Self::mask(size));
                        self.set_imul_flags(full, r, size);
                    }
                    0x20 => {
                        // MOV r64, CRn (mod is ignored — rm is a register).
                        let (cr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            self.r[g] = self.cr(cr_idx);
                        }
                    }
                    0x22 => {
                        // MOV CRn, r64 — install paging (CR3), enable PG/PAE, etc.
                        let (cr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            let v = self.r[g];
                            self.set_cr(cr_idx, v);
                        }
                    }
                    0x21 => {
                        // MOV r64, DRn — read a debug register (cpu_init reads DR6/DR7).
                        let (dr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            self.r[g] = self.dr[dr_idx & 7];
                        }
                    }
                    0x23 => {
                        // MOV DRn, r64 — write a debug register (cpu_init clears them).
                        let (dr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            self.dr[dr_idx & 7] = self.r[g];
                        }
                    }
                    0x30 => {
                        // WRMSR: MSR[ecx] = edx:eax.
                        let ecx = (self.r[RCX] & 0xffff_ffff) as u32;
                        let val = ((self.r[RDX] & 0xffff_ffff) << 32) | (self.r[RAX] & 0xffff_ffff);
                        self.wrmsr(ecx, val);
                    }
                    0x32 => {
                        // RDMSR: edx:eax = MSR[ecx].
                        let ecx = (self.r[RCX] & 0xffff_ffff) as u32;
                        let val = self.rdmsr(ecx);
                        self.r[RAX] = val & 0xffff_ffff;
                        self.r[RDX] = val >> 32;
                    }
                    0x05 => self.syscall_enter(),
                    0x07 => self.sysret(),
                    0x01 => self.group7(rex, start)?,
                    0x00 => {
                        // Group 6: LLDT(/2), LTR(/3), VERR/VERW — load the task
                        // register from the GDT descriptor `LTR` selects.
                        let (ext, rm) = self.modrm(rex);
                        let sel = self.load_rm(rm, 2);
                        if ext & 7 == 3 {
                            self.load_tr(sel as u16);
                        }
                    }
                    0xa2 => self.cpuid(),
                    0x31 => {
                        let tsc = self.read_tsc();
                        self.r[RAX] = tsc & 0xffff_ffff;
                        self.r[RDX] = tsc >> 32;
                    }
                    0x09 | 0x0d | 0x0e | 0x18..=0x1f | 0x77 | 0xae => {
                        // WBINVD/PREFETCHW/FEMMS/NOP(prefetch/hint)/EMMS/fences+fxsave
                        // — no architectural effect the integer boot path observes.
                        // 0x0d (PREFETCHW — the kernel patches SLUB's prefetcht0 to it
                        // for the write-prefetch of the freelist) and 0x18..0x1f take a
                        // ModRM; 0xae usually does too.
                        if matches!(op2, 0x0d | 0x18..=0x1f | 0xae) {
                            let _ = self.modrm(rex);
                        }
                    }
                    0x0b => {
                        #[cfg(feature = "cc44-trace")]
                        {
                            use std::io::Write as _;
                            // Dump the bytes around the operands at a UD2 site
                            // (e.g. __text_poke's BUG() after a failed verify):
                            // rdi=dest, rsi=src, rdx=len.
                            let (dst, src, len) = (self.r[7], self.r[6], self.r[2]);
                            let mut dh = [0u8; 32];
                            let mut sh = [0u8; 32];
                            for i in 0..32u64 {
                                dh[i as usize] = self.rd_virt(dst.wrapping_add(i), 1) as u8;
                                sh[i as usize] = self.rd_virt(src.wrapping_add(i), 1) as u8;
                            }
                            let dpa = self.translate(dst);
                            let _ = writeln!(
                                std::io::stderr(),
                                "\n[cc44-trace] UD2 at {start:#x} rdi(dst)={dst:#x} -> pa={dpa:#x} \
                                 rsi(src)={src:#x} rdx(len)={len} cr3={:#x}\n  dst={dh:02x?}\n  src={sh:02x?}",
                                self.cr3,
                            );
                        }
                        // UD2 (`0F 0B`). On real x86-64 this raises `#UD`, which
                        // the kernel's `exc_invalid_op` → `handle_bug` decodes
                        // against `__bug_table`: a `WARN` prints and *resumes*
                        // (the handler advances past the `ud2`), a `BUG` panics.
                        // So vector `#UD` through the IDT (rip at the faulting
                        // `ud2`, a fault) rather than halting — the boot path hits
                        // `WARN`s (e.g. an initcall returning with IRQs off) that
                        // must be survivable. Only halt if no IDT is installed yet
                        // (the earliest boot, before the kernel's traps exist).
                        if self.sys.as_ref().is_some_and(|s| s.idtr.1 != 0) {
                            self.rip = start;
                            self.raise_exception(6, 0, false);
                            return Ok(());
                        }
                        return Err(Halt::Undefined(start)); // UD2, no handler yet
                    }
                    0x40..=0x4f => {
                        // CMOVcc r, r/m.
                        let cc = op2 - 0x40;
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size);
                        if self.cond(cc) {
                            self.store_rm(Rm::Reg(reg), size, v);
                        }
                    }
                    0xa3 | 0xab | 0xb3 | 0xbb => {
                        // BT / BTS / BTR / BTC r/m, r.
                        let (reg, rm) = self.modrm(rex);
                        let bit = self.r[reg] & Self::mask(size);
                        self.bit_test(rm, size, bit, (op2 >> 3) & 3);
                    }
                    0xba => {
                        // Group 8: BT/BTS/BTR/BTC r/m, imm8.
                        let (ext, rm) = self.modrm(rex);
                        let bit = u64::from(self.fetch(1) as u8);
                        self.bit_test(rm, size, bit, ((ext & 7).wrapping_sub(4) & 3) as u8);
                    }
                    0xb8 if rep == RepKind::Rep => {
                        // POPCNT r, r/m (F3 0F B8) — population count. ZF=(src==0);
                        // CF/OF/SF/AF/PF cleared.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size) & Self::mask(size);
                        self.store_rm(Rm::Reg(reg), size, u64::from(v.count_ones()));
                        self.set(flag::ZF, v == 0);
                        for f in [flag::CF, flag::OF, flag::SF, flag::PF] {
                            self.set(f, false);
                        }
                    }
                    0xbc => {
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size) & Self::mask(size);
                        let bits = u32::from(size) * 8;
                        if rep == RepKind::Rep {
                            // TZCNT (F3 0F BC) — trailing zeros, = width if src==0.
                            let r = if v == 0 { bits } else { v.trailing_zeros() };
                            self.store_rm(Rm::Reg(reg), size, u64::from(r));
                            self.set(flag::CF, v == 0);
                            self.set(flag::ZF, r == 0);
                        } else {
                            // BSF r, r/m — index of the lowest set bit; ZF if zero.
                            self.set(flag::ZF, v == 0);
                            if v != 0 {
                                self.store_rm(Rm::Reg(reg), size, u64::from(v.trailing_zeros()));
                            }
                        }
                    }
                    0xbd => {
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size) & Self::mask(size);
                        let bits = u32::from(size) * 8;
                        if rep == RepKind::Rep {
                            // LZCNT (F3 0F BD) — leading zeros within the width.
                            let r = if v == 0 {
                                bits
                            } else {
                                v.leading_zeros() - (64 - bits)
                            };
                            self.store_rm(Rm::Reg(reg), size, u64::from(r));
                            self.set(flag::CF, v == 0);
                            self.set(flag::ZF, r == 0);
                        } else {
                            // BSR r, r/m — index of the highest set bit.
                            self.set(flag::ZF, v == 0);
                            if v != 0 {
                                let idx = 63 - v.leading_zeros();
                                self.store_rm(Rm::Reg(reg), size, u64::from(idx));
                            }
                        }
                    }
                    0xb0 => self.cmpxchg(rex, 1),
                    0xb1 => self.cmpxchg(rex, size),
                    0xc0 => self.xadd(rex, 1),
                    0xc1 => self.xadd(rex, size),
                    0xa4 | 0xac => {
                        // SHLD/SHRD r/m, r, imm8.
                        let (reg, rm) = self.modrm(rex);
                        let cnt = self.fetch(1) as u8;
                        self.shld_shrd(rm, reg, size, cnt, op2 == 0xac);
                    }
                    0xa5 | 0xad => {
                        // SHLD/SHRD r/m, r, CL.
                        let (reg, rm) = self.modrm(rex);
                        let cnt = (self.r[RCX] & 0xff) as u8;
                        self.shld_shrd(rm, reg, size, cnt, op2 == 0xad);
                    }
                    0xc7 => {
                        // Group 9. Memory form (/1) = CMPXCHG8B/16B; the
                        // register forms /6, /7 (mod==3) are RDRAND/RDSEED —
                        // the core's hardware RNG (advertised via CPUID).
                        let (ext, rm) = self.modrm(rex);
                        match (ext & 7, rm) {
                            (6, Rm::Reg(g)) | (7, Rm::Reg(g)) => self.rdrand(g, size),
                            (1, _) => self.cmpxchg16b(rm, rex & 8 != 0),
                            _ => {}
                        }
                    }
                    0xc8..=0xcf => {
                        // BSWAP r32/r64 — reverse the operand's byte order (the
                        // kernel's RNG/entropy + endian helpers use it). The
                        // register is the low 3 opcode bits, REX.B-extended.
                        let g = (op2 as usize - 0xc8) | (((rex & 1) as usize) << 3);
                        let v = self.r[g] & Self::mask(size);
                        let swapped = if size == 8 {
                            v.swap_bytes()
                        } else {
                            u64::from((v as u32).swap_bytes())
                        };
                        self.store_rm(Rm::Reg(g), size, swapped);
                    }
                    // Everything else in the `0F` map is an SSE/SSE2 (or related)
                    // instruction — the x86-64 baseline vector ISA every stock glibc
                    // binary uses. Dispatch on the mandatory prefix (66/F3/F2/none).
                    other => self.sse_0f(other, rex, opsz, rep, start)?,
                }
            }
            0xcc => {
                // INT3 — the software breakpoint. The kernel's boot-time
                // self-test (`int3_selftest`) and the alternatives/kprobe machinery
                // execute a real `int3` and expect to vector through IDT[3]. `rip`
                // already points past the byte (the trap returns to the next insn).
                self.deliver_interrupt(3, 0, false);
            }
            0xcd => {
                // INT imm8 — software interrupt through IDT[imm8].
                let vec = self.fetch_u8();
                self.deliver_interrupt(vec, 0, false);
            }
            0x9b => {} // FWAIT/WAIT — no pending unmasked FP exception to service.
            0xd8..=0xdf => {
                // x87 FPU escape opcodes. The integer boot path only *initialises*
                // the FPU (`fpu__init_cpu_generic`: `FNINIT`) and probes the control
                // word; it never depends on an x87 *computation*. With `CR0.TS/EM`
                // clear the FPU is "present and usable", so these execute as no-ops
                // here — but their operands must still be consumed so decoding stays
                // aligned. A register-form escape (`DB E3` = FNINIT, `DF E0` =
                // FNSTSW AX, …) is `op,modrm`; a memory form carries a full ModRM.
                let modrm = self.fetch_u8();
                if modrm < 0xc0 {
                    // Memory operand: re-decode it (consume SIB/disp). The byte was
                    // already fetched, so step rip back over it first.
                    self.rip = self.rip.wrapping_sub(1);
                    let (_r, rm) = self.modrm(rex);
                    // FNSTCW/FNSTSW store a benign control/status word so the
                    // kernel's read-back probe sees a sane FPU (`0x037f` default CW,
                    // `0` status); other memory forms are ignored.
                    match (op, (modrm >> 3) & 7) {
                        (0xd9, 7) => self.store_rm(rm, 2, 0x037f),        // FNSTCW
                        (0xdd, 7) | (0xdf, 7) => self.store_rm(rm, 2, 0), // FNSTSW
                        _ => {}
                    }
                }
                // Register-form escapes (modrm >= 0xc0) are pure FPU-internal ops;
                // the second byte is already consumed — nothing more to do.
            }
            _ => return Err(Halt::Undefined(start)),
        }
        // A page fault latched while executing this instruction: discard its
        // partial effects (restore the pre-instruction registers, `rip = start`),
        // set `CR2`, and vector `#PF` so the kernel's early page-fault handler maps
        // the page; the instruction re-runs on return (the real long-mode boot's
        // demand-paging of boot data through `early_top_pgt`).
        if let Some(pf) = self.fault.take() {
            self.restore_snapshot(snap);
            self.rip = start;
            self.cr2 = pf.addr;
            self.raise_exception(VEC_PAGE_FAULT, pf.error, true);
        }
        Ok(())
    }

    /// Capture the architectural register state restored on a mid-instruction
    /// `#PF` (general registers, `rip`, `rflags`, segments, `cpl`). RAM and the
    /// device/`sys` state are not snapshotted (see [`Cpu::step`]).
    fn reg_snapshot(&self) -> RegSnapshot {
        RegSnapshot {
            r: self.r,
            rip: self.rip,
            rflags: self.rflags,
            seg: self.seg,
            cpl: self.cpl,
            xmm: self.xmm,
        }
    }

    /// Restore a [`RegSnapshot`] taken at the start of an instruction.
    fn restore_snapshot(&mut self, s: RegSnapshot) {
        self.r = s.r;
        self.rip = s.rip;
        self.rflags = s.rflags;
        self.seg = s.seg;
        self.cpl = s.cpl;
        self.xmm = s.xmm;
    }
}

/// The pre-instruction architectural state restored when a `#PF` is latched
/// mid-instruction, so the faulting instruction restarts cleanly after the
/// handler maps the page ([`Cpu::step`]).
#[derive(Clone, Copy)]
struct RegSnapshot {
    r: [u64; 16],
    rip: u64,
    rflags: u64,
    seg: [Seg; 6],
    cpl: u8,
    xmm: [u128; 16],
}

// The guest-physical layout the 64-bit boot protocol uses (a low region below
// the kernel's 16 MiB load address, mirroring the firecracker/kvmtool loaders).
const ZERO_PAGE: u64 = 0x7000; // struct boot_params (the "zero page")
const CMDLINE_ADDR: u64 = 0x20000; // the kernel command line
                                   // ACPI tables in the e820-reserved 0xF0000 block (16-byte-aligned RSDP).
const RSDP_PHYS: u64 = 0x000F_0000;
const RSDT_PHYS: u64 = 0x000F_0040;
const MADT_PHYS: u64 = 0x000F_0080;
const LAPIC_BASE_PHYS: u32 = 0xFEE0_0000;
const IOAPIC_BASE_PHYS: u32 = 0xFEC0_0000;
const BOOT_GDT: u64 = 0x500; // the boot GDT (3 flat descriptors)
const BOOT_PML4: u64 = 0x9000; // the loader's identity page tables
const BOOT_PDPT: u64 = 0xa000;

impl Cpu {
    /// Boot a real, unmodified x86-64 Linux `vmlinux` ELF to userspace via the
    /// 64-bit Linux boot protocol (`#12`, the x86-64 realization of `CC-36`).
    /// `kernel` is the *uncompressed* ELF; the loader lays out its `PT_LOAD`
    /// segments at their physical addresses, builds `boot_params` (the zero page,
    /// with an e820 map + the command line), a flat GDT, and identity page tables,
    /// enables long mode, and enters `startup_64` with `rsi` = the zero page —
    /// no real-mode setup, no in-guest decompressor. Drive it with [`Cpu::run`].
    #[must_use]
    pub fn boot_linux(ram_bytes: usize, kernel: &[u8], cmdline: &str) -> Self {
        Self::boot_linux_inner(ram_bytes, kernel, cmdline, None)
    }

    /// Boot like [`Cpu::boot_linux`], additionally attaching a **`virtio-blk`
    /// root filesystem** (`CC-45`): `rootfs` is the assembled image taken as
    /// κ-addressed content into the κ-disk (`CC-7`); the guest mounts it over
    /// `/dev/vda`. The shared `devbus` services the device — the same κ-disk the
    /// other cores boot. Use a `cmdline` with `root=/dev/vda`.
    #[must_use]
    pub fn boot_linux_disk(
        ram_bytes: usize,
        kernel: &[u8],
        rootfs: Vec<u8>,
        cmdline: &str,
    ) -> Self {
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            cmdline,
            Some(super::VirtioBlk::new(rootfs)),
        )
    }

    /// Boot like [`Cpu::boot_linux_disk`], but page the κ-disk from a supplied
    /// [`KappaStore`](hologram_substrate_core::KappaStore) by **streaming**
    /// `sector_count` sectors from `read` (no full image in RAM) — the x86-64
    /// analogue of [`aarch64::Cpu::boot_linux_disk_streamed`](super::aarch64::Cpu::boot_linux_disk_streamed):
    /// the browser peer reads each sector from the OPFS rootfs into the OPFS-backed
    /// store, so a real amd64 image boots without ever materializing the whole
    /// `Vec` (the paged κ-disk, the same `KappaBacking` every core uses, `CC-7`).
    #[must_use]
    pub fn boot_linux_disk_streamed<R: FnMut(u64, &mut [u8])>(
        ram_bytes: usize,
        kernel: &[u8],
        cmdline: &str,
        store: alloc::boxed::Box<dyn hologram_substrate_core::KappaStore>,
        sector_count: u64,
        read: R,
    ) -> Self {
        let backing = super::KappaBacking::from_sectors(store, sector_count, read);
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            cmdline,
            Some(super::VirtioBlk::with_backing(backing)),
        )
    }

    /// Boot like [`Cpu::boot_linux_disk_streamed`], but page the κ-disk by its
    /// **occupancy** — the *build-capable* boot path (`CC-45`). The guest sees a
    /// `sector_count`-sector disk (declare it multi-GiB, room to compile in-guest),
    /// yet only the sectors `occupied` yields as `(index, bytes)` — the non-zero
    /// blocks the sparse assembler actually wrote — are indexed and paged. Boot
    /// setup is therefore **O(content), not O(disk)**: an 8 GiB disk holding a few
    /// hundred MiB boots as fast as its content, because the holes are skipped
    /// entirely (Laws L3/L4). Parametric in the image — any devcontainer / OCI
    /// rootfs of any declared size, the same `KappaBacking` every core uses.
    #[must_use]
    pub fn boot_linux_disk_occupancy<I: IntoIterator<Item = (u64, [u8; super::DISK_SECTOR])>>(
        ram_bytes: usize,
        kernel: &[u8],
        cmdline: &str,
        store: alloc::boxed::Box<dyn hologram_substrate_core::KappaStore>,
        sector_count: u64,
        occupied: I,
    ) -> Self {
        let backing = super::KappaBacking::from_occupancy(store, sector_count, occupied);
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            cmdline,
            Some(super::VirtioBlk::with_backing(backing)),
        )
    }

    /// Boot by occupancy from a **streamed** medium — the deployed browser path for
    /// an arbitrarily large devcontainer disk (`CC-45`). The κ-disk is declared at
    /// `sector_count` sectors but paged **O(content)**: only the `occupied_blocks`
    /// the sparse assembler wrote are read (each `sectors_per_block` sectors) through
    /// the `read` callback (the OPFS file), so a multi-GiB build-capable disk boots
    /// in proportion to its content, never reading the holes or holding the image.
    /// The streaming union of [`boot_linux_disk_streamed`](Self::boot_linux_disk_streamed)
    /// and [`boot_linux_disk_occupancy`](Self::boot_linux_disk_occupancy).
    #[allow(clippy::too_many_arguments)] // the κ-disk's full descriptor: store, geometry, occupancy, medium
    pub fn boot_linux_disk_occupancy_streamed<R: FnMut(u64, &mut [u8])>(
        ram_bytes: usize,
        kernel: &[u8],
        cmdline: &str,
        store: alloc::boxed::Box<dyn hologram_substrate_core::KappaStore>,
        sector_count: u64,
        occupied_blocks: &[u64],
        sectors_per_block: u64,
        read: R,
    ) -> Self {
        let backing = super::KappaBacking::from_occupancy_streamed(
            store,
            sector_count,
            occupied_blocks,
            sectors_per_block,
            read,
        );
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            cmdline,
            Some(super::VirtioBlk::with_backing(backing)),
        )
    }

    fn boot_linux_inner(
        ram_bytes: usize,
        kernel: &[u8],
        cmdline: &str,
        disk: Option<super::VirtioBlk>,
    ) -> Self {
        // Validate the guest sizing up front so callers of the public
        // `boot_linux*` APIs get a clear message rather than a slice OOB / e820
        // underflow deep inside. The boot needs the legacy 1 MiB region plus room
        // for the NUL-terminated command line at CMDLINE_ADDR.
        let cl = cmdline.as_bytes();
        let cmdline_end = CMDLINE_ADDR as usize + cl.len() + 1;
        assert!(
            ram_bytes > 0x0010_0000 && ram_bytes >= cmdline_end,
            "boot_linux: ram_bytes={ram_bytes:#x} too small — need > 1 MiB and \
             ≥ {cmdline_end:#x} for the command line at {CMDLINE_ADDR:#x}",
        );

        let mut cpu = Cpu::new(ram_bytes);
        let entry = cpu.load_elf64(kernel);

        // The command line + the zero page (boot_params).
        cpu.ram[CMDLINE_ADDR as usize..CMDLINE_ADDR as usize + cl.len()].copy_from_slice(cl);
        cpu.ram[CMDLINE_ADDR as usize + cl.len()] = 0;
        cpu.build_boot_params(cmdline, ram_bytes as u64, disk.is_some());
        // ACPI RSDP/RSDT/MADT so Linux uses the I/O APIC (symmetric I/O mode).
        cpu.build_acpi_tables();

        // The boot GDT: a null descriptor, a 64-bit code segment (__BOOT_CS,
        // selector 0x10), and a flat data segment (__BOOT_DS, selector 0x18) —
        // the segments the 64-bit boot protocol requires.
        cpu.build_boot_gdt();

        // Identity page tables: a single PML4 → PDPT mapping the low 512 GiB via
        // 1 GiB pages, so the entered kernel (and the zero page / command line /
        // its own code at 16 MiB) is reachable until startup_64 installs its own.
        cpu.build_boot_paging();

        // Enter long mode: PAE + PG + EFER.LME/LMA, CS = the long-mode code
        // segment, the data segments flat, rsi = the zero page, rip = startup_64.
        cpu.cr3 = BOOT_PML4;
        cpu.cr4 = 1 << 5; // PAE
        cpu.efer = (1 << 8) | (1 << 10); // LME | LMA
        cpu.cr0 = (1 << 0) | (1 << 31); // PE | PG
        cpu.seg[SegId::Cs as usize] = Seg {
            selector: 0x10,
            base: 0,
            long: true,
        };
        for s in [SegId::Ds, SegId::Es, SegId::Ss, SegId::Fs, SegId::Gs] {
            cpu.seg[s as usize] = Seg {
                selector: 0x18,
                base: 0,
                long: false,
            };
        }
        cpu.sys_mut().gdtr = (BOOT_GDT, 0x17);
        cpu.r[RSI] = ZERO_PAGE;
        cpu.rip = entry;
        cpu.rflags = 0x2; // interrupts off, as on entry
        cpu.cpl = 0;
        cpu.sys_mut().virtio = disk;
        cpu
    }

    /// Load an ELF64 executable's `PT_LOAD` segments at their physical addresses
    /// (zeroing `bss` up to `p_memsz`) and return the entry point. The kernel's
    /// `vmlinux` is `ET_EXEC`; its segments load at the physical column of each
    /// program header.
    fn load_elf64(&mut self, elf: &[u8]) -> u64 {
        let rd16 = |o: usize| u16::from_le_bytes([elf[o], elf[o + 1]]);
        let rd32 = |o: usize| u32::from_le_bytes([elf[o], elf[o + 1], elf[o + 2], elf[o + 3]]);
        let rd64 = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&elf[o..o + 8]);
            u64::from_le_bytes(b)
        };
        assert_eq!(&elf[0..4], b"\x7fELF", "vmlinux is an ELF");
        let entry = rd64(24);
        let phoff = rd64(32) as usize;
        let phentsize = rd16(54) as usize;
        let phnum = rd16(56) as usize;
        for i in 0..phnum {
            let ph = phoff + i * phentsize;
            if rd32(ph) != 1 {
                continue; // PT_LOAD only
            }
            let offset = rd64(ph + 8) as usize;
            let paddr = rd64(ph + 24);
            let filesz = rd64(ph + 32) as usize;
            let memsz = rd64(ph + 40) as usize;
            let dst = paddr as usize;
            let n = filesz.min(elf.len().saturating_sub(offset));
            // Fail fast on a malformed/oversized image rather than silently
            // skipping the segment (which would leave a partially-loaded kernel
            // that executes into undefined behavior). This is a public boot API.
            assert!(
                dst.checked_add(memsz)
                    .is_some_and(|end| end <= self.ram.len()),
                "load_elf64: PT_LOAD segment [{dst:#x}, {:#x}) does not fit in {} bytes \
                 of guest RAM — increase ram_bytes or check the kernel image",
                dst.saturating_add(memsz),
                self.ram.len(),
            );
            assert!(
                offset + n <= elf.len(),
                "load_elf64: PT_LOAD file range exceeds the image"
            );
            self.ram[dst..dst + n].copy_from_slice(&elf[offset..offset + n]);
            for b in &mut self.ram[dst + n..dst + memsz] {
                *b = 0;
            }
        }
        entry
    }

    /// Build the boot GDT in low RAM: null, `__BOOT_CS` (0x10, 64-bit code), and
    /// `__BOOT_DS` (0x18, flat data) — the descriptors the entered kernel runs on
    /// until it reloads its own with `LGDT`.
    fn build_boot_gdt(&mut self) {
        let put = |ram: &mut [u8], sel: u64, desc: u64| {
            ram[BOOT_GDT as usize + sel as usize..BOOT_GDT as usize + sel as usize + 8]
                .copy_from_slice(&desc.to_le_bytes());
        };
        put(&mut self.ram, 0x00, 0);
        // 64-bit code: present, ring 0, code, executable/readable, L=1 (long).
        put(&mut self.ram, 0x10, 0x00af_9b00_0000_ffff);
        // Flat data: present, ring 0, data, read/write.
        put(&mut self.ram, 0x18, 0x00cf_9300_0000_ffff);
    }

    /// Build identity page tables: one PML4 entry → one PDPT whose first 512
    /// entries are 1 GiB pages mapping the low 512 GiB linearly. Enough that the
    /// entered kernel, the zero page, the command line, and the kernel's own load
    /// region are all reachable until `startup_64` installs its own tables.
    fn build_boot_paging(&mut self) {
        // PML4[0] → PDPT.
        self.ram[BOOT_PML4 as usize..BOOT_PML4 as usize + 8]
            .copy_from_slice(&(BOOT_PDPT | 0x3).to_le_bytes());
        for i in 0..512u64 {
            // 1 GiB pages: present | rw | page-size (bit 7).
            let e = (i << 30) | 0x83;
            let off = BOOT_PDPT as usize + i as usize * 8;
            self.ram[off..off + 8].copy_from_slice(&e.to_le_bytes());
        }
    }

    /// Build `struct boot_params` (the zero page): the setup-header fields the
    /// 64-bit protocol requires (`type_of_loader`, `loadflags`, `cmd_line_ptr`,
    /// the heap/ramdisk fields) and an **e820 memory map** so the kernel discovers
    /// RAM, the MMIO/APIC holes, and reserved low memory.
    fn build_boot_params(&mut self, _cmdline: &str, ram_bytes: u64, _has_disk: bool) {
        let zp = ZERO_PAGE as usize;
        let put32 = |ram: &mut [u8], off: usize, v: u32| {
            ram[zp + off..zp + off + 4].copy_from_slice(&v.to_le_bytes());
        };
        let put16 = |ram: &mut [u8], off: usize, v: u16| {
            ram[zp + off..zp + off + 2].copy_from_slice(&v.to_le_bytes());
        };
        let put8 = |ram: &mut [u8], off: usize, v: u8| ram[zp + off] = v;
        // struct setup_header (within boot_params at 0x1f1). The boot-protocol
        // signature + version the kernel checks before honouring cmd_line_ptr.
        put8(&mut self.ram, 0x1f1, 0); // setup_sects (unused on the 64-bit entry)
                                       // "HdrS" header magic at 0x202; boot-protocol version 2.15 at 0x206.
        self.ram[zp + 0x202..zp + 0x206].copy_from_slice(b"HdrS");
        put16(&mut self.ram, 0x206, 0x020f);
        // type_of_loader (0x210): 0xFF = undefined/unknown loader (accepted).
        put8(&mut self.ram, 0x210, 0xff);
        // loadflags (0x211): LOADED_HIGH (bit0) | CAN_USE_HEAP (bit7).
        put8(&mut self.ram, 0x211, 0x81);
        // cmd_line_ptr (0x228) — the 32-bit physical address of the command line.
        put32(&mut self.ram, 0x228, CMDLINE_ADDR as u32);
        // cmdline_size (0x238) — the maximum command-line length.
        put32(&mut self.ram, 0x238, 0x800);
        // ramdisk_image / ramdisk_size (0x218 / 0x21c): none (initramfs embedded).
        put32(&mut self.ram, 0x218, 0);
        put32(&mut self.ram, 0x21c, 0);

        // The e820 map (boot_params.e820_entries at 0x1e8, e820_table at 0x2d0;
        // each entry is 20 bytes: u64 addr, u64 size, u32 type). type 1 = RAM,
        // type 2 = reserved.
        let mut entries: Vec<(u64, u64, u32)> = Vec::new();
        // Low RAM below the legacy 640 KiB / 1 MiB region: usable up to 0x9fc00,
        // reserved EBDA/BIOS to 1 MiB.
        entries.push((0x0000_0000, 0x0009_fc00, 1));
        entries.push((0x0009_fc00, 0x0000_0400, 2));
        entries.push((0x000f_0000, 0x0001_0000, 2));
        // Main RAM from 1 MiB up to the MMIO window (kept below 0xD000_0000).
        let main_end = ram_bytes.min(VIRTIO_BLK_BASE);
        // `saturating_sub`: `main_end ≥ 1 MiB` is guaranteed by the sizing assert
        // in `boot_linux_inner`, but stay underflow-safe regardless of caller.
        entries.push((0x0010_0000, main_end.saturating_sub(0x0010_0000), 1));
        // The virtio-mmio + APIC windows are reserved (not RAM).
        entries.push((VIRTIO_BLK_BASE, LAPIC_END - VIRTIO_BLK_BASE, 2));
        // Any RAM above 4 GiB (if sized that large) is usable.
        if ram_bytes > 0x1_0000_0000 {
            entries.push((0x1_0000_0000, ram_bytes - 0x1_0000_0000, 1));
        }
        let n = entries.len().min(128) as u8;
        self.ram[zp + 0x1e8] = n;
        for (i, (addr, size, ty)) in entries.iter().take(128).enumerate() {
            let off = zp + 0x2d0 + i * 20;
            self.ram[off..off + 8].copy_from_slice(&addr.to_le_bytes());
            self.ram[off + 8..off + 16].copy_from_slice(&size.to_le_bytes());
            self.ram[off + 16..off + 20].copy_from_slice(&ty.to_le_bytes());
        }
        // The RSDP physical address handed to the kernel via boot_params (0x070);
        // built by `build_acpi_tables`. Without it (no BIOS to scan) Linux finds no
        // MADT and falls back to virtual-wire/PIC mode.
        self.ram[zp + 0x070..zp + 0x078].copy_from_slice(&RSDP_PHYS.to_le_bytes());
    }

    /// Write the ACPI tables (RSDP → RSDT → MADT) into low reserved memory so Linux
    /// discovers the local + I/O APIC and takes the *symmetric I/O* interrupt path
    /// (the tickless LAPIC-timer regime), instead of legacy virtual-wire/PIC mode.
    /// The MADT advertises one enabled CPU, the I/O APIC at `0xFEC0_0000` with 24
    /// GSIs, and the ISA IRQ0→GSI2 source override (the PIT pin the kernel's
    /// `check_timer` expects). Tables live in the e820-reserved `0xF0000` block.
    fn build_acpi_tables(&mut self) {
        // 8-bit checksum: the byte that makes the sum of all `Length` bytes == 0.
        fn checksum(t: &[u8]) -> u8 {
            0u8.wrapping_sub(t.iter().fold(0u8, |a, &b| a.wrapping_add(b)))
        }
        const OEMID: &[u8; 6] = b"HOLOS\0";
        const OEM_TABLE: &[u8; 8] = b"HOLOSPCS";

        // Common 36-byte ACPI table header (signature, length back-filled, rev,
        // checksum back-filled, OEM fields). Returns the header bytes.
        let header = |sig: &[u8; 4], revision: u8| -> Vec<u8> {
            let mut h = Vec::new();
            h.extend_from_slice(sig);
            h.extend_from_slice(&0u32.to_le_bytes()); // length (back-filled)
            h.push(revision);
            h.push(0); // checksum (back-filled)
            h.extend_from_slice(OEMID);
            h.extend_from_slice(OEM_TABLE);
            h.extend_from_slice(&1u32.to_le_bytes()); // OEM revision
            h.extend_from_slice(&0u32.to_le_bytes()); // creator id
            h.extend_from_slice(&1u32.to_le_bytes()); // creator revision
            h
        };
        let finalize = |t: &mut Vec<u8>| {
            let len = t.len() as u32;
            t[4..8].copy_from_slice(&len.to_le_bytes());
            t[9] = 0;
            t[9] = checksum(t);
        };

        // ── MADT (APIC) ──────────────────────────────────────────────────────
        let mut madt = header(b"APIC", 4);
        madt.extend_from_slice(&LAPIC_BASE_PHYS.to_le_bytes()); // local APIC addr
        madt.extend_from_slice(&1u32.to_le_bytes()); // Flags: PCAT_COMPAT (8259 present)
                                                     // Type 0 — Processor Local APIC (UID 0, APIC ID 0, enabled).
        madt.extend_from_slice(&[0, 8, 0, 0]);
        madt.extend_from_slice(&1u32.to_le_bytes());
        // Type 1 — I/O APIC (ID 0, addr 0xFEC00000, GSI base 0).
        madt.extend_from_slice(&[1, 12, 0, 0]);
        madt.extend_from_slice(&IOAPIC_BASE_PHYS.to_le_bytes());
        madt.extend_from_slice(&0u32.to_le_bytes());
        // Type 2 — Interrupt Source Override: ISA IRQ0 → GSI 2, bus-conforming.
        madt.extend_from_slice(&[2, 10, 0, 0]);
        madt.extend_from_slice(&2u32.to_le_bytes());
        madt.extend_from_slice(&0u16.to_le_bytes());
        // Type 4 — Local APIC NMI: all CPUs (UID 0xFF), LINT1.
        madt.extend_from_slice(&[4, 6, 0xff, 0, 0, 1]);
        finalize(&mut madt);

        // ── RSDT (one entry → MADT) ──────────────────────────────────────────
        let mut rsdt = header(b"RSDT", 1);
        rsdt.extend_from_slice(&(MADT_PHYS as u32).to_le_bytes());
        finalize(&mut rsdt);

        // ── RSDP (ACPI 1.0, 20 bytes) ────────────────────────────────────────
        let mut rsdp = Vec::new();
        rsdp.extend_from_slice(b"RSD PTR ");
        rsdp.push(0); // checksum (back-filled)
        rsdp.extend_from_slice(OEMID);
        rsdp.push(0); // revision (ACPI 1.0)
        rsdp.extend_from_slice(&(RSDT_PHYS as u32).to_le_bytes());
        rsdp[8] = checksum(&rsdp);

        let write = |ram: &mut [u8], at: u64, bytes: &[u8]| {
            let a = at as usize;
            ram[a..a + bytes.len()].copy_from_slice(bytes);
        };
        write(&mut self.ram, RSDP_PHYS, &rsdp);
        write(&mut self.ram, RSDT_PHYS, &rsdt);
        write(&mut self.ram, MADT_PHYS, &madt);
    }

    // ── platform timers + interrupt delivery (the long-mode boot path; #12) ────

    /// Advance the architected timestamp counter and the platform timers one
    /// step; latch a timer interrupt (PIT IRQ 0 or the APIC-timer LVT vector)
    /// when one expires. The kernel calibrates against these and runs its
    /// periodic tick on them.
    ///
    /// The periodic IRQ sources (PIT channel-0, the APIC-timer LVT) count down
    /// every step *unconditionally* — the same model the riscv64 (`mtime`) and
    /// aarch64 (`counter`) cores use, where the timer advances regardless of the
    /// interrupt-acceptance state and only *delivery* of the latched interrupt is
    /// gated on `RFLAGS.IF`. The TSC and the channel-2 calibration one-shot
    /// advance at the same rate (one coherent time-base).
    fn sys_tick(&mut self) {
        let at_boundary = {
            let Some(sys) = self.sys.as_mut() else {
                return;
            };
            // The TSC advances every step (fine `sched_clock` resolution).
            sys.tsc = sys.tsc.wrapping_add(TSC_PER_STEP);
            // The PIT/APIC down-counters advance at ONE shared rate — once per
            // `TICK_DIV` steps. A single rate keeps the calibration reference (PIT
            // channel 2) and the tick sources (PIT channel 0, the APIC-timer) in
            // lockstep, so the LAPIC clockevent the kernel calibrates against the PIT
            // fires at exactly the rate the kernel expects — jiffies advance correctly
            // (no crawl). The period (`reload × TICK_DIV` steps) is far longer than the
            // handler (no storm) while channel 2 still expires inside
            // `quick_pit_calibrate`'s poll budget. One coherent time-base, like the
            // riscv64 (`mtime`) / aarch64 (`counter`) cores.
            sys.tdiv = sys.tdiv.wrapping_add(1);
            sys.tdiv % TICK_DIV == 0
        };
        if at_boundary {
            // The per-`TICK_DIV` PIT/APIC down-counter body.
            self.tick_down_counters();
        }
    }

    /// Advance the timers to the next armed deadline and latch its interrupt —
    /// called from `HLT` with interrupts enabled. The wake time is computed from
    /// the guest's *own* programmed timer counters (PIT channel-0, APIC-timer),
    /// so it scales to any guest/HZ with no fixed iteration cap — the same O(1)
    /// deadline-jump the riscv64 (`mtime = mtimecmp - 1`) and aarch64
    /// (`counter = deadline - 1`) WFI paths use. No loop, no arbitrary bound.
    fn idle_until_interrupt(&mut self) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        // Decrements remaining until each armed periodic source fires (each source
        // decrements once per `TICK_DIV` steps in `sys_tick`).
        let pit = (sys.pit.enabled && sys.pit.reload != 0).then(|| {
            u64::from(if sys.pit.counter == 0 {
                u32::from(sys.pit.reload)
            } else {
                sys.pit.counter
            })
        });
        // Fast-forward to the local-APIC timer's deadline only when it is armed
        // (current_count != 0) and will actually fire (LVT unmasked) — matching the
        // run-loop counter, which now runs whenever armed and stops after a one-shot
        // expiry until re-armed.
        let apic = (sys.lapic.enabled()
            && sys.lapic.current_count != 0
            && sys.lapic.lvt_timer & (1 << 16) == 0)
            .then(|| u64::from(sys.lapic.current_count));
        // No armed timer → nothing to fast-forward to; leave the CPU halted (the
        // run loop keeps pumping devices/network and an external IRQ still wakes
        // it), exactly as hardware idles until an interrupt.
        let Some(ticks) = [pit, apic].into_iter().flatten().min() else {
            return;
        };
        // Jump to the nearest deadline: `ticks` down-counter decrements =
        // `ticks × TICK_DIV` steps. Advance the (every-step) TSC and the divisor
        // phase by the step count; the down-counters advance by `ticks`. Then fire
        // the source(s) that reached the deadline and advance the rest.
        let steps = ticks.saturating_mul(TICK_DIV);
        sys.tsc = sys.tsc.wrapping_add(steps.saturating_mul(TSC_PER_STEP));
        sys.tdiv = sys.tdiv.wrapping_add(steps);
        if sys.pit.ch2_gate && !sys.pit.ch2_out && sys.pit.ch2_counter > 0 {
            if u64::from(sys.pit.ch2_counter) <= ticks {
                sys.pit.ch2_counter = 0;
                sys.pit.ch2_out = true;
            } else {
                sys.pit.ch2_counter -= ticks as u32;
            }
        }
        if let Some(pt) = pit {
            if pt == ticks {
                sys.raise_irq(0);
                if sys.pit.ch0_periodic {
                    sys.pit.counter = u32::from(sys.pit.reload);
                } else {
                    sys.pit.counter = 0;
                    sys.pit.enabled = false;
                }
            } else {
                sys.pit.counter = (pt - ticks) as u32;
            }
        }
        if let Some(at) = apic {
            if at == ticks {
                sys.lapic.current_count = if sys.lapic.lvt_timer & (1 << 17) != 0 {
                    sys.lapic.initial_count
                } else {
                    0
                };
                sys.lapic.set_irr((sys.lapic.lvt_timer & 0xff) as u8);
            } else {
                sys.lapic.current_count = (at - ticks) as u32;
            }
        }
    }

    /// Deliver a pending external interrupt through the IDT if `RFLAGS.IF` is set
    /// and the kernel has installed an IDT. Returns `true` if an interrupt was
    /// taken (the caller then re-enters the run loop at the handler).
    fn take_pending_interrupt(&mut self) -> bool {
        if self.rflags & RFLAGS_IF == 0 {
            return false;
        }
        let (vector, irq) = {
            let Some(sys) = self.sys.as_ref() else {
                return false;
            };
            if sys.idtr.1 == 0 {
                return false;
            }
            if let Some(v) = sys.lapic.deliverable() {
                (v, None)
            } else if let Some((irq, vec)) = sys.pic.pending() {
                (vec, Some(irq))
            } else {
                return false;
            }
        };
        // Acknowledge the source.
        if let Some(sys) = self.sys.as_mut() {
            if let Some(irq) = irq {
                sys.pic.ack(irq);
            } else {
                sys.lapic.take(vector);
            }
        }
        self.deliver_interrupt(vector, 0, false);
        true
    }

    /// Vector through the 64-bit IDT: push the interrupt frame (`SS,RSP,RFLAGS,
    /// CS,RIP`, and the error code for the faults that carry one), switch to the
    /// gate's stack on a ring change, clear `IF`, and jump to the handler.
    fn deliver_interrupt(&mut self, vector: u8, error: u64, has_error: bool) {
        self.int_counts[vector as usize] = self.int_counts[vector as usize].wrapping_add(1);
        let (idt_base, _) = self.sys().idtr;
        let desc = idt_base + u64::from(vector) * 16;
        let lo = self.rd_virt(desc, 8);
        let hi = self.rd_virt(desc + 8, 8);
        let off = (lo & 0xffff) | ((lo >> 32) & 0xffff_0000) | ((hi & 0xffff_ffff) << 32);
        let ist = (lo >> 32) & 0x7;
        // dev-only (`cc44-trace`): log CPU-exception vectors (< 32) as they vector
        // through the IDT — the fault type + faulting RIP + CR2 + target handler.
        // External IRQs (≥ 32) are omitted: they fire thousands of times per boot.
        #[cfg(feature = "cc44-trace")]
        if vector < 32 {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "\n[cc44-trace] VECTOR={vector} err={error:#x} from rip={:#x} cr2={:#x} -> handler={off:#x} ist={ist} rsp={:#x} if={}",
                self.rip,
                self.cr2,
                self.r[RSP],
                self.rflags & RFLAGS_IF != 0,
            );
        }
        let old_cs = self.seg[SegId::Cs as usize].selector;
        let old_ss = self.seg[SegId::Ss as usize].selector;
        let old_rsp = self.r[RSP];
        let old_cpl = self.cpl;

        // A ring transition (CPL 3 → 0) loads RSP0 from the TSS; an IST index
        // selects an IST stack. For the kernel's own interrupts (CPL 0) the stack
        // is unchanged unless an IST is set.
        if old_cpl != 0 || ist != 0 {
            let rsp0 = self.tss_stack(ist);
            if rsp0 != 0 {
                self.r[RSP] = rsp0;
            }
            self.cpl = 0;
            self.seg[SegId::Cs as usize].selector = 0x10;
            self.seg[SegId::Cs as usize].long = true;
            self.seg[SegId::Ss as usize].selector = 0x18;
        }
        let rflags = self.rflags;
        self.push(u64::from(old_ss));
        self.push(old_rsp);
        self.push(rflags);
        self.push(u64::from(old_cs));
        self.push(self.rip);
        if has_error {
            self.push(error);
        }
        self.rflags &= !RFLAGS_IF; // an interrupt gate clears IF
        self.rip = off;
    }

    /// The kernel stack for a ring transition: `RSP0` (IST 0) or the indexed IST
    /// entry from the TSS the kernel loaded with `LTR`.
    fn tss_stack(&self, ist: u64) -> u64 {
        let base = self.sys().tr_base;
        if base == 0 {
            return 0;
        }
        if ist == 0 {
            self.rd_virt(base + 4, 8) // RSP0 at TSS offset 4
        } else {
            self.rd_virt(base + 0x24 + (ist - 1) * 8, 8) // IST1.. at offset 0x24
        }
    }

    /// Deliver a CPU exception (a fault/abort) through the IDT — `#GP`, `#PF`
    /// (with `CR2` already set), `#UD`, etc. Mirrors the AArch64 core taking an
    /// EL1 exception instead of halting the flat core.
    fn raise_exception(&mut self, vector: u8, error: u64, has_error: bool) {
        if self.sys.as_ref().is_some_and(|s| s.idtr.1 != 0) {
            self.deliver_interrupt(vector, error, has_error);
        }
    }

    // ── the instruction tail the boot path hits (#12) ──────────────────────────

    /// Execute a string instruction (`MOVS`/`STOS`/`LODS`/`SCAS`/`CMPS`) of
    /// element size `osz`, honouring a `REP`/`REPE`/`REPNE` prefix and the
    /// direction flag. `RSI`/`RDI` advance by ±`osz`; `RCX` counts the repeats.
    fn string_op(&mut self, kind: StringOp, osz: u8, rep: RepKind, start: u64) {
        let step: i64 = if self.rflags & RFLAGS_DF != 0 {
            -i64::from(osz)
        } else {
            i64::from(osz)
        };
        let mut count = if rep == RepKind::None { 1 } else { self.r[RCX] };
        // A `REP` is interruptible/restartable: the hardware checks for a pending
        // interrupt between iterations, leaving RCX/RSI/RDI at the partial position
        // with RIP on the prefix so it resumes. We bound the iterations executed per
        // `step()` and, if the count is not exhausted, rewind RIP to `start` so the
        // instruction re-executes — both yielding to interrupts/the run budget and
        // preventing a pathologically large RCX from wedging the emulator in one step.
        let budget: u64 = 1 << 16;
        let mut done: u64 = 0;
        while count != 0 {
            if rep != RepKind::None && done >= budget {
                self.r[RCX] = count;
                self.rip = start;
                return;
            }
            done += 1;
            match kind {
                StringOp::Movs => {
                    let v = self.rd(self.r[RSI], osz);
                    if self.fault.is_some() {
                        break;
                    }
                    self.wr(self.r[RDI], osz, v);
                    if self.fault.is_some() {
                        break;
                    }
                    self.r[RSI] = self.r[RSI].wrapping_add(step as u64);
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
                StringOp::Stos => {
                    self.wr(self.r[RDI], osz, self.r[RAX] & Self::mask(osz));
                    if self.fault.is_some() {
                        break;
                    }
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
                StringOp::Lods => {
                    let v = self.rd(self.r[RSI], osz);
                    if self.fault.is_some() {
                        break;
                    }
                    self.store_rm(Rm::Reg(RAX), osz, v);
                    self.r[RSI] = self.r[RSI].wrapping_add(step as u64);
                }
                StringOp::Scas => {
                    let a = self.r[RAX] & Self::mask(osz);
                    let b = self.rd(self.r[RDI], osz);
                    if self.fault.is_some() {
                        break;
                    }
                    let r = a.wrapping_sub(b);
                    self.flags_arith(a, b, r, osz, true);
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
                StringOp::Cmps => {
                    let a = self.rd(self.r[RSI], osz);
                    if self.fault.is_some() {
                        break;
                    }
                    let b = self.rd(self.r[RDI], osz);
                    if self.fault.is_some() {
                        break;
                    }
                    let r = a.wrapping_sub(b);
                    self.flags_arith(a, b, r, osz, true);
                    self.r[RSI] = self.r[RSI].wrapping_add(step as u64);
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
            }
            if rep != RepKind::None {
                count -= 1;
                self.r[RCX] = count;
                // REPE/REPNE on SCAS/CMPS also terminate on the ZF condition.
                if matches!(kind, StringOp::Scas | StringOp::Cmps) {
                    let zf = self.rflags & flag::ZF != 0;
                    if (rep == RepKind::Rep && !zf) || (rep == RepKind::Repne && zf) {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    /// Execute a shift/rotate (`ROL`/`ROR`/`RCL`/`RCR`/`SHL`/`SHR`/`SAR`) on a
    /// decoded operand, masking the count to 6 bits (5 for sub-64-bit) and setting
    /// the carry/zero/sign flags as the architecture defines.
    fn shift_rotate(&mut self, ext: u8, rm: Rm, size: u8, cnt: u8) {
        let bits = u32::from(size) * 8;
        let cnt = u32::from(cnt) & if size == 8 { 63 } else { 31 };
        if cnt == 0 {
            return;
        }
        let m = Self::mask(size);
        let a = self.load_rm(rm, size) & m;
        let (res, cf) = match ext {
            0 => {
                // ROL
                let r = ((a << cnt) | (a >> (bits - cnt))) & m;
                (r, r & 1)
            }
            1 => {
                // ROR
                let r = ((a >> cnt) | (a << (bits - cnt))) & m;
                (r, (r >> (bits - 1)) & 1)
            }
            4 | 6 => {
                // SHL / SAL
                let r = (a << cnt) & m;
                (r, (a >> (bits - cnt)) & 1)
            }
            5 => {
                // SHR
                let r = a >> cnt;
                (r, (a >> (cnt - 1)) & 1)
            }
            7 => {
                // SAR (arithmetic)
                let sa = (a as i64) << (64 - bits) >> (64 - bits); // sign-extend
                let r = ((sa >> cnt) as u64) & m;
                (r, (a >> (cnt - 1)) & 1)
            }
            2 | 3 => {
                // RCL / RCR — a true rotate of `a` THROUGH CF, i.e. a (bits+1)-bit
                // rotation (the carry is the extra bit). The count reduces mod
                // (bits+1) (mod 9/17 for 8/16-bit; 32/64-bit counts are already < the
                // width from the 5/6-bit mask above). Used by multi-precision
                // carry propagation — the ROL/ROR approximation gave wrong results.
                let width = bits + 1;
                let n = match size {
                    1 => cnt % 9,
                    2 => cnt % 17,
                    _ => cnt % width,
                };
                if n == 0 {
                    (a, u64::from(self.rflags & flag::CF != 0))
                } else {
                    let cf_in = u128::from(self.rflags & flag::CF != 0);
                    let v = (cf_in << bits) | u128::from(a);
                    let fm = (1u128 << width) - 1;
                    let rot = if ext == 2 {
                        (v.wrapping_shl(n) | v.wrapping_shr(width - n)) & fm
                    } else {
                        (v.wrapping_shr(n) | v.wrapping_shl(width - n)) & fm
                    };
                    ((rot as u64) & m, u64::from((rot >> bits) & 1 != 0))
                }
            }
            _ => (a, 0),
        };
        self.store_rm(rm, size, res);
        self.set(flag::CF, cf != 0);
        if ext >= 4 {
            // The logical/arithmetic shifts update SF/ZF/PF.
            self.set(flag::ZF, res == 0);
            self.set(flag::SF, Self::sign(res, size));
            self.set(flag::PF, (res as u8).count_ones().is_multiple_of(2));
        }
        // OF is architecturally defined only for a 1-bit shift/rotate (undefined,
        // hence left unmodified, for counts > 1). It reflects whether the sign bit
        // changed: for SHL/RCL/ROL it is msb(result) XOR CF-out; for SHR it is the
        // original msb; for SAR it is 0; for ROR/RCR it is the XOR of the result's
        // top two bits.
        if cnt == 1 {
            let msb = Self::sign(res, size);
            let of = match ext {
                4 | 6 => msb ^ (cf != 0),                        // SHL/SAL
                5 => Self::sign(a, size),                        // SHR
                7 => false,                                      // SAR
                0 | 2 => msb ^ (cf != 0),                        // ROL / RCL
                1 | 3 => msb ^ (((res >> (bits - 2)) & 1) != 0), // ROR / RCR
                _ => false,
            };
            self.set(flag::OF, of);
        }
    }

    /// `SHLD`/`SHRD` — a double-precision shift of `dst` with bits shifted in from
    /// `src`.
    fn shld_shrd(&mut self, rm: Rm, reg: usize, size: u8, cnt: u8, right: bool) {
        let bits = u32::from(size) * 8;
        let cnt = u32::from(cnt) & if size == 8 { 63 } else { 31 };
        if cnt == 0 {
            return;
        }
        let m = Self::mask(size);
        let dst = self.load_rm(rm, size) & m;
        let src = self.r[reg] & m;
        let res = if right {
            ((dst >> cnt) | (src << (bits - cnt))) & m
        } else {
            ((dst << cnt) | (src >> (bits - cnt))) & m
        };
        // CF = the last bit shifted out of `dst`.
        let cf = if right {
            (dst >> (cnt - 1)) & 1
        } else {
            (dst >> (bits - cnt)) & 1
        };
        self.store_rm(rm, size, res);
        self.set(flag::CF, cf != 0);
        self.set(flag::ZF, res == 0);
        self.set(flag::SF, Self::sign(res, size));
        self.set(flag::PF, (res as u8).count_ones().is_multiple_of(2));
        // OF (1-bit shifts only): set when the sign bit changed.
        if cnt == 1 {
            self.set(flag::OF, Self::sign(res, size) != Self::sign(dst, size));
        }
    }

    /// Group 3 (`0xf6`/`0xf7`): `TEST`/`NOT`/`NEG`/`MUL`/`IMUL`/`DIV`/`IDIV`.
    fn group3(&mut self, rex: u8, size: u8, start: u64) -> Result<(), Halt> {
        let (ext, rm) = self.modrm(rex);
        let m = Self::mask(size);
        match ext & 7 {
            0 | 1 => {
                // TEST r/m, imm. The immediate is fetched first so a RIP-relative
                // `rm` resolves against the instruction-end `rip`.
                let imm = if size == 1 {
                    self.fetch(1)
                } else {
                    self.fetch_imm_z(size)
                };
                let a = self.load_rm(rm, size);
                self.flags_logic(a & imm, size);
            }
            2 => {
                // NOT (no flags).
                let a = self.load_rm(rm, size);
                self.store_rm(rm, size, !a & m);
            }
            3 => {
                // NEG.
                let a = self.load_rm(rm, size) & m;
                let r = a.wrapping_neg() & m;
                self.flags_arith(0, a, r, size, true);
                self.set(flag::CF, a != 0);
                self.store_rm(rm, size, r);
            }
            4 => {
                // MUL (unsigned): rdx:rax = rax * r/m.
                let a = self.r[RAX] & m;
                let b = self.load_rm(rm, size) & m;
                let prod = u128::from(a) * u128::from(b);
                self.store_mul_result(prod, size);
            }
            5 => {
                // IMUL (signed): rdx:rax = rax * r/m.
                let a = sign_extend(self.r[RAX] & m, size) as i128;
                let b = sign_extend(self.load_rm(rm, size) & m, size) as i128;
                let prod = a * b;
                self.store_imul_result(prod, size);
            }
            6 => {
                // DIV (unsigned).
                let divisor = self.load_rm(rm, size) & m;
                if divisor == 0 {
                    self.raise_exception(0, 0, false); // #DE
                    return Ok(());
                }
                let dividend = self.dividend(size);
                let q = dividend / u128::from(divisor);
                let r = dividend % u128::from(divisor);
                // #DE on quotient overflow — the quotient must fit the operand width
                // (else AL/AX/EAX/RAX cannot hold it). The architecture faults here
                // exactly as for divide-by-zero.
                if q > u128::from(m) {
                    self.raise_exception(0, 0, false);
                    return Ok(());
                }
                self.store_div_result(q as u64, r as u64, size);
            }
            7 => {
                // IDIV (signed).
                let divisor = sign_extend(self.load_rm(rm, size) & m, size);
                if divisor == 0 {
                    self.raise_exception(0, 0, false);
                    return Ok(());
                }
                let dividend = self.dividend_signed(size);
                let q = dividend / i128::from(divisor);
                let r = dividend % i128::from(divisor);
                // #DE on signed quotient overflow (includes INT_MIN / -1): the
                // quotient must fit `[-2^(n-1), 2^(n-1)-1]`.
                let bits = u32::from(size) * 8;
                let lo = -(1i128 << (bits - 1));
                let hi = (1i128 << (bits - 1)) - 1;
                if q < lo || q > hi {
                    self.raise_exception(0, 0, false);
                    return Ok(());
                }
                self.store_div_result(q as u64, r as u64, size);
            }
            _ => return Err(Halt::Undefined(start)),
        }
        Ok(())
    }

    fn store_mul_result(&mut self, prod: u128, size: u8) {
        let m = Self::mask(size);
        if size == 1 {
            self.r[RAX] = (self.r[RAX] & !0xffff) | (prod as u64 & 0xffff);
        } else {
            self.store_rm(Rm::Reg(RAX), size, prod as u64 & m);
            self.store_rm(
                Rm::Reg(RDX),
                size,
                (prod >> (u32::from(size) * 8)) as u64 & m,
            );
        }
        let hi = prod >> (u32::from(size) * 8);
        let overflow = hi != 0;
        self.set(flag::CF, overflow);
        self.set(flag::OF, overflow);
    }

    /// One-operand `IMUL` (`RDX:RAX = RAX * r/m`, signed). CF=OF=1 iff the full
    /// signed product does not fit the low operand width (i.e. the high half is not
    /// merely the sign extension of the low half) — unlike unsigned `MUL`, which
    /// keys overflow on a non-zero high half.
    fn store_imul_result(&mut self, prod: i128, size: u8) {
        let m = Self::mask(size);
        let bits = u32::from(size) * 8;
        let pu = prod as u128;
        if size == 1 {
            self.r[RAX] = (self.r[RAX] & !0xffff) | (pu as u64 & 0xffff);
        } else {
            self.store_rm(Rm::Reg(RAX), size, pu as u64 & m);
            self.store_rm(Rm::Reg(RDX), size, (pu >> bits) as u64 & m);
        }
        let overflow = i128::from(sign_extend(pu as u64 & m, size)) != prod;
        self.set(flag::CF, overflow);
        self.set(flag::OF, overflow);
    }

    /// CF=OF for the two/three-operand `IMUL` forms: set iff the truncated result
    /// (`result`, the low operand-width bits stored back) differs from the full
    /// signed product `full` — i.e. the product overflowed the destination. The
    /// other arithmetic flags are architecturally undefined and left unchanged.
    fn set_imul_flags(&mut self, full: i128, result: u64, size: u8) {
        let overflow = i128::from(sign_extend(result & Self::mask(size), size)) != full;
        self.set(flag::CF, overflow);
        self.set(flag::OF, overflow);
    }

    fn dividend(&self, size: u8) -> u128 {
        let m = Self::mask(size);
        if size == 1 {
            u128::from(self.r[RAX] & 0xffff)
        } else {
            (u128::from(self.r[RDX] & m) << (u32::from(size) * 8)) | u128::from(self.r[RAX] & m)
        }
    }

    fn dividend_signed(&self, size: u8) -> i128 {
        if size == 1 {
            i128::from(self.r[RAX] as i16)
        } else {
            let m = Self::mask(size);
            let hi = sign_extend(self.r[RDX] & m, size) as i128;
            let lo = (self.r[RAX] & m) as i128;
            (hi << (u32::from(size) * 8)) | lo
        }
    }

    fn store_div_result(&mut self, q: u64, r: u64, size: u8) {
        if size == 1 {
            self.r[RAX] = (self.r[RAX] & !0xffff) | (q & 0xff) | ((r & 0xff) << 8);
        } else {
            self.store_rm(Rm::Reg(RAX), size, q & Self::mask(size));
            self.store_rm(Rm::Reg(RDX), size, r & Self::mask(size));
        }
    }

    /// `BT`/`BTS`/`BTR`/`BTC` — test (and optionally set/reset/complement) bit
    /// `bit` of the operand; the prior bit value goes to `CF`. `op` selects the
    /// variant (0=BT, 1=BTS, 2=BTR, 3=BTC).
    fn bit_test(&mut self, rm: Rm, size: u8, bit: u64, op: u8) {
        let nbits = u64::from(size) * 8;
        // For a memory operand the bit index addresses beyond the operand; for a
        // register it wraps to the operand width.
        match rm {
            Rm::Reg(_) => {
                let b = bit % nbits;
                let v = self.load_rm(rm, size);
                self.set(flag::CF, (v >> b) & 1 != 0);
                let nv = match op {
                    1 => v | (1 << b),
                    2 => v & !(1 << b),
                    3 => v ^ (1 << b),
                    _ => v,
                };
                if op != 0 {
                    self.store_rm(rm, size, nv);
                }
            }
            _ => {
                let base = self.rm_addr(rm).unwrap();
                let byte = base.wrapping_add(bit / 8);
                let b = bit % 8;
                let v = self.rd(byte, 1);
                self.set(flag::CF, (v >> b) & 1 != 0);
                let nv = match op {
                    1 => v | (1 << b),
                    2 => v & !(1 << b),
                    3 => v ^ (1 << b),
                    _ => v,
                };
                if op != 0 {
                    self.wr(byte, 1, nv);
                }
            }
        }
    }

    /// `CMPXCHG`: compare `RAX` with the destination; if equal, store the source,
    /// else load the destination into `RAX`. Sets `ZF` on success.
    fn cmpxchg(&mut self, rex: u8, size: u8) {
        let (reg, rm) = self.modrm(rex);
        let dst = self.load_rm(rm, size) & Self::mask(size);
        let acc = self.r[RAX] & Self::mask(size);
        let r = acc.wrapping_sub(dst);
        self.flags_arith(acc, dst, r, size, true);
        if acc == dst {
            let src = self.r[reg] & Self::mask(size);
            self.store_rm(rm, size, src);
        } else {
            self.store_rm(Rm::Reg(RAX), size, dst);
        }
    }

    /// `CMPXCHG8B`/`CMPXCHG16B`: compare `EDX:EAX`/`RDX:RAX` with the 8/16-byte
    /// destination; swap in `ECX:EBX`/`RCX:RBX` on a match. Sets `ZF`.
    fn cmpxchg16b(&mut self, rm: Rm, wide: bool) {
        let Some(addr) = self.rm_addr(rm) else {
            return;
        };
        if wide {
            let lo = self.rd(addr, 8);
            let hi = self.rd(addr + 8, 8);
            if lo == self.r[RAX] && hi == self.r[RDX] {
                self.wr(addr, 8, self.r[RBX]); // RBX:RCX swapped in (RBX = low)
                self.wr(addr + 8, 8, self.r[RCX]);
                self.set(flag::ZF, true);
            } else {
                self.r[RAX] = lo;
                self.r[RDX] = hi;
                self.set(flag::ZF, false);
            }
        } else {
            let lo = self.rd(addr, 4);
            let hi = self.rd(addr + 4, 4);
            if lo == (self.r[RAX] & 0xffff_ffff) && hi == (self.r[RDX] & 0xffff_ffff) {
                self.wr(addr, 4, self.r[RBX] & 0xffff_ffff); // ECX:EBX swapped in (EBX = low)
                self.wr(addr + 4, 4, self.r[RCX] & 0xffff_ffff);
                self.set(flag::ZF, true);
            } else {
                self.r[RAX] = lo;
                self.r[RDX] = hi;
                self.set(flag::ZF, false);
            }
        }
    }

    /// `RDRAND`/`RDSEED` (`0F C7 /6`, `/7`): write a fresh non-constant value to
    /// the destination register (size per the operand) and report success
    /// (`CF=1`, other arithmetic flags cleared). A deterministic SplitMix64
    /// stream, perturbed by the TSC, suffices for a boot witness — it only has
    /// to vary so the kernel's crng accepts hardware entropy instead of
    /// spinning on jitterentropy.
    fn rdrand(&mut self, reg: usize, size: u8) {
        let s = self.sys_mut();
        s.rng = s.rng.wrapping_add(0x9e37_79b9_7f4a_7c15 ^ s.tsc);
        let mut z = s.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        // 16-bit RDRAND zero-extends like an 8/16-bit GPR write; 32/64-bit
        // follow the usual write semantics (store_rm masks per `size`).
        self.store_rm(Rm::Reg(reg), size, z);
        self.set(flag::CF, true);
        self.set(flag::OF, false);
        self.set(flag::SF, false);
        self.set(flag::ZF, false);
        self.set(flag::PF, false);
        // AF (bit 4) — not in the `flag` module; clear it directly.
        self.rflags &= !(1u64 << 4);
    }

    /// The architected timestamp counter as `RDTSC`/`RDTSCP` observe it: the
    /// monotonic platform `tsc`, advancing at the fixed calibrated rate
    /// (the TSC:PIT lockstep in `sys_tick`). Monotonic and jitter-free.
    fn read_tsc(&self) -> u64 {
        self.sys().tsc
    }

    /// `XADD`: exchange-and-add — the destination and source swap, then their sum
    /// is stored to the destination, setting the arithmetic flags.
    fn xadd(&mut self, rex: u8, size: u8) {
        let (reg, rm) = self.modrm(rex);
        let dst = self.load_rm(rm, size) & Self::mask(size);
        let src = self.r[reg] & Self::mask(size);
        let sum = dst.wrapping_add(src);
        self.flags_arith(dst, src, sum, size, false);
        self.store_rm(Rm::Reg(reg), size, dst);
        self.store_rm(rm, size, sum & Self::mask(size));
    }

    /// `CPUID` — report the minimal feature set the kernel's early boot checks
    /// require: long mode, the standard feature bits, and a vendor string.
    fn cpuid(&mut self) {
        let leaf = (self.r[RAX] & 0xffff_ffff) as u32;
        let (a, b, c, d): (u32, u32, u32, u32) = match leaf {
            0 => (0x10, 0x6874_7541, 0x444d_4163, 0x6974_6e65), // max leaf, "AuthenticAMD"
            1 => {
                // Family/model in EAX; EDX features: FPU,TSC,MSR,PAE,CX8,APIC,
                // SEP,MTRR,PGE,CMOV,PAT,PSE36,CLFSH,MMX,FXSR,SSE,SSE2.
                // ECX bit 30 = RDRAND — the core's hardware RNG (the kernel's
                // crng/jitterentropy path needs it to avoid spinning on the
                // deterministic TSC entropy source). ECX bit 17 = PCID, so the
                // kernel tags address spaces and `CR3` switches (its ASID reuse and
                // text_poke's poking-mm) need not flush the (software) TLB.
                //
                // EBX[15:8] = CLFLUSH line size in 8-byte units. The kernel computes
                // `x86_clflush_size = field * 8` and steps a range by it in
                // `clflush_cache_range` (cpa_flush / set_memory_*); a zero there makes
                // that loop never advance — an infinite hang. Report 8 (→ the 64-byte
                // line every x86-64 has). The CLFSH feature is advertised in EDX b19.
                (0x0000_0600, 0x0000_0800, (1 << 30) | (1 << 17), 0x078b_fbff)
            }
            // Leaf 7 sub-leaf 0: EBX bit 18 = RDSEED (the seeding RNG the kernel
            // pairs with RDRAND). EDX bit 29 = ARCH_CAPABILITIES — the core exposes
            // `IA32_ARCH_CAPABILITIES` (MSR 0x10A) so the kernel reads its truthful
            // security properties: this deterministic emulator has no speculative
            // execution, so it is immune to Meltdown/MDS/SSB/etc. and the kernel must
            // not enable the (costly, and here un-needed) PTI/MDS mitigations — which
            // also avoids the KPTI user-page-table W+X walk. Matches the qemu oracle,
            // whose boot likewise runs without PTI.
            7 => (0, 1 << 18, 0, 1 << 29),
            0x8000_0000 => (0x8000_0008, 0x6874_7541, 0x444d_4163, 0x6974_6e65),
            0x8000_0001 => {
                // EDX: SYSCALL (bit 11), NX (bit 20), Long Mode (bit 29), …
                (0, 0, 0x0000_0001, 0x2010_0800)
            }
            0x8000_0002..=0x8000_0004 => (0x6f6c_6f48, 0x6170_7373, 0x2d65_7363, 0x3436_7858),
            0x8000_0008 => (0x0000_3028, 0, 0, 0), // 48-bit virt, 40-bit phys
            _ => (0, 0, 0, 0),
        };
        self.r[RAX] = u64::from(a);
        self.r[RBX] = u64::from(b); // EBX — register index 3, NOT 1 (which is RCX)
        self.r[RCX] = u64::from(c);
        self.r[RDX] = u64::from(d);
    }

    /// Read a model-specific register the boot path uses.
    fn rdmsr(&self, ecx: u32) -> u64 {
        match ecx {
            MSR_EFER => self.efer,
            MSR_FS_BASE => self.seg[SegId::Fs as usize].base,
            MSR_GS_BASE => self.seg[SegId::Gs as usize].base,
            MSR_APIC_BASE => LAPIC_BASE | 0x900, // enabled, BSP
            // IA32_ARCH_CAPABILITIES — the core's truthful security posture. With no
            // speculative execution it is immune to the side-channel classes, so it
            // reports RDCL_NO (Meltdown), IBRS_ALL, SSB_NO, MDS_NO, IF_PSCHANGE_MC_NO,
            // TAA_NO, MMIO_STALE_DATA_NO, FB_CLEAR. RDCL_NO is what makes the kernel
            // skip PTI (and so the KPTI user-page-table W+X walk).
            MSR_IA32_ARCH_CAPABILITIES => {
                RDCL_NO | IBRS_ALL | SSB_NO | MDS_NO | IF_PSCHANGE_MC_NO | TAA_NO
            }
            _ => self.sys().msr.get(&ecx).copied().unwrap_or(0),
        }
    }

    /// Write a model-specific register, applying the architectural side effects
    /// (`EFER` long-mode bits, the `FS`/`GS` bases).
    fn wrmsr(&mut self, ecx: u32, val: u64) {
        match ecx {
            MSR_EFER => self.efer = val,
            MSR_FS_BASE => self.seg[SegId::Fs as usize].base = val,
            MSR_GS_BASE => self.seg[SegId::Gs as usize].base = val,
            _ => {
                self.sys_mut().msr.insert(ecx, val);
            }
        }
    }

    /// `SWAPGS` — exchange `GS.base` with the `KERNEL_GS_BASE` MSR (the kernel's
    /// per-CPU base swap on a ring transition).
    fn swapgs(&mut self) {
        let cur = self.seg[SegId::Gs as usize].base;
        let kern = self
            .sys()
            .msr
            .get(&MSR_KERNEL_GS_BASE)
            .copied()
            .unwrap_or(0);
        self.seg[SegId::Gs as usize].base = kern;
        self.sys_mut().msr.insert(MSR_KERNEL_GS_BASE, cur);
    }

    /// Group 7 (`0F 01`): `LGDT`/`LIDT`/`SGDT`/`SIDT`/`LMSW`/`SWAPGS`/`RDTSCP`.
    fn group7(&mut self, rex: u8, _start: u64) -> Result<(), Halt> {
        // Peek the ModRM to distinguish the memory forms (LGDT/LIDT, mod != 3)
        // from the register-encoded ones (SWAPGS = 0F 01 F8, etc.).
        let modrm = *self
            .ram
            .get(self.translate(self.rip) as usize)
            .unwrap_or(&0);
        let md = modrm >> 6;
        let ext = (modrm >> 3) & 7;
        if md == 3 {
            // Register forms keyed by the full ModRM byte.
            self.rip = self.rip.wrapping_add(1);
            match modrm {
                0xf8 => self.swapgs(),
                0xf9 => {
                    // RDTSCP — like RDTSC plus the CPU id in ECX.
                    let tsc = self.read_tsc();
                    self.r[RAX] = tsc & 0xffff_ffff;
                    self.r[RDX] = tsc >> 32;
                    self.r[RCX] = 0;
                }
                // CLAC / STAC — clear/set RFLAGS.AC (the SMAP override the kernel
                // wraps every deliberate user access in). Without these the kernel's
                // `copy_*_user`/`clear_user` (e.g. `padzero` zeroing a BSS tail
                // during `execve`) runs with AC=0, and its SMAP check faults the
                // supervisor access to the user page — killing the first userspace
                // binary. (`0F 01 CA` = CLAC, `0F 01 CB` = STAC.)
                0xca => self.rflags &= !RFLAGS_AC,
                0xcb => self.rflags |= RFLAGS_AC,
                _ => {} // MONITOR/MWAIT/etc. — no boot-path effect
            }
            return Ok(());
        }
        let (_e, rm) = self.modrm(rex);
        let addr = self.rm_addr(rm).unwrap_or(0);
        match ext {
            2 => {
                // LGDT: limit (u16) then base (u64).
                let limit = self.rd(addr, 2) as u16;
                let base = self.rd(addr + 2, 8);
                self.sys_mut().gdtr = (base, limit);
            }
            3 => {
                // LIDT.
                let limit = self.rd(addr, 2) as u16;
                let base = self.rd(addr + 2, 8);
                self.sys_mut().idtr = (base, limit);
            }
            0 => {
                // SGDT.
                let (base, limit) = self.sys().gdtr;
                self.wr(addr, 2, u64::from(limit));
                self.wr(addr + 2, 8, base);
            }
            1 => {
                // SIDT.
                let (base, limit) = self.sys().idtr;
                self.wr(addr, 2, u64::from(limit));
                self.wr(addr + 2, 8, base);
            }
            7 => {
                // INVLPG — invalidate the TLB entry for *this page only*. A precise
                // single-page invalidation (not a whole-PCID flush) keeps the rest of
                // the address space warm; the kernel issues INVLPG on every COW/unmap,
                // so over-flushing here cold-flushes the TLB constantly and makes a
                // fork+exec-heavy userspace boot ~80× slower. (Stale-permission
                // coherence is handled on the fault edge by `translate_acc`'s re-walk.)
                let page = addr & !0xfff;
                let set = (page >> 12) as usize & (TLB_SETS - 1);
                self.tlb[set].valid = false;
            }
            _ => {}
        }
        Ok(())
    }

    // ── SSE / SSE2 (the x86-64 baseline vector ISA) ──────────────────────────

    /// Read a 128-bit value from an SSE r/m operand — an `XMM` register or 16
    /// bytes of memory (little-endian: low qword first). A `#PF` on the memory
    /// form is latched and the instruction restarts (`reg_snapshot` includes the
    /// XMM file).
    fn xmm_load_rm(&mut self, rm: Rm) -> u128 {
        match rm {
            Rm::Reg(i) => self.xmm[i],
            _ => {
                let a = self.rm_addr(rm).unwrap_or(0);
                let lo = u128::from(self.rd(a, 8));
                let hi = u128::from(self.rd(a.wrapping_add(8), 8));
                lo | (hi << 64)
            }
        }
    }

    /// Write a 128-bit value to an SSE r/m operand (XMM register or 16 bytes).
    fn xmm_store_rm(&mut self, rm: Rm, val: u128) {
        match rm {
            Rm::Reg(i) => self.xmm[i] = val,
            _ => {
                let a = self.rm_addr(rm).unwrap_or(0);
                self.wr(a, 8, val as u64);
                self.wr(a.wrapping_add(8), 8, (val >> 64) as u64);
            }
        }
    }

    /// Read up to 64 bits from an SSE r/m operand's *low* lanes (an XMM register's
    /// low bits, or `size` bytes of memory) — for `MOVSS`/`MOVSD`/`MOVQ`/`MOVD`.
    fn xmm_load_rm_lo(&mut self, rm: Rm, size: u8) -> u64 {
        match rm {
            Rm::Reg(i) => (self.xmm[i] as u64) & Self::mask(size),
            _ => {
                let a = self.rm_addr(rm).unwrap_or(0);
                self.rd(a, size)
            }
        }
    }

    /// Execute an SSE/SSE2 instruction `0F <op2>` whose mandatory prefix is given
    /// by (`p66` = `66`, the `rep` kind = `F3`/`F2`, or none). These are the
    /// integer + data-movement vector instructions a stock `linux-amd64` glibc
    /// binary uses (its IFUNC string/memory routines, and `movd`/`movq`/`movdqa`
    /// from process startup). The XMM register file is [`Cpu::xmm`]. Returns
    /// [`Halt::Undefined`] for an opcode not modelled.
    #[allow(clippy::too_many_lines)]
    fn sse_0f(
        &mut self,
        op2: u8,
        rex: u8,
        p66: bool,
        rep: RepKind,
        start: u64,
    ) -> Result<(), Halt> {
        let f3 = rep == RepKind::Rep;
        let f2 = rep == RepKind::Repne;
        match op2 {
            // Multi-byte NOP / PREFETCH*/HINT_NOP (0F 18..0F 1F): consume the ModRM.
            0x18..=0x1f => {
                let _ = self.modrm(rex);
            }
            // Group 15 (0F AE): LFENCE/MFENCE/SFENCE (register form, no-ops on this
            // in-order core), and the memory forms FXSAVE/FXRSTOR/LDMXCSR/STMXCSR.
            0xae => {
                let modrm_peek = *self
                    .ram
                    .get(self.translate(self.rip) as usize)
                    .unwrap_or(&0);
                if modrm_peek >> 6 == 3 {
                    let _ = self.modrm(rex); // fence (register form) — no-op
                } else {
                    let (ext, rm) = self.modrm(rex);
                    let addr = self.rm_addr(rm).unwrap_or(0);
                    match ext & 7 {
                        0 => {
                            // FXSAVE — write the 512-byte FPU/SSE save area. We model
                            // the SSE state (16 XMM + MXCSR); the x87 region is a sane
                            // default (this guest's userspace is SSE, not x87). Saving
                            // the XMM state is REQUIRED: the kernel FXSAVE/FXRSTORs it
                            // around a context switch, so without it a preempted SSE
                            // task's XMM registers are silently corrupted.
                            self.wr(addr, 8, 0x0000_0000_0000_037f); // FCW (+FSW/FTW=0)
                            self.wr(addr + 24, 8, 0xffff_0000_0000_1f80); // MXCSR|MASK
                            for i in 4..20u64 {
                                self.wr(addr + i * 8, 8, 0); // x87 ST/MM region
                            }
                            for i in 0..16u64 {
                                let x = self.xmm[i as usize];
                                self.wr(addr + 160 + i * 16, 8, x as u64);
                                self.wr(addr + 168 + i * 16, 8, (x >> 64) as u64);
                            }
                        }
                        1 => {
                            // FXRSTOR — reload the 16 XMM registers the kernel saved.
                            for i in 0..16u64 {
                                let lo = self.rd(addr + 160 + i * 16, 8);
                                let hi = self.rd(addr + 168 + i * 16, 8);
                                self.xmm[i as usize] = u128::from(lo) | (u128::from(hi) << 64);
                            }
                        }
                        2 => {} // LDMXCSR — accepted (a fixed default MXCSR is modelled)
                        3 => self.xmm_store_lo32(rm, 0x1f80), // STMXCSR
                        _ => {} // fences (memory form)
                    }
                }
            }
            0x77 => {} // EMMS — no MMX state to clear
            // MOVUPS/MOVUPD/MOVSS/MOVSD — reg ← r/m.
            0x10 => {
                let (reg, rm) = self.modrm(rex);
                if f3 {
                    // MOVSS: reg←mem zero-extends to 128; reg←reg keeps [127:32].
                    let v = self.xmm_load_rm_lo(rm, 4);
                    self.xmm[reg] = if matches!(rm, Rm::Reg(_)) {
                        (self.xmm[reg] & !0xffff_ffff) | u128::from(v)
                    } else {
                        u128::from(v)
                    };
                } else if f2 {
                    let v = self.xmm_load_rm_lo(rm, 8);
                    self.xmm[reg] = if matches!(rm, Rm::Reg(_)) {
                        (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(v)
                    } else {
                        u128::from(v)
                    };
                } else {
                    self.xmm[reg] = self.xmm_load_rm(rm); // MOVUPS/MOVUPD
                }
            }
            // store form — r/m ← reg.
            0x11 => {
                let (reg, rm) = self.modrm(rex);
                if f3 {
                    self.xmm_store_lo(rm, self.xmm[reg] as u64, 4);
                } else if f2 {
                    self.xmm_store_lo(rm, self.xmm[reg] as u64, 8);
                } else {
                    self.xmm_store_rm(rm, self.xmm[reg]);
                }
            }
            // MOVLPS/MOVLPD (m64→low) or MOVHLPS (reg: src high→dst low).
            0x12 => {
                let (reg, rm) = self.modrm(rex);
                let lo = if matches!(rm, Rm::Reg(_)) {
                    (self.xmm_load_rm(rm) >> 64) as u64 // MOVHLPS
                } else {
                    self.xmm_load_rm_lo(rm, 8) // MOVLPS/MOVLPD
                };
                self.xmm[reg] = (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(lo);
            }
            0x13 => {
                let (reg, rm) = self.modrm(rex); // MOVLPS store low64
                self.xmm_store_lo(rm, self.xmm[reg] as u64, 8);
            }
            // MOVHPS/MOVHPD (m64→high) or MOVLHPS (reg: src low→dst high).
            0x16 => {
                let (reg, rm) = self.modrm(rex);
                let hi = if matches!(rm, Rm::Reg(_)) {
                    self.xmm_load_rm(rm) as u64 // MOVLHPS
                } else {
                    self.xmm_load_rm_lo(rm, 8) // MOVHPS/MOVHPD
                };
                self.xmm[reg] = (self.xmm[reg] & u128::from(u64::MAX)) | (u128::from(hi) << 64);
            }
            0x17 => {
                let (reg, rm) = self.modrm(rex); // MOVHPS store high64
                self.xmm_store_lo(rm, (self.xmm[reg] >> 64) as u64, 8);
            }
            0x14 => {
                let (reg, rm) = self.modrm(rex); // UNPCKLPS/PD
                let s = self.xmm_load_rm(rm);
                self.xmm[reg] = if p66 {
                    unpckl_qwords(self.xmm[reg], s)
                } else {
                    unpckl_dwords(self.xmm[reg], s)
                };
            }
            0x15 => {
                let (reg, rm) = self.modrm(rex); // UNPCKHPS/PD
                let s = self.xmm_load_rm(rm);
                self.xmm[reg] = if p66 {
                    unpckh_qwords(self.xmm[reg], s)
                } else {
                    unpckh_dwords(self.xmm[reg], s)
                };
            }
            // Packed-FLOAT bitwise logicals — 128-bit (the 66 prefix only selects the
            // PS/PD mnemonic; the result is identical). `xorps %xmm,%xmm` (register
            // zeroing) is pervasive in compiled code + glibc. The packed-INT forms
            // (PAND/PANDN/POR/PXOR, DB/DF/EB/EF) are handled by `sse_bin` below.
            //   54 ANDPS/ANDPD · 55 ANDNPS/ANDNPD · 56 ORPS/ORPD · 57 XORPS/XORPD
            0x54..=0x57 => {
                let (reg, rm) = self.modrm(rex);
                let a = self.xmm[reg];
                let b = self.xmm_load_rm(rm);
                self.xmm[reg] = match op2 {
                    0x54 => a & b,  // ANDPS
                    0x55 => !a & b, // ANDNPS — (NOT dst) AND src
                    0x56 => a | b,  // ORPS
                    _ => a ^ b,     // 0x57 XORPS
                };
            }
            // ── SSE/SSE2 scalar + packed floating point ─────────────────────────
            // Prefix selects the form: F2 = scalar double, F3 = scalar single, 66 =
            // packed double (2 lanes), none = packed single (4 lanes). gcc/cc1 and
            // glibc use these throughout; a real in-guest compile traps without them.
            //
            // CVTSI2SD/SS — integer r/m (32/64 by REX.W) → scalar float, low lane.
            0x2a => {
                let (reg, rm) = self.modrm(rex);
                let bits: u128 = if rex & 0x8 != 0 {
                    let i = self.load_rm(rm, 8) as i64;
                    if f3 {
                        u128::from((i as f32).to_bits())
                    } else {
                        u128::from((i as f64).to_bits())
                    }
                } else {
                    let i = self.load_rm(rm, 4) as u32 as i32;
                    if f3 {
                        u128::from((i as f32).to_bits())
                    } else {
                        u128::from((i as f64).to_bits())
                    }
                };
                if f3 {
                    self.xmm[reg] = (self.xmm[reg] & !0xffff_ffffu128) | bits;
                } else {
                    self.xmm[reg] = (self.xmm[reg] & !u128::from(u64::MAX)) | bits;
                }
            }
            // CVTTSD2SI/SS (truncate) · CVTSD2SI/SS (round) — scalar float → GPR int.
            0x2c | 0x2d => {
                let (reg, rm) = self.modrm(rex);
                let v = if f3 {
                    f64::from(f32::from_bits(self.xmm_load_rm_lo(rm, 4) as u32))
                } else {
                    f64::from_bits(self.xmm_load_rm_lo(rm, 8))
                };
                let r = if op2 == 0x2c {
                    v.trunc()
                } else {
                    v.round_ties_even()
                };
                self.r[reg] = if rex & 0x8 != 0 {
                    r as i64 as u64
                } else {
                    (r as i32 as u64) & 0xffff_ffff
                };
            }
            // UCOMISD/SS · COMISD/SS — compare low lanes → EFLAGS (ZF/PF/CF).
            0x2e | 0x2f => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = if p66 {
                    (
                        f64::from_bits(self.xmm[reg] as u64),
                        f64::from_bits(self.xmm_load_rm_lo(rm, 8)),
                    )
                } else {
                    (
                        f64::from(f32::from_bits(self.xmm[reg] as u32)),
                        f64::from(f32::from_bits(self.xmm_load_rm_lo(rm, 4) as u32)),
                    )
                };
                self.set(flag::OF, false);
                self.set(flag::SF, false);
                self.set(flag::AF, false);
                let unordered = a.is_nan() || b.is_nan();
                self.set(flag::ZF, unordered || a == b);
                self.set(flag::PF, unordered);
                self.set(flag::CF, unordered || a < b);
            }
            // SQRT · ADD · MUL · SUB · MIN · DIV · MAX — scalar or packed.
            0x51 | 0x58 | 0x59 | 0x5c | 0x5d | 0x5e | 0x5f => {
                let (reg, rm) = self.modrm(rex);
                let f64op = |op: u8, a: f64, b: f64| -> f64 {
                    match op {
                        0x58 => a + b,
                        0x59 => a * b,
                        0x5c => a - b,
                        0x5e => a / b,
                        0x5d => {
                            if a < b {
                                a
                            } else {
                                b
                            }
                        }
                        0x5f => {
                            if a > b {
                                a
                            } else {
                                b
                            }
                        }
                        _ => b.sqrt(),
                    }
                };
                let f32op = |op: u8, a: f32, b: f32| -> f32 {
                    match op {
                        0x58 => a + b,
                        0x59 => a * b,
                        0x5c => a - b,
                        0x5e => a / b,
                        0x5d => {
                            if a < b {
                                a
                            } else {
                                b
                            }
                        }
                        0x5f => {
                            if a > b {
                                a
                            } else {
                                b
                            }
                        }
                        _ => b.sqrt(),
                    }
                };
                let dst = self.xmm[reg];
                if f2 {
                    let b = f64::from_bits(self.xmm_load_rm_lo(rm, 8));
                    let r = f64op(op2, f64::from_bits(dst as u64), b);
                    self.xmm[reg] = (dst & !u128::from(u64::MAX)) | u128::from(r.to_bits());
                } else if f3 {
                    let b = f32::from_bits(self.xmm_load_rm_lo(rm, 4) as u32);
                    let r = f32op(op2, f32::from_bits(dst as u32), b);
                    self.xmm[reg] = (dst & !0xffff_ffffu128) | u128::from(r.to_bits());
                } else if p66 {
                    let src = self.xmm_load_rm(rm);
                    let mut out = 0u128;
                    for l in 0..2 {
                        let a = f64::from_bits((dst >> (l * 64)) as u64);
                        let b = f64::from_bits((src >> (l * 64)) as u64);
                        out |= u128::from(f64op(op2, a, b).to_bits()) << (l * 64);
                    }
                    self.xmm[reg] = out;
                } else {
                    let src = self.xmm_load_rm(rm);
                    let mut out = 0u128;
                    for l in 0..4 {
                        let a = f32::from_bits((dst >> (l * 32)) as u32);
                        let b = f32::from_bits((src >> (l * 32)) as u32);
                        out |= u128::from(f32op(op2, a, b).to_bits()) << (l * 32);
                    }
                    self.xmm[reg] = out;
                }
            }
            // CVTSS2SD/CVTSD2SS (scalar) · CVTPS2PD/CVTPD2PS (packed) — float precision.
            0x5a => {
                let (reg, rm) = self.modrm(rex);
                if f2 {
                    let v = f64::from_bits(self.xmm_load_rm_lo(rm, 8)) as f32;
                    self.xmm[reg] = (self.xmm[reg] & !0xffff_ffffu128) | u128::from(v.to_bits());
                } else if f3 {
                    let v = f64::from(f32::from_bits(self.xmm_load_rm_lo(rm, 4) as u32));
                    self.xmm[reg] =
                        (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(v.to_bits());
                } else if p66 {
                    let src = self.xmm_load_rm(rm);
                    let lo = (f64::from_bits(src as u64) as f32).to_bits();
                    let hi = (f64::from_bits((src >> 64) as u64) as f32).to_bits();
                    self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 32);
                } else {
                    let src = self.xmm_load_rm_lo(rm, 8);
                    let lo = f64::from(f32::from_bits(src as u32)).to_bits();
                    let hi = f64::from(f32::from_bits((src >> 32) as u32)).to_bits();
                    self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 64);
                }
            }
            // CVTDQ2PS (none) · CVTPS2DQ (66, round) · CVTTPS2DQ (F3, truncate).
            0x5b => {
                let (reg, rm) = self.modrm(rex);
                let src = self.xmm_load_rm(rm);
                let mut out = 0u128;
                for l in 0..4u32 {
                    if p66 || f3 {
                        let f = f32::from_bits((src >> (l * 32)) as u32);
                        let i = (if f3 { f.trunc() } else { f.round_ties_even() }) as i32 as u32;
                        out |= u128::from(i) << (l * 32);
                    } else {
                        let i = (src >> (l * 32)) as u32 as i32;
                        out |= u128::from((i as f32).to_bits()) << (l * 32);
                    }
                }
                self.xmm[reg] = out;
            }
            // MOVAPS/MOVAPD — reg ← r/m (128, alignment not enforced).
            0x28 => {
                let (reg, rm) = self.modrm(rex);
                self.xmm[reg] = self.xmm_load_rm(rm);
            }
            0x29 => {
                let (reg, rm) = self.modrm(rex);
                self.xmm_store_rm(rm, self.xmm[reg]);
            }
            // MOVMSKPS/PD — gpr ← the sign bits of the 4 floats / 2 doubles.
            0x50 => {
                let (reg, rm) = self.modrm(rex);
                let v = self.xmm_load_rm(rm);
                let m = if p66 {
                    (((v >> 63) & 1) | (((v >> 127) & 1) << 1)) as u64
                } else {
                    let b = v.to_le_bytes();
                    let mut m = 0u64;
                    for i in 0..4 {
                        m |= u64::from(b[i * 4 + 3] >> 7) << i;
                    }
                    m
                };
                self.store_rm(Rm::Reg(reg), 4, m);
            }
            // MOVD/MOVQ — xmm ← r/m (gpr or mem); REX.W ⇒ 64-bit, zero-extended.
            0x6e => {
                let (reg, rm) = self.modrm(rex);
                let sz = if rex & 8 != 0 { 8 } else { 4 };
                let v = self.load_rm(rm, sz);
                self.xmm[reg] = u128::from(v);
            }
            // MOVDQA (66) / MOVDQU (F3) — reg ← r/m (128).
            0x6f => {
                let (reg, rm) = self.modrm(rex);
                self.xmm[reg] = self.xmm_load_rm(rm);
                let _ = (p66, f3); // both forms move 128 bits
            }
            0x7f => {
                let (reg, rm) = self.modrm(rex);
                self.xmm_store_rm(rm, self.xmm[reg]);
            }
            0x7e => {
                let (reg, rm) = self.modrm(rex);
                if f3 {
                    // MOVQ xmm ← xmm/m64 (low 64, zero upper).
                    let v = self.xmm_load_rm_lo(rm, 8);
                    self.xmm[reg] = u128::from(v);
                } else {
                    // MOVD/MOVQ r/m ← xmm (REX.W ⇒ 64).
                    let sz = if rex & 8 != 0 { 8 } else { 4 };
                    self.store_rm(rm, sz, self.xmm[reg] as u64);
                }
            }
            // MOVQ — r/m ← xmm (store low 64; 66 0F D6).
            0xd6 => {
                let (reg, rm) = self.modrm(rex);
                self.xmm_store_lo(rm, self.xmm[reg] as u64, 8);
            }
            // SHUFPS/SHUFPD imm8.
            0xc6 => {
                let (reg, rm) = self.modrm(rex);
                let imm = self.fetch_u8();
                let s = self.xmm_load_rm(rm);
                let d = self.xmm[reg];
                self.xmm[reg] = if p66 {
                    let dq = [d as u64, (d >> 64) as u64];
                    let sq = [s as u64, (s >> 64) as u64];
                    u128::from(dq[(imm & 1) as usize])
                        | (u128::from(sq[((imm >> 1) & 1) as usize]) << 64)
                } else {
                    let dd = to_dwords(d);
                    let sd = to_dwords(s);
                    from_dwords([
                        dd[(imm & 3) as usize],
                        dd[((imm >> 2) & 3) as usize],
                        sd[((imm >> 4) & 3) as usize],
                        sd[((imm >> 6) & 3) as usize],
                    ])
                };
            }
            // PSHUFD (66) / PSHUFLW (F2) / PSHUFHW (F3) imm8.
            0x70 => {
                let (reg, rm) = self.modrm(rex);
                let imm = self.fetch_u8();
                let s = self.xmm_load_rm(rm);
                self.xmm[reg] = if f2 {
                    let w = to_words(s);
                    let mut o = w;
                    for i in 0..4 {
                        o[i] = w[((imm >> (2 * i)) & 3) as usize];
                    }
                    from_words(o)
                } else if f3 {
                    let w = to_words(s);
                    let mut o = w;
                    for i in 0..4 {
                        o[4 + i] = w[4 + ((imm >> (2 * i)) & 3) as usize];
                    }
                    from_words(o)
                } else {
                    let dw = to_dwords(s);
                    from_dwords([
                        dw[(imm & 3) as usize],
                        dw[((imm >> 2) & 3) as usize],
                        dw[((imm >> 4) & 3) as usize],
                        dw[((imm >> 6) & 3) as usize],
                    ])
                };
            }
            // PINSRW imm8 — insert a 16-bit lane from a gpr/m16.
            0xc4 => {
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, 2) as u16;
                let imm = self.fetch_u8();
                let mut w = to_words(self.xmm[reg]);
                w[(imm & 7) as usize] = v;
                self.xmm[reg] = from_words(w);
            }
            // PEXTRW imm8 — extract a 16-bit lane into a gpr.
            0xc5 => {
                let (reg, rm) = self.modrm(rex);
                let imm = self.fetch_u8();
                let w = to_words(self.xmm_load_rm(rm));
                self.store_rm(Rm::Reg(reg), 4, u64::from(w[(imm & 7) as usize]));
            }
            // PMOVMSKB — gpr ← the sign bit of each of the 16 bytes.
            0xd7 => {
                let (reg, rm) = self.modrm(rex);
                let b = self.xmm_load_rm(rm).to_le_bytes();
                let mut m = 0u64;
                for (i, &byte) in b.iter().enumerate() {
                    m |= u64::from(byte >> 7) << i;
                }
                self.store_rm(Rm::Reg(reg), 4, m);
            }
            // Bitwise: PAND/PANDN/POR/PXOR.
            0xdb => self.sse_bin(rex, |a, b| a & b),
            0xdf => self.sse_bin(rex, |a, b| !a & b),
            0xeb => self.sse_bin(rex, |a, b| a | b),
            0xef => self.sse_bin(rex, |a, b| a ^ b),
            // PCMPEQB/W/D.
            0x74 => self.sse_packed_b(rex, |a, b| if a == b { 0xff } else { 0 }),
            0x75 => self.sse_packed_w(rex, |a, b| if a == b { 0xffff } else { 0 }),
            0x76 => self.sse_packed_d(rex, |a, b| if a == b { 0xffff_ffff } else { 0 }),
            // PCMPGTB/W/D (signed).
            0x64 => self.sse_packed_b(rex, |a, b| if (a as i8) > (b as i8) { 0xff } else { 0 }),
            0x65 => self.sse_packed_w(rex, |a, b| if (a as i16) > (b as i16) { 0xffff } else { 0 }),
            0x66 => self.sse_packed_d(rex, |a, b| {
                if (a as i32) > (b as i32) {
                    0xffff_ffff
                } else {
                    0
                }
            }),
            // PADDB/W/D/Q and PSUBB/W/D/Q.
            0xfc => self.sse_packed_b(rex, |a, b| a.wrapping_add(b)),
            0xfd => self.sse_packed_w(rex, |a, b| a.wrapping_add(b)),
            0xfe => self.sse_packed_d(rex, |a, b| a.wrapping_add(b)),
            0xd4 => self.sse_packed_q(rex, |a, b| a.wrapping_add(b)),
            0xf8 => self.sse_packed_b(rex, |a, b| a.wrapping_sub(b)),
            0xf9 => self.sse_packed_w(rex, |a, b| a.wrapping_sub(b)),
            0xfa => self.sse_packed_d(rex, |a, b| a.wrapping_sub(b)),
            0xfb => self.sse_packed_q(rex, |a, b| a.wrapping_sub(b)),
            // PMINUB/PMAXUB (unsigned bytes), PMINSW/PMAXSW (signed words).
            0xda => self.sse_packed_b(rex, |a, b| a.min(b)),
            0xde => self.sse_packed_b(rex, |a, b| a.max(b)),
            0xea => self.sse_packed_w(rex, |a, b| (a as i16).min(b as i16) as u16),
            0xee => self.sse_packed_w(rex, |a, b| (a as i16).max(b as i16) as u16),
            // PADDUSB/PSUBUSB/PADDUSW/PSUBUSW (saturating unsigned).
            0xdc => self.sse_packed_b(rex, |a, b| a.saturating_add(b)),
            0xd8 => self.sse_packed_b(rex, |a, b| a.saturating_sub(b)),
            0xdd => self.sse_packed_w(rex, |a, b| a.saturating_add(b)),
            0xd9 => self.sse_packed_w(rex, |a, b| a.saturating_sub(b)),
            // PMULLW (low 16 of the product).
            0xd5 => self.sse_packed_w(rex, |a, b| a.wrapping_mul(b)),
            // PMULHW/PMULHUW — high 16 of the signed/unsigned word product.
            0xe5 => self.sse_packed_w(rex, |a, b| {
                (((a as i16 as i32) * (b as i16 as i32)) >> 16) as u16
            }),
            0xe4 => self.sse_packed_w(rex, |a, b| ((u32::from(a) * u32::from(b)) >> 16) as u16),
            // PMULUDQ — unsigned 32×32→64 of the low dword of each 64-bit lane.
            0xf4 => self.sse_packed_q(rex, |a, b| (a & 0xffff_ffff) * (b & 0xffff_ffff)),
            // PAVGB/PAVGW — rounded unsigned average.
            0xe0 => self.sse_packed_b(rex, |a, b| ((u16::from(a) + u16::from(b) + 1) >> 1) as u8),
            0xe3 => self.sse_packed_w(rex, |a, b| ((u32::from(a) + u32::from(b) + 1) >> 1) as u16),
            // Saturating signed add/sub (bytes + words).
            0xec => self.sse_packed_b(rex, |a, b| (a as i8).saturating_add(b as i8) as u8),
            0xed => self.sse_packed_w(rex, |a, b| (a as i16).saturating_add(b as i16) as u16),
            0xe8 => self.sse_packed_b(rex, |a, b| (a as i8).saturating_sub(b as i8) as u8),
            0xe9 => self.sse_packed_w(rex, |a, b| (a as i16).saturating_sub(b as i16) as u16),
            // PMADDWD — signed word multiply, adjacent pairs summed into dwords.
            0xf5 => {
                let (reg, rm) = self.modrm(rex);
                let a = to_words(self.xmm[reg]);
                let b = to_words(self.xmm_load_rm(rm));
                let mut o = [0u32; 4];
                for i in 0..4 {
                    let p0 = i32::from(a[2 * i] as i16) * i32::from(b[2 * i] as i16);
                    let p1 = i32::from(a[2 * i + 1] as i16) * i32::from(b[2 * i + 1] as i16);
                    o[i] = p0.wrapping_add(p1) as u32;
                }
                let mut v = 0u128;
                for (i, &word) in o.iter().enumerate() {
                    v |= u128::from(word) << (i * 32);
                }
                self.xmm[reg] = v;
            }
            // PUNPCKL/H BW/WD/DQ/QDQ.
            0x60 => self.sse_unpack(rex, Unpack::LoB),
            0x61 => self.sse_unpack(rex, Unpack::LoW),
            0x62 => self.sse_unpack(rex, Unpack::LoD),
            0x6c => self.sse_unpack(rex, Unpack::LoQ),
            0x68 => self.sse_unpack(rex, Unpack::HiB),
            0x69 => self.sse_unpack(rex, Unpack::HiW),
            0x6a => self.sse_unpack(rex, Unpack::HiD),
            0x6d => self.sse_unpack(rex, Unpack::HiQ),
            // PACKUSWB/PACKSSWB/PACKSSDW (saturating pack).
            0x67 => self.sse_packus_wb(rex),
            // PSLLW/D/Q & PSRLW/D/Q & PSRAW/D by imm8 (group 0F 71/72/73).
            0x71..=0x73 => {
                let (ext, rm) = self.modrm(rex);
                let imm = u32::from(self.fetch_u8());
                let Rm::Reg(i) = rm else { return Ok(()) };
                self.xmm[i] = shift_imm(op2, (ext & 7) as u8, self.xmm[i], imm);
            }
            // PSRLW/D/Q, PSLLW/D/Q, PSRAW/D by an XMM/m count.
            0xd1 => self.sse_shift_var(rex, ShiftKind::SrlW),
            0xd2 => self.sse_shift_var(rex, ShiftKind::SrlD),
            0xd3 => self.sse_shift_var(rex, ShiftKind::SrlQ),
            0xf1 => self.sse_shift_var(rex, ShiftKind::SllW),
            0xf2 => self.sse_shift_var(rex, ShiftKind::SllD),
            0xf3 => self.sse_shift_var(rex, ShiftKind::SllQ),
            0xe1 => self.sse_shift_var(rex, ShiftKind::SraW),
            0xe2 => self.sse_shift_var(rex, ShiftKind::SraD),
            // PSADBW — sum of absolute byte differences into two word lanes.
            0xf6 => {
                let (reg, rm) = self.modrm(rex);
                let a = self.xmm[reg].to_le_bytes();
                let b = self.xmm_load_rm(rm).to_le_bytes();
                let mut lo = 0u64;
                let mut hi = 0u64;
                for i in 0..8 {
                    lo += u64::from(a[i].abs_diff(b[i]));
                    hi += u64::from(a[8 + i].abs_diff(b[8 + i]));
                }
                self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 64);
            }
            _ => {
                #[cfg(feature = "cc44-trace")]
                {
                    let b = self.translate(start);
                    eprintln!(
                        "[sse] UNDEF op2={op2:#04x} p66={p66} f3={f3} f2={f2} @ {start:#x}  bytes={:02x?}",
                        (0..8).map(|k| *self.ram.get(b as usize + k).unwrap_or(&0)).collect::<Vec<_>>()
                    );
                }
                return Err(Halt::Undefined(start));
            }
        }
        Ok(())
    }

    /// Store the low `size` bytes of an SSE value to an r/m operand — the
    /// store-form moves (`MOVSS`/`MOVSD`/`MOVLPS`/`MOVHPS`/`MOVQ`). For a register
    /// destination only the low `size` bytes are replaced.
    fn xmm_store_lo(&mut self, rm: Rm, val: u64, size: u8) {
        match rm {
            Rm::Reg(i) => {
                let m = u128::from(Self::mask(size));
                self.xmm[i] = (self.xmm[i] & !m) | (u128::from(val) & m);
            }
            _ => {
                let a = self.rm_addr(rm).unwrap_or(0);
                self.wr(a, size, val);
            }
        }
    }

    /// Store a 32-bit value to a memory r/m operand (STMXCSR); register form ignored.
    fn xmm_store_lo32(&mut self, rm: Rm, val: u32) {
        if let Some(a) = self.rm_addr(rm) {
            self.wr(a, 4, u64::from(val));
        }
    }

    /// `dst = f(dst, src)` over the whole 128-bit value (the bitwise SSE ops).
    fn sse_bin(&mut self, rex: u8, f: impl Fn(u128, u128) -> u128) {
        let (reg, rm) = self.modrm(rex);
        let s = self.xmm_load_rm(rm);
        self.xmm[reg] = f(self.xmm[reg], s);
    }

    /// Per-byte packed op `dst[i] = f(dst[i], src[i])`.
    fn sse_packed_b(&mut self, rex: u8, f: impl Fn(u8, u8) -> u8) {
        let (reg, rm) = self.modrm(rex);
        let a = self.xmm[reg].to_le_bytes();
        let b = self.xmm_load_rm(rm).to_le_bytes();
        let mut o = [0u8; 16];
        for i in 0..16 {
            o[i] = f(a[i], b[i]);
        }
        self.xmm[reg] = u128::from_le_bytes(o);
    }

    /// Per-word (16-bit) packed op.
    fn sse_packed_w(&mut self, rex: u8, f: impl Fn(u16, u16) -> u16) {
        let (reg, rm) = self.modrm(rex);
        let a = to_words(self.xmm[reg]);
        let b = to_words(self.xmm_load_rm(rm));
        let mut o = [0u16; 8];
        for i in 0..8 {
            o[i] = f(a[i], b[i]);
        }
        self.xmm[reg] = from_words(o);
    }

    /// Per-dword (32-bit) packed op.
    fn sse_packed_d(&mut self, rex: u8, f: impl Fn(u32, u32) -> u32) {
        let (reg, rm) = self.modrm(rex);
        let a = to_dwords(self.xmm[reg]);
        let b = to_dwords(self.xmm_load_rm(rm));
        let mut o = [0u32; 4];
        for i in 0..4 {
            o[i] = f(a[i], b[i]);
        }
        self.xmm[reg] = from_dwords(o);
    }

    /// Per-qword (64-bit) packed op.
    fn sse_packed_q(&mut self, rex: u8, f: impl Fn(u64, u64) -> u64) {
        let (reg, rm) = self.modrm(rex);
        let d = self.xmm[reg];
        let s = self.xmm_load_rm(rm);
        let lo = f(d as u64, s as u64);
        let hi = f((d >> 64) as u64, (s >> 64) as u64);
        self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 64);
    }

    /// `PUNPCKL/H` interleave of `dst` and `src` at the byte/word/dword/qword
    /// granularity, from the low or high half.
    fn sse_unpack(&mut self, rex: u8, kind: Unpack) {
        let (reg, rm) = self.modrm(rex);
        let d = self.xmm[reg];
        let s = self.xmm_load_rm(rm);
        self.xmm[reg] = match kind {
            Unpack::LoB => unpack_bytes(d, s, false),
            Unpack::HiB => unpack_bytes(d, s, true),
            Unpack::LoW => unpack_words(d, s, false),
            Unpack::HiW => unpack_words(d, s, true),
            Unpack::LoD => unpckl_dwords(d, s),
            Unpack::HiD => unpckh_dwords(d, s),
            Unpack::LoQ => unpckl_qwords(d, s),
            Unpack::HiQ => unpckh_qwords(d, s),
        };
    }

    /// `PACKUSWB` — pack 8+8 signed words into 16 unsigned-saturated bytes.
    fn sse_packus_wb(&mut self, rex: u8) {
        let (reg, rm) = self.modrm(rex);
        let a = to_words(self.xmm[reg]);
        let b = to_words(self.xmm_load_rm(rm));
        let sat = |w: u16| -> u8 { (w as i16).clamp(0, 255) as u8 };
        let mut o = [0u8; 16];
        for i in 0..8 {
            o[i] = sat(a[i]);
            o[8 + i] = sat(b[i]);
        }
        self.xmm[reg] = u128::from_le_bytes(o);
    }

    /// Variable (by an XMM/m count) packed shift.
    fn sse_shift_var(&mut self, rex: u8, kind: ShiftKind) {
        let (reg, rm) = self.modrm(rex);
        let cnt = (self.xmm_load_rm(rm) as u64).min(255) as u32;
        self.xmm[reg] = shift_lanes(kind, self.xmm[reg], cnt);
    }

    /// `LTR` — load the task register: read the 64-bit TSS descriptor `sel`
    /// selects from the GDT and latch the TSS base (for the ring-transition stack
    /// switch).
    fn load_tr(&mut self, sel: u16) {
        let (gdt, _) = self.sys().gdtr;
        let desc = gdt + u64::from(sel & 0xfff8);
        let lo = self.rd_virt(desc, 8);
        let hi = self.rd_virt(desc + 8, 8);
        let base =
            ((lo >> 16) & 0xff_ffff) | (((lo >> 56) & 0xff) << 24) | ((hi & 0xffff_ffff) << 32);
        self.sys_mut().tr_base = base;
    }

    /// `SYSCALL` — the fast system-call entry: save `RIP→RCX`, `RFLAGS→R11`, mask
    /// `RFLAGS` with `SFMASK`, load `RIP` from `LSTAR`, and switch `CS`/`SS` to the
    /// kernel selectors from `STAR`. The userspace PID-1 enters the kernel here.
    fn syscall_enter(&mut self) {
        let star = self.sys().msr.get(&MSR_STAR).copied().unwrap_or(0);
        let lstar = self.sys().msr.get(&MSR_LSTAR).copied().unwrap_or(0);
        let sfmask = self.sys().msr.get(&MSR_SFMASK).copied().unwrap_or(0);
        self.r[RCX] = self.rip;
        self.r[11] = self.rflags;
        self.rflags &= !sfmask;
        self.rflags &= !RFLAGS_IF;
        let kcs = ((star >> 32) & 0xffff) as u16;
        self.seg[SegId::Cs as usize].selector = kcs;
        self.seg[SegId::Cs as usize].long = true;
        self.seg[SegId::Ss as usize].selector = kcs.wrapping_add(8);
        self.cpl = 0;
        self.rip = lstar;
    }

    /// `SYSRET` — the fast system-call return: restore `RIP←RCX`, `RFLAGS←R11`,
    /// and the userspace `CS`/`SS` from `STAR`, returning to CPL 3.
    fn sysret(&mut self) {
        let star = self.sys().msr.get(&MSR_STAR).copied().unwrap_or(0);
        self.rip = self.r[RCX];
        // Restore RFLAGS from R11 (the value SYSCALL saved). The mask MUST keep IF
        // (bit 9, 0x200): SYSRET restores the interrupt flag from R11 — dropping it
        // left userspace running with interrupts permanently disabled after any
        // syscall (no timer preemption, no device IRQs, until the next — impossible
        // — interrupt), wedging the boot once a task needed to be woken.
        self.rflags = (self.r[11] & 0x0024_4fd5) | 0x2;
        let ucs = (((star >> 48) & 0xffff) as u16).wrapping_add(16) | 3;
        self.seg[SegId::Cs as usize].selector = ucs;
        self.seg[SegId::Cs as usize].long = true;
        self.seg[SegId::Ss as usize].selector =
            (((star >> 48) & 0xffff) as u16).wrapping_add(8) | 3;
        self.cpl = 3;
    }
}

/// Sign-extend the low `size` bytes of `v` to a full `i64`.
fn sign_extend(v: u64, size: u8) -> i64 {
    match size {
        1 => v as u8 as i8 as i64,
        2 => v as u16 as i16 as i64,
        4 => v as u32 as i32 as i64,
        _ => v as i64,
    }
}

// ── SSE packed-lane helpers (little-endian lane order) ───────────────────────

fn to_words(v: u128) -> [u16; 8] {
    let b = v.to_le_bytes();
    core::array::from_fn(|i| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]))
}
fn from_words(w: [u16; 8]) -> u128 {
    let mut b = [0u8; 16];
    for i in 0..8 {
        b[2 * i..2 * i + 2].copy_from_slice(&w[i].to_le_bytes());
    }
    u128::from_le_bytes(b)
}
fn to_dwords(v: u128) -> [u32; 4] {
    let b = v.to_le_bytes();
    core::array::from_fn(|i| {
        u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
    })
}
fn from_dwords(d: [u32; 4]) -> u128 {
    let mut b = [0u8; 16];
    for i in 0..4 {
        b[4 * i..4 * i + 4].copy_from_slice(&d[i].to_le_bytes());
    }
    u128::from_le_bytes(b)
}

/// `PUNPCKL/HBW` — interleave bytes of `a` (even result lanes) and `b` (odd) from
/// the low (`hi=false`) or high (`hi=true`) half.
fn unpack_bytes(a: u128, b: u128, hi: bool) -> u128 {
    let (ab, bb) = (a.to_le_bytes(), b.to_le_bytes());
    let base = if hi { 8 } else { 0 };
    let mut o = [0u8; 16];
    for i in 0..8 {
        o[2 * i] = ab[base + i];
        o[2 * i + 1] = bb[base + i];
    }
    u128::from_le_bytes(o)
}
fn unpack_words(a: u128, b: u128, hi: bool) -> u128 {
    let (aw, bw) = (to_words(a), to_words(b));
    let base = if hi { 4 } else { 0 };
    let mut o = [0u16; 8];
    for i in 0..4 {
        o[2 * i] = aw[base + i];
        o[2 * i + 1] = bw[base + i];
    }
    from_words(o)
}
fn unpckl_dwords(a: u128, b: u128) -> u128 {
    let (ad, bd) = (to_dwords(a), to_dwords(b));
    from_dwords([ad[0], bd[0], ad[1], bd[1]])
}
fn unpckh_dwords(a: u128, b: u128) -> u128 {
    let (ad, bd) = (to_dwords(a), to_dwords(b));
    from_dwords([ad[2], bd[2], ad[3], bd[3]])
}
fn unpckl_qwords(a: u128, b: u128) -> u128 {
    (a & u128::from(u64::MAX)) | (b << 64)
}
fn unpckh_qwords(a: u128, b: u128) -> u128 {
    (a >> 64) | ((b >> 64) << 64)
}

/// The granularity + direction of a packed shift selected by the `0F 71/72/73`
/// group's ModRM.reg field.
fn shift_imm(op2: u8, ext: u8, v: u128, cnt: u32) -> u128 {
    // 0x73 /3 = PSRLDQ, /7 = PSLLDQ — whole-register *byte* shifts.
    if op2 == 0x73 && ext == 3 {
        return if cnt >= 16 { 0 } else { v >> (cnt * 8) };
    }
    if op2 == 0x73 && ext == 7 {
        return if cnt >= 16 { 0 } else { v << (cnt * 8) };
    }
    let kind = match (op2, ext) {
        (0x71, 2) => ShiftKind::SrlW,
        (0x71, 4) => ShiftKind::SraW,
        (0x71, 6) => ShiftKind::SllW,
        (0x72, 2) => ShiftKind::SrlD,
        (0x72, 4) => ShiftKind::SraD,
        (0x72, 6) => ShiftKind::SllD,
        (0x73, 2) => ShiftKind::SrlQ,
        (0x73, 6) => ShiftKind::SllQ,
        _ => return v,
    };
    shift_lanes(kind, v, cnt)
}

/// Apply a per-lane logical/arithmetic shift by `cnt` (a count ≥ the lane width
/// produces 0, or a full sign-fill for the arithmetic forms).
fn shift_lanes(kind: ShiftKind, v: u128, cnt: u32) -> u128 {
    match kind {
        ShiftKind::SrlW => from_words(to_words(v).map(|x| if cnt >= 16 { 0 } else { x >> cnt })),
        ShiftKind::SllW => from_words(to_words(v).map(|x| if cnt >= 16 { 0 } else { x << cnt })),
        ShiftKind::SraW => from_words(to_words(v).map(|x| {
            let s = cnt.min(15);
            ((x as i16) >> s) as u16
        })),
        ShiftKind::SrlD => from_dwords(to_dwords(v).map(|x| if cnt >= 32 { 0 } else { x >> cnt })),
        ShiftKind::SllD => from_dwords(to_dwords(v).map(|x| if cnt >= 32 { 0 } else { x << cnt })),
        ShiftKind::SraD => from_dwords(to_dwords(v).map(|x| {
            let s = cnt.min(31);
            ((x as i32) >> s) as u32
        })),
        ShiftKind::SrlQ => {
            let lo = if cnt >= 64 { 0 } else { (v as u64) >> cnt };
            let hi = if cnt >= 64 {
                0
            } else {
                ((v >> 64) as u64) >> cnt
            };
            u128::from(lo) | (u128::from(hi) << 64)
        }
        ShiftKind::SllQ => {
            let lo = if cnt >= 64 { 0 } else { (v as u64) << cnt };
            let hi = if cnt >= 64 {
                0
            } else {
                ((v >> 64) as u64) << cnt
            };
            u128::from(lo) | (u128::from(hi) << 64)
        }
    }
}

/// The interleave selector for `PUNPCKL/H {BW,WD,DQ,QDQ}`.
#[derive(Clone, Copy)]
enum Unpack {
    LoB,
    HiB,
    LoW,
    HiW,
    LoD,
    HiD,
    LoQ,
    HiQ,
}

/// A packed-shift lane width + direction.
#[derive(Clone, Copy)]
enum ShiftKind {
    SrlW,
    SllW,
    SraW,
    SrlD,
    SllD,
    SraD,
    SrlQ,
    SllQ,
}

/// A decoded ModRM r/m operand: a register index, a resolved effective address,
/// or a RIP-relative operand (`disp`, `segment base`) resolved lazily once the
/// full instruction — including any trailing immediate — has decoded.
#[derive(Clone, Copy)]
enum Rm {
    Reg(usize),
    Mem(u64),
    RipRel(i64, u64),
}

/// A string-instruction repeat prefix (`REP`/`REPE` = `F3`, `REPNE` = `F2`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RepKind {
    None,
    Rep,
    Repne,
}

/// The string instruction family (`MOVS`/`STOS`/`LODS`/`SCAS`/`CMPS`).
#[derive(Clone, Copy)]
enum StringOp {
    Movs,
    Stos,
    Lods,
    Scas,
    Cmps,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load + run a flat x86-64 code blob; assert it stopped cleanly.
    fn run(code: &[u8]) -> Cpu {
        let mut cpu = Cpu::new(64 * 1024);
        cpu.load_at(0, code);
        let h = cpu.run(100_000);
        assert!(
            matches!(h, Halt::Halted | Halt::OutOfBudget),
            "unexpected halt: {h:?}"
        );
        cpu
    }

    #[test]
    fn adds_two_registers() {
        // mov rax,5 ; mov rbx,7 ; add rax,rbx ; hlt
        let code = [
            0x48, 0xc7, 0xc0, 0x05, 0, 0, 0, // mov rax, 5
            0x48, 0xc7, 0xc3, 0x07, 0, 0, 0, // mov rbx, 7
            0x48, 0x01, 0xd8, // add rax, rbx
            0xf4, // hlt
        ];
        assert_eq!(run(&code).reg(0), 12);
    }

    #[test]
    fn countdown_loop_sets_the_zero_flag() {
        // mov rcx,3 ; (dec rcx ; jnz -5) ; hlt  — a real branch loop
        let code = [
            0x48, 0xc7, 0xc1, 0x03, 0, 0, 0, // mov rcx, 3
            0x48, 0xff, 0xc9, // dec rcx
            0x75, 0xfb, // jnz -5 (back to dec)
            0xf4, // hlt
        ];
        let cpu = run(&code);
        assert_eq!(cpu.reg(1), 0, "the loop ran to zero");
        assert!(cpu.rflags() & flag::ZF != 0, "ZF set at zero");
    }

    #[test]
    fn writes_to_the_serial_console() {
        // mov dx,0x3f8 ; mov al,'h' ; out dx,al ; mov al,'i' ; out dx,al ; hlt
        let code = [
            0x66, 0xba, 0xf8, 0x03, // mov dx, 0x3f8 (the 16550 THR port)
            0xb0, b'h', 0xee, // mov al,'h' ; out dx,al
            0xb0, b'i', 0xee, // mov al,'i' ; out dx,al
            0xf4, // hlt
        ];
        assert_eq!(run(&code).console(), b"hi");
    }

    #[test]
    fn long_mode_paging_translates_through_four_levels() {
        // Build a 4 KiB-page mapping VA 0 → PA 0x5000 through PML4→PDPT→PD→PT
        // (entry 0 of each table, the tables at 0x1000/0x2000/0x3000/0x4000), then
        // enable long-mode paging and check translation honours it.
        let mut cpu = Cpu::new(64 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| {
            cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        };
        put(&mut cpu, 0x1000, 0x2000 | 1); // PML4[0] → PDPT, present
        put(&mut cpu, 0x2000, 0x3000 | 1); // PDPT[0] → PD
        put(&mut cpu, 0x3000, 0x4000 | 1); // PD[0]   → PT
        put(&mut cpu, 0x4000, 0x5000 | 1); // PT[0]   → frame 0x5000
        cpu.cr3 = 0x1000;
        cpu.cr4 = 1 << 5; // PAE
        cpu.efer = 1 << 8; // LME
        assert_eq!(cpu.translate(0x123), 0x123, "paging off → identity");
        cpu.cr0 = 1 << 31; // PG → paging on
        assert_eq!(cpu.translate(0x123), 0x5123, "VA 0x123 maps to PA 0x5123");
        // A write through the VA lands at the physical frame (rd/wr translate).
        cpu.wr(0x40, 4, 0xdead_beef);
        assert_eq!(
            cpu.rd_phys(0x5040, 4),
            0xdead_beef,
            "the write hit frame 0x5000"
        );
    }

    /// The software TLB caches a page's R/W permission, but the kernel can upgrade
    /// a page RO→RW (COW / dirty / access-flag set) and — seeing the live PTE
    /// already permits the access — never re-flush. A naive cache would then fault
    /// forever on that stale `writable=false` (the `fork`/exec COW storm that
    /// stalls a real busybox boot). The MMU must re-validate a TLB-sourced
    /// permission denial against the live page tables before faulting, exactly as
    /// qemu's hardware TLB stays coherent. This reproduces the upgrade-without-flush
    /// and asserts the write then succeeds (no infinite fault loop).
    #[test]
    fn software_tlb_revalidates_stale_write_permission() {
        let mut cpu = Cpu::new(256 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| {
            cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        };
        // VA 0 → frame 0x6000 through PML4→PDPT→PD→PT; upper entries present+RW,
        // the leaf present but **read-only** (RW=0) — a supervisor page.
        put(&mut cpu, 0x1000, 0x2000 | 0b11);
        put(&mut cpu, 0x2000, 0x3000 | 0b11);
        put(&mut cpu, 0x3000, 0x4000 | 0b11);
        put(&mut cpu, 0x4000, 0x6000 | 0b001); // leaf: present, RW=0
        cpu.cr3 = 0x1000;
        cpu.cr4 = 1 << 5; // PAE
        cpu.efer = 1 << 8; // LME
        cpu.cr0 = (1 << 31) | CR0_WP; // PG | WP (enforce RW for supervisor too)
        cpu.cpl = 0;

        // A read caches the translation with writable=false.
        let _ = cpu.rd(0x10, 4);
        assert!(cpu.fault.is_none(), "reading a present page does not fault");

        // A write to the read-only page faults (the COW edge), as on real hardware.
        cpu.wr(0x10, 4, 0x1111_2222);
        assert!(
            cpu.fault.is_some(),
            "a write to a read-only page faults (COW)"
        );
        cpu.fault = None; // ...the kernel's #PF handler runs.

        // The kernel upgrades the leaf RO→RW but does NOT flush our software TLB
        // (it relies on the spurious-fault path; the live PTE now permits the write).
        put(&mut cpu, 0x4000, 0x6000 | 0b011); // leaf: present, RW=1

        // The write must now succeed — the MMU re-walks on the stale denial and sees
        // the live PTE permits it — instead of faulting forever on the stale entry.
        cpu.wr(0x10, 4, 0x1111_2222);
        assert!(
            cpu.fault.is_none(),
            "after RO→RW upgrade the write succeeds (no stale-TLB COW fault storm)"
        );
        assert_eq!(
            cpu.rd_phys(0x6010, 4),
            0x1111_2222,
            "the write landed at the frame, not the phys-0 fault scratch"
        );
    }

    #[test]
    fn mov_cr_and_wrmsr_set_up_long_mode() {
        // mov rax, 0x1000 ; mov cr3, rax ; mov rax, 0x20 ; mov cr4, rax ;
        // mov ecx,0xC0000080 ; mov eax,0x100 ; xor edx,edx ; wrmsr ; rdmsr ; hlt
        let code = [
            0x48, 0xc7, 0xc0, 0x00, 0x10, 0, 0, // mov rax, 0x1000
            0x0f, 0x22, 0xd8, // mov cr3, rax
            0xb9, 0x80, 0x00, 0x00, 0xc0, // mov ecx, 0xC0000080
            0xb8, 0x00, 0x01, 0x00, 0x00, // mov eax, 0x100 (LME)
            0x31, 0xd2, // xor edx, edx
            0x0f, 0x30, // wrmsr → EFER = 0x100
            0x0f, 0x32, // rdmsr → eax = EFER low
            0xf4, // hlt
        ];
        let cpu = run(&code);
        assert_eq!(cpu.cr3, 0x1000, "mov cr3 installed the PML4 base");
        assert_eq!(cpu.efer, 0x100, "wrmsr set EFER.LME");
        assert_eq!(
            cpu.reg(0) & 0xffff_ffff,
            0x100,
            "rdmsr read EFER back into eax"
        );
    }

    #[test]
    fn call_and_ret_use_the_stack() {
        // call +1 (push ret, jump over) ; hlt ; (target:) ret would pop — instead
        // verify push/pop directly: mov rax,0x1234 ; push rax ; pop rbx ; hlt
        let code = [
            0x48, 0xc7, 0xc0, 0x34, 0x12, 0, 0,    // mov rax, 0x1234
            0x50, // push rax
            0x5b, // pop rbx
            0xf4, // hlt
        ];
        assert_eq!(run(&code).reg(3), 0x1234);
    }
}
