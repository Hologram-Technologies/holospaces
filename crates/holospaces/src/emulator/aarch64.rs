//! **AArch64 (ARMv8-A) core** — the system emulator's second ISA target
//! (ADR-021). This module is the A64 **integer** instruction core (`CC-35`): a
//! faithful interpreter of the A64 base instruction set over a flat
//! little-endian RAM, with the `NZCV` condition flags driving the condition
//! codes — the AArch64 analogue of the [RISC-V core](super)'s integer base.
//!
//! The external authority is the **Arm Architecture Reference Manual** (ARM DDI
//! 0487) for the A64 base instruction set and `PSTATE.NZCV`, with
//! `qemu-system-aarch64` as the differential oracle (`vv/suites/cc35-aarch64-core`):
//! a hand-assembled A64 battery's final register/memory state is the
//! Arm-ARM-defined result and matches qemu byte-for-byte, with `X0` carrying the
//! self-check verdict. The in-crate batteries below exercise every instruction
//! group against that same Arm-ARM-defined result.
//!
//! The instruction groups implemented (the `CC-35` surface): data-processing
//! immediate (`ADD`/`SUB`/`ADDS`/`SUBS` immediate, the logical-immediate group,
//! `MOVZ`/`MOVN`/`MOVK`, the bitfield/`EXTR` group, `ADR`/`ADRP`);
//! data-processing register (logical and add/sub shifted/extended register,
//! add/sub-with-carry, conditional select, conditional compare, the 1/2/3-source
//! groups — `MADD`/`MSUB`/`MUL`, `UMULH`/`SMULH`, `UDIV`/`SDIV`, the variable
//! shifts, `RBIT`/`REV`/`CLZ`/`CLS`); the full load/store family (`LDR`/`STR`
//! unsigned-offset + unscaled + pre/post-index + register-offset, `LDR` literal,
//! `LDP`/`STP`, with the sign/zero-extension variants); and control flow
//! (`B`/`BL`/`B.cond`/`CBZ`/`CBNZ`/`TBZ`/`TBNZ`/`RET`/`BR`/`BLR`). A flat program
//! reaches the host boundary with `SVC #0` under the Linux `arm64` syscall ABI
//! (`x8` selects `write`/`exit`) — the A64 analogue of the RISC-V `ecall` flat
//! boundary — so a self-checking battery exits with its verdict in `X0`.
//!
//! `no_std` + `alloc`, like the RISC-V core, so the AArch64 target compiles into
//! the same κ-addressed emulator codemodule (ADR-009).

use alloc::collections::BTreeMap;
#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::{boxed::Box, string::String, vec, vec::Vec};

/// The Linux `arm64` syscall numbers the `SVC #0` host boundary recognises (the
/// guest passes the number in `x8`, arguments in `x0`–`x5`). Identical numbers
/// to the RISC-V core's generic Linux ABI — `write` and `exit`/`exit_group` let
/// a real self-checking battery run unmodified.
mod syscall {
    pub const WRITE: u64 = 64;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
}

/// Why the AArch64 core stopped stepping — the analogue of [`super::Halt`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Halt {
    /// The guest called `exit`/`exit_group` (`SVC #0`, `x8` = 93/94) with this
    /// status in `x0`.
    Exit(u64),
    /// An instruction could not be executed (an unimplemented/illegal encoding or
    /// a memory fault).
    Trap(Trap),
    /// The step budget was exhausted before the guest exited (a liveness bound,
    /// not a guest fault).
    OutOfBudget,
}

/// An A64 processor trap — an encoding the core cannot execute, a fault, or a
/// debug/host event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trap {
    /// An A64 encoding the integer core does not implement / a reserved encoding.
    Illegal(u32),
    /// A load/store/fetch outside guest RAM (the flat-core analogue of an
    /// external abort).
    AccessFault(u64),
    /// An `SVC #0` whose `x8` is not a recognised syscall.
    UnknownSyscall(u64),
    /// A `BRK #imm` software breakpoint (the `imm16` comment field).
    Breakpoint(u16),
    /// A `HLT #imm` — a flat battery uses it as an explicit halt.
    Halted(u16),
}

/// `PSTATE.NZCV` — the four condition flags the A64 condition codes read.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Nzcv {
    n: bool,
    z: bool,
    c: bool,
    v: bool,
}

impl Nzcv {
    /// Pack into the `NZCV` field layout (`N` at bit 31 … `V` at bit 28) — the
    /// `MRS NZCV` / `SPSR` form.
    fn pack(self) -> u32 {
        (u32::from(self.n) << 31)
            | (u32::from(self.z) << 30)
            | (u32::from(self.c) << 29)
            | (u32::from(self.v) << 28)
    }
    /// Unpack a 4-bit `nzcv` immediate (`{n,z,c,v}` in bits 3..0) — the
    /// `CCMP`/`CCMN` "condition false" result.
    fn from_imm4(v: u32) -> Self {
        Nzcv {
            n: v & 0b1000 != 0,
            z: v & 0b0100 != 0,
            c: v & 0b0010 != 0,
            v: v & 0b0001 != 0,
        }
    }
}

/// A single AArch64 PE (processing element) over a flat little-endian RAM: the
/// 31 general registers `X0`–`X30`, the stack pointer `SP` (the architectural
/// `XZR`/`SP` split at register slot 31 is resolved per-instruction), the program
/// counter, and `PSTATE.NZCV`. RAM is mapped at `base`; a flat image loads there
/// and the reset PC is `base`. Deterministic — identical image + input yield
/// identical console output and final state (Law L1/L5), so a κ snapshot is
/// reproducible.
pub struct Cpu {
    x: [u64; 31],
    sp: u64,
    pc: u64,
    flags: Nzcv,
    ram: Vec<u8>,
    base: u64,
    console: Vec<u8>,
    /// The privileged-mode state (`CC-36`): the EL0/EL1 exception model,
    /// VMSAv8-64 MMU, and the ARM `virt` platform devices. `None` is the flat
    /// `CC-35` integer core — `SVC` is the host syscall boundary, memory is
    /// identity-mapped RAM, and there is no privileged state — so the integer
    /// batteries run unchanged. [`Cpu::boot_linux`] installs `Some` to boot a real
    /// `arm64` kernel.
    sys: Option<Box<Sys>>,
    /// The local exclusive monitor address set by `LDXR`/`LDAXR`; a `STXR`/`STLXR`
    /// succeeds only while it is set (cleared on exception entry, so a `LDXR`/`STXR`
    /// loop retries when interrupted — the kernel's lock semantics on a single PE).
    excl: Option<u64>,
    /// The SIMD&FP register file `V0`–`V31` (128-bit). The integer core never
    /// computes with them; they exist so the kernel's FP/SIMD context
    /// save/restore (`LDP`/`STP`/`LDR`/`STR` of the `Q`/`D`/`S` registers, e.g.
    /// `fpsimd_load_state`) moves a task's FP state through memory on the way to
    /// userspace (`CC-36`).
    v: [u128; 32],
}

/// The kind of guest memory access (selects the page-table permission bit and the
/// fault syndrome).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Fetch,
    Load,
    Store,
}

impl Cpu {
    /// A new machine with `ram_bytes` of zeroed RAM mapped at `base`; the reset PC
    /// is `base` and `SP` is the top of RAM (a flat battery resets it as needed).
    #[must_use]
    pub fn new(base: u64, ram_bytes: usize) -> Self {
        Cpu {
            x: [0; 31],
            sp: base.wrapping_add(ram_bytes as u64),
            pc: base,
            flags: Nzcv::default(),
            ram: vec![0; ram_bytes],
            base,
            console: Vec::new(),
            sys: None,
            excl: None,
            v: [0; 32],
        }
    }

    /// Load a flat A64 image at `base` and reset the PC to it.
    pub fn load_image(&mut self, image: &[u8]) {
        let n = image.len().min(self.ram.len());
        self.ram[..n].copy_from_slice(&image[..n]);
        self.pc = self.base;
    }

    /// **V&V device-driver hook** (`CC-46`): perform a device-MMIO store at the
    /// guest-physical address `pa`, exactly as a guest's `virtio` driver would —
    /// the same private [`phys_write`](Self::phys_write) the executing CPU routes
    /// device stores through. A conformance witness uses it to drive the shared
    /// `devbus` devices over the AArch64 MMIO transport (post the
    /// virtqueue and ring `QueueNotify`) without booting a full guest kernel.
    #[doc(hidden)]
    pub fn vv_mmio_write(&mut self, pa: u64, width: usize, value: u64) {
        self.phys_write(pa, width, value);
    }

    /// **V&V device-driver hook** (`CC-46`): perform a device-MMIO load at `pa`
    /// — the same path the executing CPU routes device loads through. Reads a
    /// `virtio-mmio` register (magic/device-id/interrupt-status/config space).
    #[doc(hidden)]
    pub fn vv_mmio_read(&mut self, pa: u64, width: usize) -> u64 {
        self.phys_read(pa, width)
    }

    /// **V&V hook** (`CC-46`): write `bytes` into guest RAM at guest-physical
    /// `pa` — a witness lays out the virtqueue (descriptor table, avail ring) and
    /// the T-message buffers a guest driver would build in RAM.
    #[doc(hidden)]
    pub fn vv_ram_write(&mut self, pa: u64, bytes: &[u8]) {
        let o = (pa - self.base) as usize;
        self.ram[o..o + bytes.len()].copy_from_slice(bytes);
    }

    /// **V&V hook** (`CC-46`): read `len` bytes of guest RAM at guest-physical
    /// `pa` — a witness reads back the R-message the device scattered into the
    /// writable descriptors, and the used-ring the device updated.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_ram_read(&self, pa: u64, len: usize) -> Vec<u8> {
        let o = (pa - self.base) as usize;
        self.ram[o..o + len].to_vec()
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of this machine's first
    /// `virtio-9p` MMIO slot, and the workspace mount tag — so a witness drives
    /// the device at the same address the AArch64 `virt` device tree advertises.
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

    /// The bytes the guest has written to the host console (`SVC #0`, `write` to
    /// fd 1/2).
    #[must_use]
    pub fn console(&self) -> &[u8] {
        &self.console
    }

    /// The current value of `X0`–`X30` (`i` in `0..=30`); `X31` reads as the zero
    /// register. The verdict register a `CC-35` battery leaves its self-check in.
    #[must_use]
    pub fn xreg(&self, i: usize) -> u64 {
        if i >= 31 {
            0
        } else {
            self.x[i]
        }
    }

    /// The current `SP`.
    #[must_use]
    pub fn sp(&self) -> u64 {
        self.sp
    }

    /// The current PC.
    #[must_use]
    pub fn pc(&self) -> u64 {
        self.pc
    }

    /// Run up to `max_steps` instructions, stopping at the first `Halt` (an
    /// `exit`, a trap, or the budget). The liveness bound mirrors the RISC-V
    /// core's [`run`](super::Emulator::run).
    pub fn run(&mut self, max_steps: u64) -> Halt {
        for _ in 0..max_steps {
            // Pump the network periodically so host-side data and connection
            // events reach the guest without it having to transmit first (the
            // `virtio-net` receive path; `CC-16` parity, `CC-46`). Gated on the
            // architected counter, which `sys_tick` advances every step.
            if self.sys.is_some()
                && self.sys().virtionet.is_some()
                && self.sys().counter & 0x3ff == 0
            {
                self.virtio_net_pump();
            }
            if let Err(h) = self.step() {
                return h;
            }
        }
        Halt::OutOfBudget
    }

    // ── register file (the XZR/SP slot-31 split) ────────────────────────────

    /// Read register `i` with slot 31 = `XZR` (the common data-processing case).
    #[inline]
    fn rx(&self, i: u32) -> u64 {
        if i == 31 {
            0
        } else {
            self.x[i as usize]
        }
    }

    /// Read register `i` with slot 31 = `SP` (base registers and `SP`-forms).
    #[inline]
    fn rx_sp(&self, i: u32) -> u64 {
        if i == 31 {
            self.sp
        } else {
            self.x[i as usize]
        }
    }

    /// Write register `i` with slot 31 = `XZR` (discard).
    #[inline]
    fn wx(&mut self, i: u32, v: u64) {
        if i != 31 {
            self.x[i as usize] = v;
        }
    }

    /// Write register `i` with slot 31 = `SP`.
    #[inline]
    fn wx_sp(&mut self, i: u32, v: u64) {
        if i == 31 {
            self.sp = v;
        } else {
            self.x[i as usize] = v;
        }
    }

    /// A 32-bit register write zero-extends into the 64-bit slot (the A64 rule for
    /// every `W`-register result).
    #[inline]
    fn wx_sz(&mut self, i: u32, v: u64, sf: bool) {
        self.wx(i, if sf { v } else { v & 0xffff_ffff });
    }

    /// Read register `i` as an operand of size `sf` (64-bit) / `!sf` (32-bit,
    /// masked to the low word), slot 31 = `XZR`.
    #[inline]
    fn op(&self, i: u32, sf: bool) -> u64 {
        let v = self.rx(i);
        if sf {
            v
        } else {
            v & 0xffff_ffff
        }
    }

    // ── flat memory bus ─────────────────────────────────────────────────────

    #[inline]
    fn offset(&self, addr: u64, width: usize) -> Result<usize, Trap> {
        let off = addr.wrapping_sub(self.base);
        let end = off.checked_add(width as u64);
        match end {
            Some(e) if e <= self.ram.len() as u64 => Ok(off as usize),
            _ => Err(Trap::AccessFault(addr)),
        }
    }

    fn read(&mut self, addr: u64, width: usize, acc: Access) -> Result<u64, Trap> {
        if self.sys.is_some() {
            let pa = self.translate(addr, acc)?;
            return Ok(self.phys_read(pa, width));
        }
        let o = self.offset(addr, width)?;
        let mut v = 0u64;
        for i in 0..width {
            v |= u64::from(self.ram[o + i]) << (8 * i);
        }
        Ok(v)
    }

    fn write(&mut self, addr: u64, width: usize, value: u64) -> Result<(), Trap> {
        if self.sys.is_some() {
            let pa = self.translate(addr, Access::Store)?;
            self.phys_write(pa, width, value);
            return Ok(());
        }
        let o = self.offset(addr, width)?;
        for i in 0..width {
            self.ram[o + i] = (value >> (8 * i)) as u8;
        }
        Ok(())
    }

    // ── the step loop ───────────────────────────────────────────────────────

    /// Fetch, decode, and execute one A64 instruction (a fixed 32-bit word). In
    /// system mode (`CC-36`) a pending interrupt is delivered first, and a fetch /
    /// data abort or an undefined instruction raises the corresponding EL1
    /// exception (rather than halting the flat core).
    fn step(&mut self) -> Result<(), Halt> {
        if self.sys.is_some() {
            if self.sys().halted {
                return Err(Halt::Exit(self.sys().halt_status));
            }
            self.sys_tick();
            if self.take_pending_interrupt() {
                return Ok(());
            }
        }
        let pc = self.pc;
        let inst = match self.read(pc, 4, Access::Fetch) {
            Ok(v) => v as u32,
            Err(t) => {
                if self.sys.is_some() {
                    self.take_mem_abort(pc, pc, true, false, false);
                    return Ok(());
                }
                return Err(Halt::Trap(t));
            }
        };
        match self.exec(inst, pc) {
            Err(Halt::Trap(t)) if self.sys.is_some() => {
                self.take_exec_trap(pc, inst, t);
                Ok(())
            }
            other => other,
        }
    }

    /// Decode + execute one instruction. The PC is advanced to `pc + 4` unless a
    /// branch sets it. `Err(Halt)` stops the machine — an `exit` (`Halt::Exit`),
    /// or a `Halt::Trap` for an illegal encoding or a fault (a flat core has no
    /// vectors; the privileged exception model is `CC-36`).
    fn exec(&mut self, inst: u32, pc: u64) -> Result<(), Halt> {
        // A64 top-level decode (Arm-ARM C4.1): op0 = bits[28:25].
        let next = pc.wrapping_add(4);
        // Data Processing -- Immediate (op0 = 100x).
        if inst & 0x1c00_0000 == 0x1000_0000 {
            self.dp_immediate(inst, pc, next).map_err(Halt::Trap)
        } else if inst & 0x1c00_0000 == 0x1400_0000 {
            // Branches, Exception generating, and System (op0 = 101x) — carries the
            // `SVC` host boundary, so it produces `Halt::Exit` directly.
            self.branch_system(inst, pc, next)
        } else if inst & 0x0a00_0000 == 0x0800_0000 {
            // Loads and Stores (op0 = x1x0).
            self.loads_stores(inst, next).map_err(Halt::Trap)
        } else if inst & 0x0e00_0000 == 0x0a00_0000 {
            // Data Processing -- Register (op0 = x101).
            self.dp_register(inst, next).map_err(Halt::Trap)
        } else {
            // Scalar FP / Advanced SIMD (op0 = x111).
            match self.simd_fp(inst) {
                Ok(()) => {
                    self.pc = next;
                    Ok(())
                }
                Err(t) => Err(Halt::Trap(t)),
            }
        }
    }

    /// Scalar Floating-Point and Advanced SIMD (Arm-ARM C4.1.6). The PC is *not*
    /// advanced here — the caller advances on `Ok(())`.
    ///
    /// This is the SIMD&FP execution unit the integer-userspace ecosystem needs:
    /// AArch64 mandates Advanced SIMD (there is no "no-NEON" AArch64 glibc), so a
    /// stock `linux-arm64` binary's baseline `memcpy`/`memset`/`strlen`/… run
    /// NEON unconditionally. Dispatch follows the C4.1.6 family tables (vector
    /// forms have bit 31 = 0; the per-family masks fix the distinguishing bits).
    fn simd_fp(&mut self, inst: u32) -> Result<(), Trap> {
        // Scalar floating-point (FP compare / data-proc / convert / select /
        // immediate): bits[28:24] = 11110 with bits[30:29] = 00. bit 31 is `M`
        // (0) for the FP data-processing forms but `sf` for the FP↔integer
        // conversions (so it is *not* constrained here); bits[30:29] = 00 is what
        // separates scalar FP from the Advanced-SIMD *scalar* forms (bit 30 = 1).
        if (inst >> 24) & 0x1f == 0b1_1110 && (inst >> 29) & 0x3 == 0 {
            return self.fp_scalar(inst);
        }
        // Advanced SIMD (vector: bit 31 = 0; scalar: bits[31:30] = 01).
        if inst & 0x9f20_8400 == 0x0e00_0400 {
            self.asimd_copy(inst, false)
        } else if inst & 0xdf20_8400 == 0x5e00_0400 {
            self.asimd_copy(inst, true)
        } else if inst & 0x9f3e_0c00 == 0x0e20_0800 {
            self.asimd_two_reg_misc(inst, false)
        } else if inst & 0xdf3e_0c00 == 0x5e20_0800 {
            self.asimd_two_reg_misc(inst, true)
        } else if inst & 0x9f3e_0c00 == 0x0e30_0800 {
            self.asimd_across_lanes(inst)
        } else if inst & 0xdf3e_0c00 == 0x5e30_0800 {
            self.asimd_scalar_pairwise(inst)
        } else if inst & 0x9f20_0c00 == 0x0e20_0000 {
            self.asimd_three_different(inst)
        } else if inst & 0x9f20_0400 == 0x0e20_0400 {
            self.asimd_three_same(inst, false)
        } else if inst & 0xdf20_0400 == 0x5e20_0400 {
            self.asimd_three_same(inst, true)
        } else if inst & 0xbf20_8c00 == 0x0e00_0000 {
            self.asimd_tbl(inst)
        } else if inst & 0xbf20_8c00 == 0x0e00_0800 {
            self.asimd_permute(inst)
        } else if inst & 0xbf20_8400 == 0x2e00_0000 {
            self.asimd_ext(inst)
        } else if inst & 0x9f80_0400 == 0x0f00_0400 {
            // immh==0 → modified immediate; immh!=0 → shift by immediate.
            if (inst >> 19) & 0xf == 0 {
                self.asimd_modified_imm(inst)
            } else {
                self.asimd_shift_imm(inst, false)
            }
        } else if inst & 0xdf80_0400 == 0x5f00_0400 && (inst >> 19) & 0xf != 0 {
            self.asimd_shift_imm(inst, true)
        } else if inst & 0x9f00_0400 == 0x0f00_0000 {
            self.asimd_indexed(inst, false)
        } else if inst & 0xdf00_0400 == 0x5f00_0000 {
            self.asimd_indexed(inst, true)
        } else {
            Err(Trap::Illegal(inst))
        }
    }

    // ── Advanced SIMD: register-file access (little-endian lane layout) ──────

    /// Write `val` to `Vd`, zeroing the upper 64 bits when the form is 64-bit
    /// (`Q == 0`) — every AArch64 SIMD writer clears the unused top half.
    fn write_vreg(&mut self, rd: u32, val: u128, q: bool) {
        self.v[rd as usize] = if q { val } else { val & (u64::MAX as u128) };
    }

    /// AdvSIMD copy: `DUP` (element/general), `INS` (element/general), `UMOV`,
    /// `SMOV` — the lane↔general and lane-broadcast moves glibc's string routines
    /// use (`DUP Vd.16b,Wn` for `memset`; `UMOV`/`INS` for tail handling).
    fn asimd_copy(&mut self, inst: u32, scalar: bool) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let op = inst & (1 << 29) != 0;
        let imm5 = (inst >> 16) & 0x1f;
        let imm4 = (inst >> 11) & 0xf;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        // `size` = position of the lowest set bit of imm5 (B=0,H=1,S=2,D=3).
        let size = imm5.trailing_zeros();
        if size > 3 || imm5 == 0 {
            return Err(Trap::Illegal(inst));
        }
        let esize = 8u32 << size;
        let idx = (imm5 >> (size + 1)) as usize; // element index within the source
        if scalar {
            // Scalar DUP (DUP Dd, Vn.D[i]) — extract one element into a scalar V.
            let v = lane(self.v[rn as usize], size, idx);
            self.write_vreg(rd, v as u128, false);
            return Ok(());
        }
        if !op && imm4 == 0b0000 {
            // DUP (element): broadcast Vn.T[idx] across Vd.
            let elt = lane(self.v[rn as usize], size, idx);
            let mut out = 0u128;
            let lanes = (if q { 128 } else { 64 }) / esize;
            for i in 0..lanes as usize {
                set_lane(&mut out, size, i, elt);
            }
            self.write_vreg(rd, out, q);
            Ok(())
        } else if !op && imm4 == 0b0001 {
            // DUP (general): broadcast Xn/Wn across Vd.
            let elt = self.rx(rn) & mask_bits(esize);
            let mut out = 0u128;
            let lanes = (if q { 128 } else { 64 }) / esize;
            for i in 0..lanes as usize {
                set_lane(&mut out, size, i, elt);
            }
            self.write_vreg(rd, out, q);
            Ok(())
        } else if !op && imm4 == 0b0101 {
            // SMOV: sign-extend Vn.T[idx] into Xd/Wd.
            let elt = lane(self.v[rn as usize], size, idx);
            let val = sign_extend(elt, esize);
            // Q selects 64-bit (X) vs 32-bit (W) destination.
            self.wx_sz(rd, val, q);
            Ok(())
        } else if !op && imm4 == 0b0111 {
            // UMOV: zero-extend Vn.T[idx] into Xd/Wd (a.k.a. MOV Wd,Vn.S[i]).
            let elt = lane(self.v[rn as usize], size, idx);
            self.wx_sz(rd, elt, q);
            Ok(())
        } else if !op && imm4 == 0b0011 {
            // INS (general): Vd.Ts[idx] = Xn/Wn (op = 0, imm4 = 0011).
            let mut out = self.v[rd as usize];
            set_lane(&mut out, size, idx, self.rx(rn) & mask_bits(esize));
            self.v[rd as usize] = out;
            Ok(())
        } else if op {
            // INS (element): Vd.Ts[idx] = Vn.Ts[idx2] (op = 1).
            let idx2 = (imm4 >> size) as usize;
            let elt = lane(self.v[rn as usize], size, idx2);
            let mut out = self.v[rd as usize];
            set_lane(&mut out, size, idx, elt);
            self.v[rd as usize] = out;
            Ok(())
        } else {
            Err(Trap::Illegal(inst))
        }
    }

    /// AdvSIMD three-same (integer + the FP forms): the bulk of NEON compute —
    /// bitwise logic (`AND`/`ORR`/`EOR`/`BIC`/`ORN`/`BSL`/`BIT`/`BIF`), `ADD`/`SUB`,
    /// the comparisons (`CMEQ`/`CMGT`/`CMGE`/`CMHI`/`CMHS`/`CMTST`), `MIN`/`MAX`,
    /// `MUL`, pairwise (`ADDP`/`MAXP`/`MINP`), and the shifts (`USHL`/`SSHL`).
    fn asimd_three_same(&mut self, inst: u32, scalar: bool) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let u = inst & (1 << 29) != 0;
        let size = (inst >> 22) & 0x3;
        let opcode = (inst >> 11) & 0x1f;
        let rm = (inst >> 16) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let a = self.v[rn as usize];
        let b = self.v[rm as usize];

        // Logical ops (opcode 0b00011) select on size/U, not element size.
        if opcode == 0b00011 {
            let r = match (u, size) {
                (false, 0b00) => a & b,                                                 // AND
                (false, 0b01) => a & !b,                                                // BIC
                (false, 0b10) => a | b,  // ORR (MOV when Rn==Rm)
                (false, 0b11) => a | !b, // ORN
                (true, 0b00) => a ^ b,   // EOR
                (true, 0b01) => (self.v[rd as usize] & !b) | (a & b), // BSL
                (true, 0b10) => self.v[rd as usize] ^ ((a ^ self.v[rd as usize]) & b), // BIT
                (true, 0b11) => self.v[rd as usize] ^ ((a ^ self.v[rd as usize]) & !b), // BIF
                _ => unreachable!(),
            };
            self.write_vreg(rd, r, q);
            return Ok(());
        }

        // FP three-same (opcode 0b11x067 region, bit 11 set) — handled separately.
        if opcode >= 0b11000 {
            return self.asimd_three_same_fp(inst, scalar);
        }

        let esize = 8u32 << size;
        let elems = if scalar {
            1
        } else {
            (if q { 128 } else { 64 }) / esize
        } as usize;
        let mut out = 0u128;
        // Pairwise ops read the concatenation of Vn:Vm; handle them up front.
        let pairwise = matches!(opcode, 0b10111 | 0b10100 | 0b10101);
        if pairwise {
            let total = elems;
            for i in 0..total {
                let (x, y) = if i < total / 2 {
                    (lane(a, size, 2 * i), lane(a, size, 2 * i + 1))
                } else {
                    let j = i - total / 2;
                    (lane(b, size, 2 * j), lane(b, size, 2 * j + 1))
                };
                let r = match opcode {
                    0b10111 => x.wrapping_add(y) & mask_bits(esize), // ADDP
                    0b10100 => {
                        if u {
                            x.max(y)
                        } else {
                            smax(x, y, esize)
                        }
                    } // UMAXP/SMAXP
                    0b10101 => {
                        if u {
                            x.min(y)
                        } else {
                            smin(x, y, esize)
                        }
                    } // UMINP/SMINP
                    _ => unreachable!(),
                };
                set_lane(&mut out, size, i, r & mask_bits(esize));
            }
            self.write_vreg(rd, out, q);
            return Ok(());
        }
        for i in 0..elems {
            let x = lane(a, size, i);
            let y = lane(b, size, i);
            let r: u64 = match opcode {
                0b10000 => {
                    if u {
                        x.wrapping_sub(y)
                    } else {
                        x.wrapping_add(y)
                    }
                } // SUB / ADD
                0b10001 => {
                    if u {
                        bool_lane(x & mask_bits(esize) == y & mask_bits(esize))
                    } else {
                        bool_lane(x & y & mask_bits(esize) != 0)
                    }
                } // CMEQ / CMTST
                0b00110 => {
                    if u {
                        bool_lane(x & mask_bits(esize) > y & mask_bits(esize))
                    } else {
                        bool_lane(scmp(x, y, esize) == core::cmp::Ordering::Greater)
                    }
                } // CMHI / CMGT
                0b00111 => {
                    if u {
                        bool_lane(x & mask_bits(esize) >= y & mask_bits(esize))
                    } else {
                        bool_lane(scmp(x, y, esize) != core::cmp::Ordering::Less)
                    }
                } // CMHS / CMGE
                0b01100 => {
                    if u {
                        (x & mask_bits(esize)).max(y & mask_bits(esize))
                    } else {
                        smax(x, y, esize)
                    }
                } // UMAX / SMAX
                0b01101 => {
                    if u {
                        (x & mask_bits(esize)).min(y & mask_bits(esize))
                    } else {
                        smin(x, y, esize)
                    }
                } // UMIN / SMIN
                0b01110 => {
                    if u {
                        (x & mask_bits(esize)).abs_diff(y & mask_bits(esize))
                    } else {
                        sabd(x, y, esize)
                    }
                } // UABD / SABD
                0b10011 => {
                    if u {
                        pmul(x, y, esize)
                    } else {
                        x.wrapping_mul(y)
                    }
                } // PMUL / MUL
                0b10010 => {
                    let p = x.wrapping_mul(y);
                    let acc = lane(self.v[rd as usize], size, i);
                    if u {
                        acc.wrapping_sub(p)
                    } else {
                        acc.wrapping_add(p)
                    }
                } // MLS / MLA
                0b01000 => ushl_sshl(x, y, esize, u), // USHL / SSHL
                0b00000 => {
                    // SHADD / UHADD (halving add)
                    if u {
                        ((x & mask_bits(esize)) + (y & mask_bits(esize))) >> 1
                    } else {
                        let xs = sign_extend(x, esize) as i64;
                        let ys = sign_extend(y, esize) as i64;
                        ((xs + ys) >> 1) as u64
                    }
                }
                0b00001 => uqadd_sqadd(x, y, esize, u), // UQADD / SQADD
                0b00101 => uqsub_sqsub(x, y, esize, u), // UQSUB / SQSUB
                _ => return Err(Trap::Illegal(inst)),
            };
            set_lane(&mut out, size, i, r & mask_bits(esize));
        }
        self.write_vreg(rd, out, q);
        Ok(())
    }

    /// AdvSIMD three-same, floating-point forms (`FADD`/`FSUB`/`FMUL`/`FDIV`,
    /// `FMAX`/`FMIN`, `FCMEQ`/`FCMGE`/`FCMGT`, `FMLA`/`FMLS`, `FABD`).
    fn asimd_three_same_fp(&mut self, inst: u32, scalar: bool) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let u = inst & (1 << 29) != 0;
        let sz = (inst >> 22) & 1; // 0 = f32, 1 = f64
        let opcode = (inst >> 11) & 0x1f;
        let rm = (inst >> 16) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let ebytes: u32 = if sz == 1 { 8 } else { 4 };
        let size = if sz == 1 { 3 } else { 2 };
        let elems = if scalar {
            1
        } else {
            (if q { 128 } else { 64 }) / (ebytes * 8)
        } as usize;
        let a = self.v[rn as usize];
        let b = self.v[rm as usize];
        let mut out = 0u128;
        for i in 0..elems {
            let x = lane(a, size, i);
            let y = lane(b, size, i);
            let o1 = (inst >> 23) & 1 == 1;
            let r = fp3(
                opcode,
                u,
                o1,
                sz == 1,
                x,
                y,
                lane(self.v[rd as usize], size, i),
            );
            let Some(bits) = r else {
                return Err(Trap::Illegal(inst));
            };
            set_lane(&mut out, size, i, bits);
        }
        self.write_vreg(rd, out, q);
        Ok(())
    }

    /// AdvSIMD two-register miscellaneous: `REV*`, `CLZ`/`CLS`, `CNT`, `NOT`,
    /// `RBIT`, the compare-to-zero forms, `ABS`/`NEG`, `XTN`, the pairwise-long
    /// adds — the lane-reshaping ops glibc's `strlen`/`memchr` use.
    fn asimd_two_reg_misc(&mut self, inst: u32, scalar: bool) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let u = inst & (1 << 29) != 0;
        let size = (inst >> 22) & 0x3;
        let opcode = (inst >> 12) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let esize = 8u32 << size;
        let datasize = if q { 128 } else { 64 };
        let a = self.v[rn as usize];

        // REV64/REV32/REV16: reverse the `esize`-byte chunks within each
        // container (64/32/16-bit). `size` is the chunk (sub-element) size.
        if opcode == 0b00000 || opcode == 0b00001 {
            let container: u32 = match (opcode, u) {
                (0b00000, false) => 64, // REV64
                (0b00000, true) => 32,  // REV32
                (0b00001, false) => 16, // REV16
                _ => return Err(Trap::Illegal(inst)),
            };
            let mut out = 0u128;
            let cbytes = (container / esize) as usize; // chunks per container
            let containers = (datasize / container) as usize;
            for c in 0..containers {
                for k in 0..cbytes {
                    let src = lane(a, size, c * cbytes + k);
                    set_lane(&mut out, size, c * cbytes + (cbytes - 1 - k), src);
                }
            }
            self.write_vreg(rd, out, q);
            return Ok(());
        }

        // Narrowing XTN/XTN2 (and the saturating SQXTN/UQXTN, treated as XTN for
        // the values glibc produces): write `esize`-byte halves of `2*esize`
        // source elements. XTN2 (Q) fills the high half, preserving the low.
        if opcode == 0b10010 || opcode == 0b10100 {
            let ssize = size + 1;
            let selems = (64 / esize) as usize; // 64-bit worth of narrowed elements
            let mut out = if q { self.v[rd as usize] } else { 0 };
            for i in 0..selems {
                let s = lane(a, ssize, i) & mask(esize);
                set_lane(&mut out, size, if q { selems + i } else { i }, s);
            }
            self.v[rd as usize] = if q { out } else { out & (u64::MAX as u128) };
            return Ok(());
        }

        // ── Floating-point two-register misc (the `a == size<1>` set) ────────
        // FCVT{N,M,P,Z,A}{S,U}, SCVTF/UCVTF, FABS/FNEG, FSQRT, FRINT*, and the
        // FP compare-with-zero forms — busybox/glibc reach these (e.g. `sleep`'s
        // `fcvtzs d, d` parsing a duration). The FP element is single (sz=0) or
        // double (sz=1); `a` (size<1>) selects the op subgroup, opcode + U the op.
        let a_bit = size & 0b10 != 0;
        let fp_opcode = matches!(
            (a_bit, opcode),
            (false, 0b11010..=0b11101) | (true, 0b01111 | 0b11010 | 0b11011)
        );
        if fp_opcode {
            let ftype = if size & 1 == 1 { 0b01u32 } else { 0b00u32 };
            let fsz = if ftype == 0b01 { 3u32 } else { 2 }; // log2 element bytes
            let fesize = if ftype == 0b01 { 64u32 } else { 32 };
            let is64 = ftype == 0b01;
            let felems = if scalar {
                1
            } else {
                (datasize / fesize) as usize
            };
            let mut out = 0u128;
            for i in 0..felems {
                let xbits = lane(a, fsz, i);
                let f = if is64 {
                    f64::from_bits(xbits)
                } else {
                    f32::from_bits(xbits as u32) as f64
                };
                let r: u64 = match (a_bit, opcode, u) {
                    // FCVT{N,M,A,P,Z}{S,U}: float → integer with a rounding mode.
                    (false, 0b11010, false) => fp_to_int(f, Round::Nearest, true, is64), // FCVTNS
                    (false, 0b11010, true) => fp_to_int(f, Round::Nearest, false, is64), // FCVTNU
                    (false, 0b11011, false) => fp_to_int(f, Round::Minus, true, is64),   // FCVTMS
                    (false, 0b11011, true) => fp_to_int(f, Round::Minus, false, is64),   // FCVTMU
                    (false, 0b11100, false) => fp_to_int(f, Round::Nearest, true, is64), // FCVTAS (ties-away ≈ nearest here)
                    (false, 0b11100, true) => fp_to_int(f, Round::Nearest, false, is64), // FCVTAU
                    (true, 0b11010, false) => fp_to_int(f, Round::Plus, true, is64),     // FCVTPS
                    (true, 0b11010, true) => fp_to_int(f, Round::Plus, false, is64),     // FCVTPU
                    (true, 0b11011, false) => fp_to_int(f, Round::Zero, true, is64),     // FCVTZS
                    (true, 0b11011, true) => fp_to_int(f, Round::Zero, false, is64),     // FCVTZU
                    // SCVTF / UCVTF: integer → float (same lane width).
                    (false, 0b11101, false) => fp_bits(
                        ftype,
                        if is64 {
                            xbits as i64 as f64
                        } else {
                            xbits as i32 as f64
                        },
                    ) as u64,
                    (false, 0b11101, true) => fp_bits(
                        ftype,
                        if is64 {
                            xbits as f64
                        } else {
                            (xbits as u32) as f64
                        },
                    ) as u64,
                    // FABS / FNEG.
                    (true, 0b01111, false) => fp_bits(ftype, f.abs()) as u64,
                    (true, 0b01111, true) => fp_bits(ftype, -f) as u64,
                    _ => return Err(Trap::Illegal(inst)),
                };
                set_lane(&mut out, fsz, i, r);
            }
            self.write_vreg(rd, out, q);
            return Ok(());
        }

        let elems = if scalar {
            1
        } else {
            (datasize / esize) as usize
        };
        let mut out = 0u128;
        for i in 0..elems {
            let x = lane(a, size, i);
            let r: u64 = match (opcode, u) {
                (0b00100, false) => clsz(x, esize, false), // CLS
                (0b00100, true) => clsz(x, esize, true),   // CLZ
                (0b00101, false) => (x & mask(esize)).count_ones() as u64, // CNT
                (0b00101, true) if size == 0 => !x,        // NOT (MVN)
                (0b00101, true) if size == 1 => rbit(x, esize), // RBIT
                (0b01000, false) => bool_lane(scmp(x, 0, esize) == core::cmp::Ordering::Greater), // CMGT #0
                (0b01000, true) => bool_lane(scmp(x, 0, esize) != core::cmp::Ordering::Less), // CMGE #0
                (0b01001, false) => bool_lane(x & mask(esize) == 0), // CMEQ #0
                (0b01001, true) => bool_lane(scmp(x, 0, esize) != core::cmp::Ordering::Greater), // CMLE #0
                (0b01010, false) => bool_lane(scmp(x, 0, esize) == core::cmp::Ordering::Less), // CMLT #0
                (0b01011, false) => (sign_extend(x, esize) as i64).unsigned_abs(), // ABS
                (0b01011, true) => 0u64.wrapping_sub(x),                           // NEG
                _ => return Err(Trap::Illegal(inst)),
            };
            set_lane(&mut out, size, i, r & mask(esize));
        }
        self.write_vreg(rd, out, q);
        Ok(())
    }

    /// AdvSIMD across lanes: `ADDV`, `UMAXV`/`UMINV`/`SMAXV`/`SMINV`,
    /// `UADDLV`/`SADDLV` — the horizontal reductions that collapse a vector to a
    /// single element (glibc's `strlen`/`memchr` use them to test a whole chunk).
    fn asimd_across_lanes(&mut self, inst: u32) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let u = inst & (1 << 29) != 0;
        let size = (inst >> 22) & 0x3;
        let opcode = (inst >> 12) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let esize = 8u32 << size;
        let a = self.v[rn as usize];
        let elems = ((if q { 128 } else { 64 }) / esize) as usize;
        // Across-lanes opcodes: 00011 SADDLV/UADDLV, 01010 SMAXV/UMAXV,
        // 11010 SMINV/UMINV, 11011 ADDV.
        let mut acc = match opcode {
            0b11011 | 0b00011 => 0u64,
            _ => lane(a, size, 0),
        };
        for i in 0..elems {
            let x = lane(a, size, i);
            match opcode {
                0b11011 => acc = acc.wrapping_add(x), // ADDV
                0b00011 => {
                    acc = acc.wrapping_add(if u {
                        x & mask(esize)
                    } else {
                        sign_extend(x, esize)
                    });
                } // UADDLV / SADDLV
                0b01010 => {
                    acc = if u {
                        acc.max(x & mask(esize))
                    } else if scmp(x, acc, esize) == core::cmp::Ordering::Greater {
                        x
                    } else {
                        acc
                    };
                } // UMAXV / SMAXV
                0b11010 => {
                    acc = if u {
                        acc.min(x & mask(esize))
                    } else if scmp(x, acc, esize) == core::cmp::Ordering::Less {
                        x
                    } else {
                        acc
                    };
                } // UMINV / SMINV
                _ => return Err(Trap::Illegal(inst)),
            }
        }
        // The destination element is `esize` (or 2*esize for the long add).
        let dsize = if opcode == 0b00011 { size + 1 } else { size };
        let mut out = 0u128;
        set_lane(&mut out, dsize, 0, acc & mask(8u32 << dsize));
        self.write_vreg(rd, out, false);
        Ok(())
    }

    /// AdvSIMD scalar pairwise (`ADDP Dd, Vn.2D` and friends) — the final
    /// reduction step a few routines use.
    fn asimd_scalar_pairwise(&mut self, inst: u32) -> Result<(), Trap> {
        let size = (inst >> 22) & 0x3;
        let opcode = (inst >> 12) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let a = self.v[rn as usize];
        if opcode == 0b11011 {
            // ADDP (scalar): Dd = Vn.D[0] + Vn.D[1]
            let r = lane(a, size, 0).wrapping_add(lane(a, size, 1));
            let mut out = 0u128;
            set_lane(&mut out, size, 0, r & mask(8u32 << size));
            self.write_vreg(rd, out, false);
            Ok(())
        } else {
            Err(Trap::Illegal(inst))
        }
    }

    /// AdvSIMD three-different (widening): `SMULL`/`UMULL`, `SMLAL`/`UMLAL`,
    /// `SADDL`/`UADDL`, `SSUBL`/`USUBL`, `ADDHN`/`SUBHN` — the long-arithmetic
    /// forms.
    fn asimd_three_different(&mut self, inst: u32) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let u = inst & (1 << 29) != 0;
        let size = (inst >> 22) & 0x3;
        let opcode = (inst >> 12) & 0xf;
        let rm = (inst >> 16) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let esize = 8u32 << size;
        let dsize = size + 1;
        let part = if q { 1 } else { 0 }; // _2 forms read the high half
        let a = self.v[rn as usize];
        let b = self.v[rm as usize];
        let delems = (128 / (esize * 2)) as usize;
        let mut out = 0u128;
        for i in 0..delems {
            let xi = part * delems + i;
            let x = lane(a, size, xi);
            let y = lane(b, size, xi);
            let r: u64 = match opcode {
                0b1100 => {
                    if u {
                        (x & mask(esize)).wrapping_mul(y & mask(esize))
                    } else {
                        (sign_extend(x, esize)).wrapping_mul(sign_extend(y, esize))
                    }
                } // UMULL / SMULL
                0b1000 | 0b1010 => {
                    let p = if u {
                        (x & mask(esize)).wrapping_mul(y & mask(esize))
                    } else {
                        (sign_extend(x, esize)).wrapping_mul(sign_extend(y, esize))
                    };
                    let acc = lane(self.v[rd as usize], dsize, i);
                    if opcode == 0b1000 {
                        acc.wrapping_add(p)
                    } else {
                        acc.wrapping_sub(p)
                    }
                } // UMLAL/SMLAL (1000) UMLSL/SMLSL (1010)
                0b0000 => {
                    if u {
                        (x & mask(esize)).wrapping_add(y & mask(esize))
                    } else {
                        sign_extend(x, esize).wrapping_add(sign_extend(y, esize))
                    }
                } // UADDL / SADDL
                0b0010 => {
                    if u {
                        (x & mask(esize)).wrapping_sub(y & mask(esize))
                    } else {
                        sign_extend(x, esize).wrapping_sub(sign_extend(y, esize))
                    }
                } // USUBL / SSUBL
                _ => return Err(Trap::Illegal(inst)),
            };
            set_lane(&mut out, dsize, i, r & mask(8u32 << dsize));
        }
        self.write_vreg(rd, out, true);
        Ok(())
    }

    /// AdvSIMD table lookup: `TBL`/`TBX` — gather bytes from a 1–4 register table
    /// by an index vector (used by some `memchr`/charset routines).
    fn asimd_tbl(&mut self, inst: u32) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let len = ((inst >> 13) & 0x3) as usize + 1; // 1..4 table registers
        let is_tbx = inst & (1 << 12) != 0;
        let rm = (inst >> 16) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let idx = self.v[rm as usize];
        let bytes = if q { 16 } else { 8 };
        let mut out = if is_tbx { self.v[rd as usize] } else { 0 };
        for i in 0..bytes {
            let sel = ((idx >> (i * 8)) & 0xff) as usize;
            if sel < len * 16 {
                let treg = (rn as usize + sel / 16) % 32;
                let b = (self.v[treg] >> ((sel % 16) * 8)) & 0xff;
                out = (out & !(0xffu128 << (i * 8))) | (b << (i * 8));
            } else if !is_tbx {
                out &= !(0xffu128 << (i * 8)); // TBL: out-of-range → 0
            }
        }
        self.write_vreg(rd, out, q);
        Ok(())
    }

    /// AdvSIMD permute: `ZIP1`/`ZIP2`, `UZP1`/`UZP2`, `TRN1`/`TRN2`.
    fn asimd_permute(&mut self, inst: u32) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let size = (inst >> 22) & 0x3;
        let opcode = (inst >> 12) & 0x7;
        let rm = (inst >> 16) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let a = self.v[rn as usize];
        let b = self.v[rm as usize];
        let elems = ((if q { 128 } else { 64 }) / (8u32 << size)) as usize;
        let mut out = 0u128;
        let half = elems / 2;
        for i in 0..elems {
            let v = match opcode {
                0b011 | 0b111 => {
                    // ZIP1 (011) / ZIP2 (111)
                    let base = if opcode == 0b111 { half } else { 0 };
                    let p = i / 2 + base;
                    if i % 2 == 0 {
                        lane(a, size, p)
                    } else {
                        lane(b, size, p)
                    }
                }
                0b001 | 0b101 => {
                    // UZP1 (001) / UZP2 (101)
                    let odd = if opcode == 0b101 { 1 } else { 0 };
                    let src = 2 * i + odd;
                    if src < elems {
                        lane(a, size, src)
                    } else {
                        lane(b, size, src - elems)
                    }
                }
                0b010 | 0b110 => {
                    // TRN1 (010) / TRN2 (110)
                    let off = if opcode == 0b110 { 1 } else { 0 };
                    let p = (i & !1) + off;
                    if i % 2 == 0 {
                        lane(a, size, p)
                    } else {
                        lane(b, size, p)
                    }
                }
                _ => return Err(Trap::Illegal(inst)),
            };
            set_lane(&mut out, size, i, v);
        }
        self.write_vreg(rd, out, q);
        Ok(())
    }

    /// AdvSIMD extract: `EXT Vd, Vn, Vm, #imm` — byte-granular concatenation, the
    /// misalignment primitive glibc's `memcpy`/`strcmp` tails use.
    fn asimd_ext(&mut self, inst: u32) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let rm = (inst >> 16) & 0x1f;
        let imm4 = (inst >> 11) & 0xf;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let bytes = if q { 16 } else { 8 };
        let pos = imm4 as usize;
        if pos >= bytes {
            return Err(Trap::Illegal(inst));
        }
        let a = self.v[rn as usize];
        let b = self.v[rm as usize];
        let mut out = 0u128;
        for i in 0..bytes {
            let src = pos + i;
            let byte = if src < bytes {
                (a >> (src * 8)) & 0xff
            } else {
                (b >> ((src - bytes) * 8)) & 0xff
            };
            out |= byte << (i * 8);
        }
        self.write_vreg(rd, out, q);
        Ok(())
    }

    /// AdvSIMD modified immediate: `MOVI`/`MVNI`, the immediate `ORR`/`BIC`, and
    /// `FMOV` (vector immediate) — `memset` builds its fill byte with `MOVI`.
    fn asimd_modified_imm(&mut self, inst: u32) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let op = inst & (1 << 29) != 0;
        let cmode = (inst >> 12) & 0xf;
        let abc = (inst >> 16) & 0x7;
        let defgh = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let imm8 = (abc << 5) | defgh;
        let (imm, is_mvni, is_logical) = adv_simd_expand_imm(cmode, op, imm8 as u8);
        let val = if q { imm } else { imm & (u64::MAX as u128) };
        let r = if is_logical {
            // ORR/BIC immediate accumulate into Vd.
            let cur = self.v[rd as usize];
            if op {
                cur & !val // BIC
            } else {
                cur | val // ORR
            }
        } else if is_mvni {
            !val
        } else {
            val
        };
        self.write_vreg(rd, r, q);
        Ok(())
    }

    /// AdvSIMD shift by immediate: `SHL`, `USHR`/`SSHR`, `SHRN`, `SSHLL`/`USHLL`,
    /// `SLI`/`SRI` — including the `SHRN v.8b, v.8h, #4` nibble-narrowing that
    /// glibc's `strlen` uses to compress a 16-byte compare to a 64-bit word.
    fn asimd_shift_imm(&mut self, inst: u32, scalar: bool) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let u = inst & (1 << 29) != 0;
        let immh = (inst >> 19) & 0xf;
        let immb = (inst >> 16) & 0x7;
        let opcode = (inst >> 11) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        // size = highest set bit position of immh (B/H/S/D), shift derived per form.
        let size = (31 - immh.leading_zeros()).min(3);
        let esize = 8u32 << size;
        let imm = (immh << 3) | immb;
        let a = self.v[rn as usize];

        // Narrowing forms (SHRN/RSHRN): dest element is half, source is 2*esize.
        if opcode == 0b10000 {
            let shift = 2 * esize - imm;
            let dsize = size; // narrow output element
            let ssize = size + 1;
            let delems = (128 / (esize * 2)) as usize;
            let mut out = if q { self.v[rd as usize] } else { 0 };
            for i in 0..delems {
                let s = lane(a, ssize, i);
                let r = (s >> shift) & mask(esize);
                set_lane(&mut out, dsize, if q { delems + i } else { i }, r);
            }
            self.write_vreg(rd, out, true);
            if !q {
                self.write_vreg(rd, out & (u64::MAX as u128), false);
            }
            return Ok(());
        }

        // Widening forms (SSHLL/USHLL, a.k.a. SXTL/UXTL when shift==0).
        if opcode == 0b10100 {
            let shift = imm - esize;
            let dsize = size + 1;
            let delems = (128 / (esize * 2)) as usize;
            let part = if q { delems } else { 0 };
            let mut out = 0u128;
            for i in 0..delems {
                let s = lane(a, size, part + i);
                let ext = if u {
                    s & mask(esize)
                } else {
                    sign_extend(s, esize)
                };
                set_lane(
                    &mut out,
                    dsize,
                    i,
                    ext.wrapping_shl(shift) & mask(8u32 << dsize),
                );
            }
            self.write_vreg(rd, out, true);
            return Ok(());
        }

        let elems = if scalar {
            1
        } else {
            (if q { 128 } else { 64 }) / esize
        } as usize;
        // SLI/SRI insert into the existing destination; the others overwrite.
        let insert = matches!(opcode, 0b01000) || (opcode == 0b01010 && u);
        let mut out = if insert { self.v[rd as usize] } else { 0 };
        let right = 2 * esize - imm; // right-shift amount (for the SHR family)
        let left = imm - esize; // left-shift amount (for SHL/SLI)
        for i in 0..elems {
            let x = lane(a, size, i);
            let em = mask(esize);
            let val: u64 = match (opcode, u) {
                (0b00000, true) => (x & em) >> right, // USHR
                (0b00000, false) => ((sign_extend(x, esize) as i64) >> right) as u64, // SSHR
                (0b00010, _) => {
                    let shifted = if u {
                        (x & em) >> right
                    } else {
                        ((sign_extend(x, esize) as i64) >> right) as u64
                    };
                    lane(self.v[rd as usize], size, i).wrapping_add(shifted)
                } // USRA / SSRA
                (0b01010, false) => x.wrapping_shl(left), // SHL
                (0b01010, true) => {
                    // SLI: shift left, insert (keep the low `left` bits of Vd).
                    let keep = mask(left);
                    (lane(self.v[rd as usize], size, i) & keep) | ((x << left) & em)
                }
                (0b01000, true) => {
                    // SRI: shift right, insert (keep the high `right` bits of Vd).
                    let shifted = (x & em) >> right;
                    let keep = if right == 0 { 0 } else { (em << right) & em };
                    (lane(self.v[rd as usize], size, i) & keep) | shifted
                }
                _ => return Err(Trap::Illegal(inst)),
            };
            set_lane(&mut out, size, i, val & em);
        }
        self.write_vreg(rd, out, q);
        Ok(())
    }

    /// AdvSIMD vector × indexed element: `MUL`/`MLA`/`MLS` and the `FMUL`/`FMLA`
    /// by-element forms.
    fn asimd_indexed(&mut self, inst: u32, _scalar: bool) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let size = (inst >> 22) & 0x3;
        let opcode = (inst >> 12) & 0xf;
        let l = (inst >> 21) & 1;
        let m = (inst >> 20) & 1;
        let rmlo = (inst >> 16) & 0xf;
        let h = (inst >> 11) & 1;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        let esize = 8u32 << size;
        // Integer MUL/MLA/MLS by element (opcode 1000=MLA,0100=MLS,1000? ; 1000? ).
        let (rm, index) = match size {
            0b01 => (rmlo, (h << 2) | (l << 1) | m), // H elements: Vm in 0..15
            0b10 => ((m << 4) | rmlo, (h << 1) | l), // S elements
            _ => return Err(Trap::Illegal(inst)),
        };
        let velt = lane(self.v[rm as usize], size, index as usize);
        let a = self.v[rn as usize];
        let elems = ((if q { 128 } else { 64 }) / esize) as usize;
        let mut out = 0u128;
        for i in 0..elems {
            let x = lane(a, size, i);
            let p = x.wrapping_mul(velt);
            let r = match opcode {
                0b1000 => p,                                                  // MUL
                0b0000 => lane(self.v[rd as usize], size, i).wrapping_add(p), // MLA
                0b0100 => lane(self.v[rd as usize], size, i).wrapping_sub(p), // MLS
                _ => return Err(Trap::Illegal(inst)),
            };
            set_lane(&mut out, size, i, r & mask(esize));
        }
        self.write_vreg(rd, out, q);
        Ok(())
    }

    // ── Scalar floating-point ───────────────────────────────────────────────

    /// Scalar floating-point (Arm-ARM C4.1.6): `FMOV` (register/immediate and
    /// the general↔SIMD moves `strlen` uses), `FCVT`, the data-processing ops
    /// (`FADD`/`FSUB`/`FMUL`/`FDIV`/`FABS`/`FNEG`/`FMAX`/`FMIN`), `FCMP`, `FCSEL`,
    /// and the integer/fixed-point conversions (`SCVTF`/`UCVTF`/`FCVTZS`/…).
    fn fp_scalar(&mut self, inst: u32) -> Result<(), Trap> {
        let ftype = (inst >> 22) & 0x3;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        if (inst >> 21) & 1 == 0 {
            // FP↔fixed-point conversion (scale in bits[15:10]).
            return self.fp_fixed_convert(inst);
        }
        let lo = (inst >> 10) & 0x3f;
        if lo == 0b000000 {
            return self.fp_int_convert(inst);
        }
        if lo & 0x1f == 0b10000 {
            // FP data-processing (1 source).
            let opcode = (inst >> 15) & 0x3f;
            let x = self.v[rn as usize];
            let r: u128 = match (ftype, opcode) {
                (_, 0b000000) => fp_bits(ftype, self.fval(rn, ftype)), // FMOV (register)
                (_, 0b000001) => fp_map(ftype, x, |v| v.abs()),        // FABS
                (_, 0b000010) => fp_map(ftype, x, |v| -v),             // FNEG
                (0b00, 0b000101) => fp_bits(0b01, self.fval(rn, 0b00)), // FCVT S→D
                (0b01, 0b000100) => fp_bits(0b00, self.fval(rn, 0b01)), // FCVT D→S
                (_, 0b001000) => fp_round(ftype, x, Round::Nearest),   // FRINTN
                (_, 0b001111) => fp_round(ftype, x, Round::Zero),      // FRINTZ ? (variant)
                _ => return Err(Trap::Illegal(inst)),
            };
            self.v[rd as usize] = r;
            return Ok(());
        }
        match lo & 0x3 {
            0b10 => {
                // FP data-processing (2 source).
                let opcode = (inst >> 12) & 0xf;
                let rm = (inst >> 16) & 0x1f;
                let a = self.fval(rn, ftype);
                let b = self.fval(rm, ftype);
                let r = match opcode {
                    0b0000 => a * b,
                    0b0001 => a / b,
                    0b0010 => a + b,
                    0b0011 => a - b,
                    0b0100 => a.max(b), // FMAX
                    0b0101 => a.min(b), // FMIN
                    0b0110 => a.max(b), // FMAXNM
                    0b0111 => a.min(b), // FMINNM
                    0b1000 => -(a * b), // FNMUL
                    _ => return Err(Trap::Illegal(inst)),
                };
                self.v[rd as usize] = fp_bits(ftype, r);
                Ok(())
            }
            0b00 if (lo >> 2) & 0x3 == 0b10 => {
                // FP compare (bits[13:10]=1000).
                let rm = (inst >> 16) & 0x1f;
                let opc = (inst >> 14) & 0x3;
                let a = self.fval(rn, ftype);
                let b = if opc & 0b10 != 0 {
                    0.0
                } else {
                    self.fval(rm, ftype)
                };
                self.flags = fp_compare(a, b);
                Ok(())
            }
            0b00 if (lo >> 2) & 0x1 == 1 => {
                // FP immediate (bits[12:10]=100).
                let imm8 = ((inst >> 13) & 0xff) as u8;
                self.v[rd as usize] = fp_bits(ftype, vfp_expand_imm(imm8, ftype));
                Ok(())
            }
            0b11 => {
                // FP conditional select (FCSEL).
                let rm = (inst >> 16) & 0x1f;
                let cond = (inst >> 12) & 0xf;
                let src = if self.cond_holds(cond) { rn } else { rm };
                self.v[rd as usize] = self.v[src as usize];
                Ok(())
            }
            _ => Err(Trap::Illegal(inst)),
        }
    }

    /// Read SIMD register `i` as an f64 (the value of its low element at `ftype`
    /// precision, widened to f64 for the host computation).
    fn fval(&self, i: u32, ftype: u32) -> f64 {
        match ftype {
            0b00 => f32::from_bits(self.v[i as usize] as u32) as f64,
            0b01 => f64::from_bits(self.v[i as usize] as u64),
            _ => f64::NAN,
        }
    }

    /// FP↔integer conversion + the general↔SIMD `FMOV`s.
    fn fp_int_convert(&mut self, inst: u32) -> Result<(), Trap> {
        let sf = inst & (1 << 31) != 0;
        let ftype = (inst >> 22) & 0x3;
        let rmode = (inst >> 19) & 0x3;
        let opcode = (inst >> 16) & 0x7;
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        match opcode {
            0b110 => {
                // FMOV to general register (Xd/Wd ← Vn).
                let v = if ftype == 0b01 {
                    self.v[rn as usize] as u64
                } else {
                    (self.v[rn as usize] as u32) as u64
                };
                self.wx_sz(rd, v, sf);
                Ok(())
            }
            0b111 => {
                // FMOV from general register (Vd ← Xn/Wn).
                let g = self.rx(rn);
                self.v[rd as usize] = if ftype == 0b01 {
                    g as u128
                } else {
                    (g as u32) as u128
                };
                Ok(())
            }
            0b010 | 0b011 => {
                // SCVTF (010) / UCVTF (011): integer → float.
                let g = self.rx(rn);
                let val = if opcode == 0b010 {
                    if sf {
                        g as i64 as f64
                    } else {
                        g as i32 as f64
                    }
                } else if sf {
                    g as f64
                } else {
                    (g as u32) as f64
                };
                self.v[rd as usize] = fp_bits(ftype, val);
                Ok(())
            }
            0b000 | 0b001 | 0b100 | 0b101 => {
                // FCVT{N,P,M,Z,A}{S,U}: float → integer with a rounding mode.
                let f = self.fval(rn, ftype);
                let round = match rmode {
                    0b00 => Round::Nearest,
                    0b01 => Round::Plus,
                    0b10 => Round::Minus,
                    _ => Round::Zero,
                };
                let signed = opcode & 1 == 0;
                let r = fp_to_int(f, round, signed, sf);
                self.wx_sz(rd, r, sf);
                Ok(())
            }
            _ => Err(Trap::Illegal(inst)),
        }
    }

    /// FP↔fixed-point conversion (`SCVTF`/`UCVTF`/`FCVTZS`/`FCVTZU` with a scale).
    fn fp_fixed_convert(&mut self, inst: u32) -> Result<(), Trap> {
        let sf = inst & (1 << 31) != 0;
        let ftype = (inst >> 22) & 0x3;
        let opcode = (inst >> 16) & 0x7;
        let scale = 64 - ((inst >> 10) & 0x3f);
        let rn = (inst >> 5) & 0x1f;
        let rd = inst & 0x1f;
        // 2^scale built from the IEEE-754 exponent (f64::powi is std-only and the
        // crate is no_std). `scale` is 1..=64, so the exponent never overflows.
        let factor = f64::from_bits((1023u64 + u64::from(scale)) << 52);
        match opcode {
            0b010 | 0b011 => {
                // SCVTF / UCVTF (fixed → float).
                let g = self.rx(rn);
                let base = if opcode == 0b010 {
                    if sf {
                        g as i64 as f64
                    } else {
                        g as i32 as f64
                    }
                } else if sf {
                    g as f64
                } else {
                    (g as u32) as f64
                };
                self.v[rd as usize] = fp_bits(ftype, base / factor);
                Ok(())
            }
            0b000 | 0b001 => {
                // FCVTZS / FCVTZU (float → fixed, round to zero).
                let f = self.fval(rn, ftype) * factor;
                let r = fp_to_int(f, Round::Zero, opcode == 0b000, sf);
                self.wx_sz(rd, r, sf);
                Ok(())
            }
            _ => Err(Trap::Illegal(inst)),
        }
    }

    // ── Data Processing -- Immediate ────────────────────────────────────────

    fn dp_immediate(&mut self, inst: u32, pc: u64, next: u64) -> Result<(), Trap> {
        let rd = inst & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let sf = inst & 0x8000_0000 != 0;
        if inst & 0x1f00_0000 == 0x1000_0000 {
            // PC-relative addressing: ADR / ADRP.
            let immlo = (inst >> 29) & 0x3;
            let immhi = (inst >> 5) & 0x7_ffff;
            let imm = ((immhi << 2) | immlo) as u64;
            if inst & 0x8000_0000 == 0 {
                // ADR: signed 21-bit byte offset from the instruction's PC.
                let off = sign_extend(imm, 21);
                self.wx(rd, pc.wrapping_add(off));
            } else {
                // ADRP: signed 21-bit offset of 4 KiB pages from the page of PC.
                let off = sign_extend(imm << 12, 33);
                self.wx(rd, (pc & !0xfff).wrapping_add(off));
            }
            self.pc = next;
            return Ok(());
        }
        match (inst >> 23) & 0x7 {
            0b010 => {
                // Add/subtract (immediate).
                let sh = (inst >> 22) & 1;
                let mut imm12 = ((inst >> 10) & 0xfff) as u64;
                if sh == 1 {
                    imm12 <<= 12;
                }
                let op = inst & 0x4000_0000 != 0; // 0=ADD, 1=SUB
                let setflags = inst & 0x2000_0000 != 0;
                let a = self.rx_sp(rn);
                let (res, f) = add_with_carry(a, if op { !imm12 } else { imm12 }, op, sf);
                if setflags {
                    self.flags = f;
                    self.wx_sz(rd, res, sf);
                } else {
                    self.wx_sp(rd, if sf { res } else { res & 0xffff_ffff });
                }
            }
            0b100 => {
                // Logical (immediate).
                let n = (inst >> 22) & 1;
                let immr = (inst >> 16) & 0x3f;
                let imms = (inst >> 10) & 0x3f;
                let opc = (inst >> 29) & 0x3;
                let datasize = if sf { 64 } else { 32 };
                let Some((imm, _)) = decode_bit_masks(n, imms, immr, datasize, true) else {
                    return Err(Trap::Illegal(inst));
                };
                let a = self.op(rn, sf);
                let res = match opc {
                    0b00 => a & imm, // AND
                    0b01 => a | imm, // ORR
                    0b10 => a ^ imm, // EOR
                    0b11 => a & imm, // ANDS
                    _ => unreachable!(),
                };
                let res = if sf { res } else { res & 0xffff_ffff };
                if opc == 0b11 {
                    self.flags = logical_flags(res, sf);
                    self.wx_sz(rd, res, sf);
                } else {
                    // ANDS targets XZR-slot; AND/ORR/EOR target SP-slot.
                    self.wx_sp(rd, res);
                }
            }
            0b101 => {
                // Move wide (immediate): MOVN / MOVZ / MOVK.
                let opc = (inst >> 29) & 0x3;
                let hw = (inst >> 21) & 0x3;
                let imm16 = ((inst >> 5) & 0xffff) as u64;
                if !sf && hw > 1 {
                    return Err(Trap::Illegal(inst)); // 32-bit: only hw 0/1 valid
                }
                let shift = hw * 16;
                let shifted = imm16 << shift;
                let res = match opc {
                    0b00 => !shifted,                                        // MOVN
                    0b10 => shifted,                                         // MOVZ
                    0b11 => (self.rx(rd) & !(0xffffu64 << shift)) | shifted, // MOVK
                    _ => return Err(Trap::Illegal(inst)),
                };
                self.wx_sz(rd, res, sf);
            }
            0b110 => {
                // Bitfield: SBFM / BFM / UBFM.
                let n = (inst >> 22) & 1;
                let immr = (inst >> 16) & 0x3f;
                let imms = (inst >> 10) & 0x3f;
                let opc = (inst >> 29) & 0x3;
                let datasize = if sf { 64 } else { 32 };
                // 32-bit requires N==0 and immr/imms<6:5>==0.
                if (sf && n == 0) || (!sf && n == 1) {
                    return Err(Trap::Illegal(inst));
                }
                let Some((wmask, tmask)) = decode_bit_masks(n, imms, immr, datasize, false) else {
                    return Err(Trap::Illegal(inst));
                };
                let (inzero, extend) = match opc {
                    0b00 => (true, true),   // SBFM
                    0b01 => (false, false), // BFM
                    0b10 => (true, false),  // UBFM
                    _ => return Err(Trap::Illegal(inst)),
                };
                let src = self.op(rn, sf);
                let dst = if inzero { 0 } else { self.op(rd, sf) };
                let bot = (dst & !wmask) | (ror(src, immr, datasize) & wmask);
                let sign_bit = (src >> imms) & 1;
                let top = if extend && sign_bit == 1 {
                    mask(datasize)
                } else if extend {
                    0
                } else {
                    dst
                };
                let res = (top & !tmask) | (bot & tmask);
                self.wx_sz(rd, res, sf);
            }
            0b111 => {
                // Extract: EXTR (and its ROR alias when Rn==Rm).
                let n = (inst >> 22) & 1;
                let imms = (inst >> 10) & 0x3f;
                let rm = (inst >> 16) & 0x1f;
                if (sf && n == 0) || (!sf && (n == 1 || imms & 0x20 != 0)) {
                    return Err(Trap::Illegal(inst));
                }
                let datasize = if sf { 64 } else { 32 };
                let hi = self.op(rn, sf);
                let lo = self.op(rm, sf);
                let res = if imms == 0 {
                    lo
                } else if datasize == 64 {
                    (lo >> imms) | (hi << (64 - imms))
                } else {
                    let concat = ((hi & 0xffff_ffff) << 32) | (lo & 0xffff_ffff);
                    (concat >> imms) & 0xffff_ffff
                };
                self.wx_sz(rd, res, sf);
            }
            _ => return Err(Trap::Illegal(inst)),
        }
        self.pc = next;
        Ok(())
    }

    // ── Branches, Exception generating, and System ──────────────────────────

    fn branch_system(&mut self, inst: u32, pc: u64, next: u64) -> Result<(), Halt> {
        if inst & 0x7c00_0000 == 0x1400_0000 {
            // Unconditional branch (immediate): B / BL.
            let imm = sign_extend((inst & 0x03ff_ffff) as u64, 26) << 2;
            if inst & 0x8000_0000 != 0 {
                self.x[30] = next; // BL sets the link register
            }
            self.pc = pc.wrapping_add(imm);
            return Ok(());
        }
        if inst & 0x7e00_0000 == 0x3400_0000 {
            // Compare and branch (immediate): CBZ / CBNZ.
            let sf = inst & 0x8000_0000 != 0;
            let rt = inst & 0x1f;
            let imm = sign_extend(((inst >> 5) & 0x7_ffff) as u64, 19) << 2;
            let v = self.op(rt, sf);
            let take = if inst & 0x0100_0000 != 0 {
                v != 0
            } else {
                v == 0
            };
            self.pc = if take { pc.wrapping_add(imm) } else { next };
            return Ok(());
        }
        if inst & 0x7e00_0000 == 0x3600_0000 {
            // Test and branch (immediate): TBZ / TBNZ.
            let rt = inst & 0x1f;
            let bit = ((inst >> 31) << 5) | ((inst >> 19) & 0x1f);
            let imm = sign_extend(((inst >> 5) & 0x3fff) as u64, 14) << 2;
            let set = (self.rx(rt) >> bit) & 1 == 1;
            let take = if inst & 0x0100_0000 != 0 { set } else { !set };
            self.pc = if take { pc.wrapping_add(imm) } else { next };
            return Ok(());
        }
        if inst & 0xfe00_0000 == 0x5400_0000 && inst & 0x10 == 0 {
            // Conditional branch (immediate): B.cond.
            let cond = inst & 0xf;
            let imm = sign_extend(((inst >> 5) & 0x7_ffff) as u64, 19) << 2;
            self.pc = if self.cond_holds(cond) {
                pc.wrapping_add(imm)
            } else {
                next
            };
            return Ok(());
        }
        if inst & 0xff00_0000 == 0xd400_0000 {
            // Exception generation: SVC / HVC / SMC / BRK / HLT.
            let imm16 = ((inst >> 5) & 0xffff) as u16;
            return match inst & 0x00e0_001f {
                0x0000_0001 => self.svc(next), // SVC #imm
                0x0000_0002 | 0x0000_0003 if self.sys.is_some() => {
                    // HVC / SMC — the PSCI firmware conduit (handled in-machine).
                    self.psci(next);
                    Ok(())
                }
                0x0020_0000 => Err(Halt::Trap(Trap::Breakpoint(imm16))), // BRK #imm
                0x0040_0000 => Err(Halt::Trap(Trap::Halted(imm16))),     // HLT #imm
                _ => Err(Halt::Trap(Trap::Illegal(inst))),
            };
        }
        if inst & 0xffc0_0000 == 0xd500_0000 {
            // System: hints (`CRn==2`), barriers (`CRn==3` — DSB/DMB/ISB/CLREX),
            // the `MSR`(immediate) PSTATE writes, the `SYS`/`TLBI`/`AT` ops, and the
            // `MRS`/`MSR` system-register moves. In the flat integer core only the
            // side-effect-free hints/barriers are valid; the privileged ones need
            // the `CC-36` EL model.
            if self.sys.is_some() {
                return self.system_instr(inst, next);
            }
            let crn = (inst >> 12) & 0xf;
            if crn == 0x2 || crn == 0x3 {
                self.pc = next;
                return Ok(());
            }
            return Err(Halt::Trap(Trap::Illegal(inst)));
        }
        if inst & 0xfe00_0000 == 0xd600_0000 {
            // Unconditional branch (register): RET / BR / BLR / ERET.
            let rn = (inst >> 5) & 0x1f;
            let opc = (inst >> 21) & 0xf;
            let target = self.rx(rn);
            match opc {
                0b0000 => self.pc = target, // BR
                0b0001 => {
                    self.x[30] = next; // BLR
                    self.pc = target;
                }
                0b0010 => self.pc = target,                  // RET
                0b0100 if self.sys.is_some() => self.eret(), // ERET
                _ => return Err(Halt::Trap(Trap::Illegal(inst))),
            }
            return Ok(());
        }
        Err(Halt::Trap(Trap::Illegal(inst)))
    }

    /// `SVC #0` host boundary (the flat-core Linux `arm64` ABI): `x8` selects the
    /// syscall, `x0`–`x5` are the arguments. `write(fd, buf, len)` appends to the
    /// console; `exit`/`exit_group` stop the machine with the status in `x0`.
    fn svc(&mut self, next: u64) -> Result<(), Halt> {
        // System mode (`CC-36`): `SVC` is a synchronous exception to EL1 (the
        // kernel's syscall entry), not the flat host boundary.
        if self.sys.is_some() {
            // The syscall number is in x8 (the Linux ABI); the ISS is 0.
            self.take_to_el1(Some(ExcKind::Svc), 0, next, 0x00);
            return Ok(());
        }
        match self.x[8] {
            syscall::EXIT | syscall::EXIT_GROUP => Err(Halt::Exit(self.x[0])),
            syscall::WRITE => {
                let (_fd, buf, len) = (self.x[0], self.x[1], self.x[2]);
                for i in 0..len {
                    let byte = self
                        .read(buf.wrapping_add(i), 1, Access::Load)
                        .map_err(Halt::Trap)? as u8;
                    self.console.push(byte);
                }
                self.x[0] = len; // bytes written
                self.pc = next;
                Ok(())
            }
            other => Err(Halt::Trap(Trap::UnknownSyscall(other))),
        }
    }

    // ── Data Processing -- Register ─────────────────────────────────────────

    fn dp_register(&mut self, inst: u32, next: u64) -> Result<(), Trap> {
        let rd = inst & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rm = (inst >> 16) & 0x1f;
        let sf = inst & 0x8000_0000 != 0;
        let datasize = if sf { 64 } else { 32 };

        if inst & 0x1f00_0000 == 0x0a00_0000 {
            // Logical (shifted register).
            let shift_type = (inst >> 22) & 0x3;
            let n = (inst >> 21) & 1;
            let amount = (inst >> 10) & 0x3f;
            if !sf && amount & 0x20 != 0 {
                return Err(Trap::Illegal(inst));
            }
            let opc = (inst >> 29) & 0x3;
            let a = self.op(rn, sf);
            let mut b = shift_reg(self.op(rm, sf), shift_type, amount, datasize);
            if n == 1 {
                b = !b & mask(datasize);
            }
            let res = match opc {
                0b00 | 0b11 => a & b, // AND / ANDS
                0b01 => a | b,        // ORR / ORN
                0b10 => a ^ b,        // EOR / EON
                _ => unreachable!(),
            } & mask(datasize);
            if opc == 0b11 {
                self.flags = logical_flags(res, sf);
            }
            self.wx_sz(rd, res, sf);
            self.pc = next;
            return Ok(());
        }
        if inst & 0x1f20_0000 == 0x0b00_0000 {
            // Add/subtract (shifted register).
            let shift_type = (inst >> 22) & 0x3;
            let amount = (inst >> 10) & 0x3f;
            if shift_type == 0b11 || (!sf && amount & 0x20 != 0) {
                return Err(Trap::Illegal(inst));
            }
            let op = inst & 0x4000_0000 != 0;
            let setflags = inst & 0x2000_0000 != 0;
            let a = self.op(rn, sf);
            let b = shift_reg(self.op(rm, sf), shift_type, amount, datasize);
            let (res, f) = add_with_carry(a, if op { !b } else { b }, op, sf);
            if setflags {
                self.flags = f;
            }
            self.wx_sz(rd, res, sf);
            self.pc = next;
            return Ok(());
        }
        if inst & 0x1f20_0000 == 0x0b20_0000 {
            // Add/subtract (extended register).
            let option = (inst >> 13) & 0x7;
            let imm3 = (inst >> 10) & 0x7;
            if imm3 > 4 {
                return Err(Trap::Illegal(inst));
            }
            let op = inst & 0x4000_0000 != 0;
            let setflags = inst & 0x2000_0000 != 0;
            let a = self.rx_sp(rn);
            let b = extend_reg(self.rx(rm), option, imm3, sf);
            let (res, f) = add_with_carry(a, if op { !b } else { b }, op, sf);
            if setflags {
                self.flags = f;
                self.wx_sz(rd, res, sf);
            } else {
                self.wx_sp(rd, if sf { res } else { res & 0xffff_ffff });
            }
            self.pc = next;
            return Ok(());
        }
        if inst & 0x1fe0_0000 == 0x1a00_0000 {
            // Add/subtract (with carry): ADC (op=0) / SBC (op=1).
            let op = inst & 0x4000_0000 != 0;
            let setflags = inst & 0x2000_0000 != 0;
            let a = self.op(rn, sf);
            let b = self.op(rm, sf);
            // ADC: a + b + C; SBC: a + NOT(b) + C.
            let (res, f) = add_with_carry(a, if op { !b } else { b }, self.flags.c, sf);
            if setflags {
                self.flags = f;
            }
            self.wx_sz(rd, res, sf);
            self.pc = next;
            return Ok(());
        }
        if inst & 0x1fe0_0000 == 0x1a40_0000 {
            // Conditional compare (register and immediate): CCMP / CCMN.
            let cond = (inst >> 12) & 0xf;
            let nzcv = inst & 0xf;
            let op = inst & 0x4000_0000 != 0; // 0=CCMN, 1=CCMP
            let imm_form = inst & 0x0000_0800 != 0;
            let a = self.op(rn, sf);
            let b = if imm_form {
                ((inst >> 16) & 0x1f) as u64
            } else {
                self.op(rm, sf)
            };
            if self.cond_holds(cond) {
                let (_, f) = add_with_carry(a, if op { !b } else { b }, op, sf);
                self.flags = f;
            } else {
                self.flags = Nzcv::from_imm4(nzcv);
            }
            self.pc = next;
            return Ok(());
        }
        if inst & 0x1fe0_0000 == 0x1a80_0000 {
            // Conditional select: CSEL / CSINC / CSINV / CSNEG.
            let cond = (inst >> 12) & 0xf;
            let op = inst & 0x4000_0000 != 0;
            let o2 = inst & 0x0000_0400 != 0;
            let a = self.op(rn, sf);
            let b = self.op(rm, sf);
            let res = if self.cond_holds(cond) {
                a
            } else {
                match (op, o2) {
                    (false, false) => b,                  // CSEL
                    (false, true) => b.wrapping_add(1),   // CSINC
                    (true, false) => !b,                  // CSINV
                    (true, true) => (!b).wrapping_add(1), // CSNEG
                }
            };
            self.wx_sz(rd, res, sf);
            self.pc = next;
            return Ok(());
        }
        if inst & 0x1f00_0000 == 0x1b00_0000 {
            // Data-processing (3 source).
            let ra = (inst >> 10) & 0x1f;
            let op31 = (inst >> 21) & 0x7;
            let o0 = inst & 0x0000_8000 != 0;
            let res = match (op31, o0) {
                (0b000, false) => self
                    .op(ra, sf)
                    .wrapping_add(self.op(rn, sf).wrapping_mul(self.op(rm, sf))), // MADD
                (0b000, true) => self
                    .op(ra, sf)
                    .wrapping_sub(self.op(rn, sf).wrapping_mul(self.op(rm, sf))), // MSUB
                (0b001, false) => self.rx(ra).wrapping_add(smull(self.rx(rn), self.rx(rm))), // SMADDL
                (0b001, true) => self.rx(ra).wrapping_sub(smull(self.rx(rn), self.rx(rm))), // SMSUBL
                (0b010, false) => smulh(self.rx(rn), self.rx(rm)),                          // SMULH
                (0b101, false) => self.rx(ra).wrapping_add(umull(self.rx(rn), self.rx(rm))), // UMADDL
                (0b101, true) => self.rx(ra).wrapping_sub(umull(self.rx(rn), self.rx(rm))), // UMSUBL
                (0b110, false) => umulh(self.rx(rn), self.rx(rm)),                          // UMULH
                _ => return Err(Trap::Illegal(inst)),
            };
            // The long-multiply forms (SMADDL…UMULH) are 64-bit results.
            let res = if op31 == 0b000 && !sf {
                res & 0xffff_ffff
            } else {
                res
            };
            self.wx(rd, res);
            self.pc = next;
            return Ok(());
        }
        if inst & 0x5fe0_0000 == 0x1ac0_0000 {
            // Data-processing (2 source, bit30==0): the variable shifts and divides.
            let opcode = (inst >> 10) & 0x3f;
            let a = self.op(rn, sf);
            let b = self.op(rm, sf);
            let res = match opcode {
                0b000010 => udiv(a, b, sf),                                      // UDIV
                0b000011 => sdiv(a, b, sf),                                      // SDIV
                0b001000 => shift_reg(a, 0b00, (b as u32) % datasize, datasize), // LSLV
                0b001001 => shift_reg(a, 0b01, (b as u32) % datasize, datasize), // LSRV
                0b001010 => shift_reg(a, 0b10, (b as u32) % datasize, datasize), // ASRV
                0b001011 => shift_reg(a, 0b11, (b as u32) % datasize, datasize), // RORV
                _ => return Err(Trap::Illegal(inst)),
            };
            self.wx_sz(rd, res, sf);
            self.pc = next;
            return Ok(());
        }
        if inst & 0x5fe0_0000 == 0x5ac0_0000 {
            // Data-processing (1 source): RBIT / REV16 / REV32 / REV / CLZ / CLS.
            let opcode = (inst >> 10) & 0x3f;
            let opcode2 = (inst >> 16) & 0x1f;
            if opcode2 != 0 {
                return Err(Trap::Illegal(inst));
            }
            let a = self.op(rn, sf);
            let res = match opcode {
                0b000000 => rbit(a, datasize),  // RBIT
                0b000001 => rev16(a, datasize), // REV16
                0b000010 => {
                    if sf {
                        rev32(a)
                    } else {
                        rev_bytes(a, 4)
                    }
                } // REV32 (64-bit) / REV (32-bit)
                0b000011 if sf => rev_bytes(a, 8), // REV (64-bit)
                0b000100 => u64::from(clz(a, datasize)), // CLZ
                0b000101 => u64::from(cls(a, datasize)), // CLS
                _ => return Err(Trap::Illegal(inst)),
            };
            self.wx_sz(rd, res, sf);
            self.pc = next;
            return Ok(());
        }
        Err(Trap::Illegal(inst))
    }

    // ── Loads and Stores ────────────────────────────────────────────────────

    fn loads_stores(&mut self, inst: u32, next: u64) -> Result<(), Trap> {
        // Load/store exclusive + load-acquire/store-release (the kernel's locks
        // and atomics: LDXR/STXR/LDAXR/STLXR/LDAR/STLR/LDXP/STXP). Without the LSE
        // atomics (ID_AA64ISAR0.Atomic = 0) the kernel uses these LL/SC forms.
        if inst & 0x3f00_0000 == 0x0800_0000 {
            return self.ldst_exclusive(inst, next);
        }
        // Advanced SIMD load/store structures (LD1..LD4 / ST1..ST4, multiple +
        // single-element, no-offset + post-index) — glibc's string routines load
        // a chunk with `LD1 {v.16b},[x]` and broadcast with `LD1R`.
        if inst & 0xbe00_0000 == 0x0c00_0000 {
            return self.asimd_ldst_structures(inst, next);
        }
        // Load register (literal) — integer (V=0) and SIMD&FP (V=1).
        if inst & 0x3b00_0000 == 0x1800_0000 {
            let opc = (inst >> 30) & 0x3;
            let rt = inst & 0x1f;
            let imm = sign_extend(((inst >> 5) & 0x7_ffff) as u64, 19) << 2;
            let addr = self.pc.wrapping_add(imm);
            if inst & 0x0400_0000 != 0 {
                // LDR (literal, SIMD&FP): opc 00→S, 01→D, 10→Q.
                let width = 4usize << opc;
                let val = self.read_simd(addr, width)?;
                self.v[rt as usize] = val;
                self.pc = next;
                return Ok(());
            }
            let (val, sf) = match opc {
                0b00 => (self.read(addr, 4, Access::Load)?, false), // LDR Wt
                0b01 => (self.read(addr, 8, Access::Load)?, true),  // LDR Xt
                0b10 => (sign_extend(self.read(addr, 4, Access::Load)?, 32), true), // LDRSW
                _ => return Err(Trap::Illegal(inst)),
            };
            self.wx_sz(rt, val, sf);
            self.pc = next;
            return Ok(());
        }
        // Load/store pair (integer and SIMD&FP).
        if inst & 0x3a00_0000 == 0x2800_0000 {
            if inst & 0x0400_0000 != 0 {
                return self.ldst_pair_simd(inst, next);
            }
            return self.ldst_pair(inst, next);
        }
        // Load/store register (the size-111-V-0 group). V=1 is SIMD&FP.
        if inst & 0x3b00_0000 == 0x3900_0000 {
            // Unsigned immediate offset.
            return self.ldst_reg(inst, LdStMode::UnsignedOffset, next);
        }
        if inst & 0x3b00_0000 == 0x3800_0000 {
            if inst & 0x0020_0000 != 0 && inst & 0x0000_0c00 == 0x0000_0800 {
                // Register offset.
                return self.ldst_reg(inst, LdStMode::RegisterOffset, next);
            }
            // Unscaled / immediate pre/post-index / unprivileged.
            let mode = match (inst >> 10) & 0x3 {
                0b00 => LdStMode::Unscaled,
                0b01 => LdStMode::PostIndex,
                0b11 => LdStMode::PreIndex,
                0b10 => LdStMode::Unscaled, // LDTR/STTR (unprivileged) — flat-core equivalent
                _ => unreachable!(),
            };
            return self.ldst_reg(inst, mode, next);
        }
        Err(Trap::Illegal(inst))
    }

    /// Compute the effective address (and any base writeback) of a load/store
    /// register instruction, for the given addressing `mode` and `scale` (the
    /// log2 access size — the integer `size` or the SIMD element size).
    fn ldst_addr(&self, inst: u32, mode: LdStMode, scale: u32) -> (u64, Option<u64>) {
        let rn = (inst >> 5) & 0x1f;
        let base = self.rx_sp(rn);
        match mode {
            LdStMode::UnsignedOffset => {
                let imm = ((inst >> 10) & 0xfff) as u64;
                (base.wrapping_add(imm << scale), None)
            }
            LdStMode::Unscaled => {
                let imm = sign_extend(((inst >> 12) & 0x1ff) as u64, 9);
                (base.wrapping_add(imm), None)
            }
            LdStMode::PostIndex => {
                let imm = sign_extend(((inst >> 12) & 0x1ff) as u64, 9);
                (base, Some(base.wrapping_add(imm)))
            }
            LdStMode::PreIndex => {
                let imm = sign_extend(((inst >> 12) & 0x1ff) as u64, 9);
                let a = base.wrapping_add(imm);
                (a, Some(a))
            }
            LdStMode::RegisterOffset => {
                let rm = (inst >> 16) & 0x1f;
                let option = (inst >> 13) & 0x7;
                let s = (inst >> 12) & 1;
                let shift = if s == 1 { scale } else { 0 };
                let off = extend_reg_for_addr(self.rx(rm), option, shift);
                (base.wrapping_add(off), None)
            }
        }
    }

    fn ldst_reg(&mut self, inst: u32, mode: LdStMode, next: u64) -> Result<(), Trap> {
        let size = (inst >> 30) & 0x3;
        let opc = (inst >> 22) & 0x3;
        let rt = inst & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        if inst & 0x0400_0000 != 0 {
            return self.ldst_reg_simd(inst, mode, next); // V==1 → SIMD&FP
        }
        let width = 1usize << size;
        // opc decodes load/store + sign/zero extension (the integer table).
        let (is_load, signed, dst64) = match (size, opc) {
            (_, 0b00) => (false, false, size == 0b11), // STR
            (_, 0b01) => (true, false, size == 0b11),  // LDR (zero-extend)
            (0b11, 0b10) => {
                // PRFM (prefetch) — architecturally a hint; consume and continue.
                self.pc = next;
                return Ok(());
            }
            (_, 0b10) => (true, true, true), // LDRS* to 64-bit
            (0b10, 0b11) => return Err(Trap::Illegal(inst)), // reserved
            (_, 0b11) => (true, true, false), // LDRS* to 32-bit
            _ => return Err(Trap::Illegal(inst)),
        };

        let (addr, writeback) = self.ldst_addr(inst, mode, size);

        if is_load {
            let raw = self.read(addr, width, Access::Load)?;
            let val = if signed {
                let v = sign_extend(raw, (width * 8) as u32);
                if dst64 {
                    v
                } else {
                    v & 0xffff_ffff
                }
            } else {
                raw
            };
            self.wx_sz(rt, val, dst64);
        } else {
            self.write(addr, width, self.rx(rt))?;
        }
        if let Some(wb) = writeback {
            self.wx_sp(rn, wb);
        }
        self.pc = next;
        Ok(())
    }

    /// Read a `width`-byte (1/2/4/8/16) value from memory into a 128-bit lane,
    /// zero-extended — the SIMD&FP load primitive.
    fn read_simd(&mut self, addr: u64, width: usize) -> Result<u128, Trap> {
        Ok(if width == 16 {
            let lo = self.read(addr, 8, Access::Load)? as u128;
            let hi = self.read(addr.wrapping_add(8), 8, Access::Load)? as u128;
            lo | (hi << 64)
        } else {
            self.read(addr, width, Access::Load)? as u128
        })
    }

    /// Write the low `width` bytes of a 128-bit lane to memory — the SIMD&FP store
    /// primitive.
    fn write_simd(&mut self, addr: u64, width: usize, val: u128) -> Result<(), Trap> {
        if width == 16 {
            self.write(addr, 8, val as u64)?;
            self.write(addr.wrapping_add(8), 8, (val >> 64) as u64)?;
        } else {
            self.write(addr, width, val as u64)?;
        }
        Ok(())
    }

    /// Load/store a single SIMD&FP register (`B`/`H`/`S`/`D`/`Q`) — `LDR`/`STR`
    /// (vector). Pure data movement (no FP arithmetic): the kernel uses these to
    /// move a task's FP context, and the integer userspace never computes with it.
    fn ldst_reg_simd(&mut self, inst: u32, mode: LdStMode, next: u64) -> Result<(), Trap> {
        let size = (inst >> 30) & 0x3;
        let opc = (inst >> 22) & 0x3;
        let rt = inst & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        // The SIMD access size: opc<1> is the high bit of the log2 size (Q = 0b100).
        let scale = ((opc & 2) << 1) | size;
        if scale > 4 {
            return Err(Trap::Illegal(inst));
        }
        let width = 1usize << scale;
        let is_load = opc & 1 != 0;
        let (addr, writeback) = self.ldst_addr(inst, mode, scale);
        if is_load {
            let val = self.read_simd(addr, width)?;
            self.v[rt as usize] = val;
        } else {
            self.write_simd(addr, width, self.v[rt as usize])?;
        }
        if let Some(wb) = writeback {
            self.wx_sp(rn, wb);
        }
        self.pc = next;
        Ok(())
    }

    /// Load/store a pair of SIMD&FP registers (`S`/`D`/`Q`) — `LDP`/`STP` (vector),
    /// the form the kernel's `fpsimd_save_state`/`fpsimd_load_state` use.
    fn ldst_pair_simd(&mut self, inst: u32, next: u64) -> Result<(), Trap> {
        let opc = (inst >> 30) & 0x3;
        let l = inst & 0x0040_0000 != 0;
        let rt = inst & 0x1f;
        let rt2 = (inst >> 10) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let scale = 2 + opc; // 00→S(4), 01→D(8), 10→Q(16)
        if scale > 4 {
            return Err(Trap::Illegal(inst));
        }
        let width = 1usize << scale;
        let imm = sign_extend(((inst >> 15) & 0x7f) as u64, 7) << scale;
        let base = self.rx_sp(rn);
        let (addr, writeback) = match (inst >> 23) & 0x3 {
            0b01 => (base, Some(base.wrapping_add(imm))), // post-index
            0b11 => (base.wrapping_add(imm), Some(base.wrapping_add(imm))), // pre-index
            _ => (base.wrapping_add(imm), None),          // signed offset / no-alloc
        };
        if l {
            let v1 = self.read_simd(addr, width)?;
            let v2 = self.read_simd(addr.wrapping_add(width as u64), width)?;
            self.v[rt as usize] = v1;
            self.v[rt2 as usize] = v2;
        } else {
            self.write_simd(addr, width, self.v[rt as usize])?;
            self.write_simd(addr.wrapping_add(width as u64), width, self.v[rt2 as usize])?;
        }
        if let Some(wb) = writeback {
            self.wx_sp(rn, wb);
        }
        self.pc = next;
        Ok(())
    }

    /// Advanced SIMD load/store structures (`LD1`–`LD4` / `ST1`–`ST4`, both the
    /// multiple-register and single-element forms, no-offset + post-index). The
    /// vector loads glibc's `memcpy`/`strlen`/`memset` use: `LD1 {v.16b},[x]`,
    /// the `LD1R` broadcast, and the single-lane tail moves. Follows the Arm-ARM
    /// `LDST*` pseudocode (de-interleaving for `selem > 1`).
    fn asimd_ldst_structures(&mut self, inst: u32, next: u64) -> Result<(), Trap> {
        let q = inst & (1 << 30) != 0;
        let l = inst & (1 << 22) != 0; // load
        let single = inst & (1 << 24) != 0;
        let post = inst & (1 << 23) != 0;
        let rm = (inst >> 16) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rt = inst & 0x1f;
        let base = self.rx_sp(rn);
        let mut addr = base;

        let bytes: u64 = if !single {
            let opcode = (inst >> 12) & 0xf;
            let (rpt, selem): (u32, u32) = match opcode {
                0b0000 => (1, 4), // LD/ST4
                0b0010 => (4, 1), // LD/ST1 (4 regs)
                0b0100 => (1, 3), // LD/ST3
                0b0110 => (3, 1), // LD/ST1 (3 regs)
                0b0111 => (1, 1), // LD/ST1 (1 reg)
                0b1000 => (1, 2), // LD/ST2
                0b1010 => (2, 1), // LD/ST1 (2 regs)
                _ => return Err(Trap::Illegal(inst)),
            };
            let size = (inst >> 10) & 0x3;
            let ebytes = 1usize << size;
            let elements = (if q { 16 } else { 8 }) / ebytes;
            for r in 0..rpt {
                for e in 0..elements {
                    for s in 0..selem {
                        let tt = ((rt + r * selem + s) % 32) as usize;
                        if l {
                            let v = self.read(addr, ebytes, Access::Load)?;
                            let mut reg = self.v[tt];
                            set_lane(&mut reg, size, e, v);
                            self.v[tt] = reg;
                        } else {
                            let v = lane(self.v[tt], size, e);
                            self.write(addr, ebytes, v)?;
                        }
                        addr = addr.wrapping_add(ebytes as u64);
                    }
                }
            }
            if l && !q {
                // Each loaded 64-bit register clears its upper half.
                for k in 0..rpt * selem {
                    let tt = ((rt + k) % 32) as usize;
                    self.v[tt] &= u64::MAX as u128;
                }
            }
            (rpt * selem * elements as u32 * ebytes as u32) as u64
        } else {
            // Single-element structures: opcode<2:1> = scale, S = bit12,
            // selem = (opcode<0>:R)+1, with scale==3 → LD1R replicate.
            let opcode = (inst >> 13) & 0x7;
            let s_bit = (inst >> 12) & 1;
            let size = (inst >> 10) & 0x3;
            let r_bit = (inst >> 21) & 1;
            let selem = ((opcode & 1) << 1 | r_bit) + 1;
            let scale = opcode >> 1;
            let (ebytes, index, replicate) = match scale {
                0 => (1usize, (q as u32) << 3 | s_bit << 2 | size, false),
                1 => (2usize, (q as u32) << 2 | s_bit << 1 | (size >> 1), false),
                2 if size & 1 == 0 => (4usize, (q as u32) << 1 | s_bit, false),
                2 => (8usize, q as u32, false), // D element (scale becomes 3)
                3 => (1usize << size, 0, true), // LD1R/LD2R… replicate
                _ => return Err(Trap::Illegal(inst)),
            };
            for s in 0..selem {
                let tt = ((rt + s) % 32) as usize;
                if replicate {
                    // Load one element and broadcast across all lanes of Vt.
                    let v = self.read(addr, ebytes, Access::Load)?;
                    let mut reg = 0u128;
                    let lanes = (if q { 16 } else { 8 }) / ebytes;
                    for e in 0..lanes {
                        set_lane(&mut reg, size, e, v);
                    }
                    self.v[tt] = reg;
                } else if l {
                    let v = self.read(addr, ebytes, Access::Load)?;
                    let mut reg = self.v[tt];
                    set_lane(&mut reg, size, index as usize, v);
                    self.v[tt] = reg;
                } else {
                    let v = lane(self.v[tt], size, index as usize);
                    self.write(addr, ebytes, v)?;
                }
                addr = addr.wrapping_add(ebytes as u64);
            }
            (selem as usize * ebytes) as u64
        };

        if post {
            let off = if rm == 31 { bytes } else { self.rx(rm) };
            self.wx_sp(rn, base.wrapping_add(off));
        }
        self.pc = next;
        Ok(())
    }

    fn ldst_pair(&mut self, inst: u32, next: u64) -> Result<(), Trap> {
        if inst & 0x0400_0000 != 0 {
            return Err(Trap::Illegal(inst)); // V==1 → SIMD/FP
        }
        let opc = (inst >> 30) & 0x3;
        let l = inst & 0x0040_0000 != 0;
        let rt = inst & 0x1f;
        let rt2 = (inst >> 10) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let (scale, signed): (u32, bool) = match opc {
            0b00 => (2, false), // 32-bit
            0b01 => (2, true),  // LDPSW (signed-word → 64-bit)
            0b10 => (3, false), // 64-bit
            _ => return Err(Trap::Illegal(inst)),
        };
        let width = 1usize << scale;
        let dst64 = scale == 3 || signed;
        let imm = sign_extend(((inst >> 15) & 0x7f) as u64, 7) << scale;
        let base = self.rx_sp(rn);
        // bits[24:23] select the index variant.
        let (addr, writeback) = match (inst >> 23) & 0x3 {
            0b01 => (base, Some(base.wrapping_add(imm))), // post-index
            0b11 => (base.wrapping_add(imm), Some(base.wrapping_add(imm))), // pre-index
            _ => (base.wrapping_add(imm), None),          // signed offset / no-alloc
        };
        if l {
            let v1 = self.read(addr, width, Access::Load)?;
            let v2 = self.read(addr.wrapping_add(width as u64), width, Access::Load)?;
            let (v1, v2) = if signed {
                (sign_extend(v1, 32), sign_extend(v2, 32))
            } else {
                (v1, v2)
            };
            self.wx_sz(rt, v1, dst64);
            self.wx_sz(rt2, v2, dst64);
        } else {
            self.write(addr, width, self.rx(rt))?;
            self.write(addr.wrapping_add(width as u64), width, self.rx(rt2))?;
        }
        if let Some(wb) = writeback {
            self.wx_sp(rn, wb);
        }
        self.pc = next;
        Ok(())
    }

    /// Load/store exclusive + ordered (Arm-ARM C3.2.6): `LDXR`/`STXR`,
    /// `LDAXR`/`STLXR`, the pair forms `LDXP`/`STXP`, and the non-exclusive
    /// `LDAR`/`STLR`. On a single PE the monitor is a one-address reservation;
    /// the acquire/release ordering is a no-op on a sequential interpreter.
    fn ldst_exclusive(&mut self, inst: u32, next: u64) -> Result<(), Trap> {
        let size = (inst >> 30) & 0x3;
        let o2 = (inst >> 23) & 1; // 0 = exclusive, 1 = ordered (LDAR/STLR)
        let l = (inst >> 22) & 1; // 0 = store, 1 = load
        let o1 = (inst >> 21) & 1; // 1 = pair
        let rs = (inst >> 16) & 0x1f; // status result (stores)
        let rt2 = (inst >> 10) & 0x1f;
        let rn = (inst >> 5) & 0x1f;
        let rt = inst & 0x1f;
        let width = 1usize << size;
        let addr = self.rx_sp(rn);
        let dst64 = size == 3;

        if o2 == 1 {
            // Load-acquire / store-release (non-exclusive): LDAR / STLR.
            if l == 1 {
                let v = self.read(addr, width, Access::Load)?;
                self.wx_sz(rt, v, dst64);
            } else {
                self.write(addr, width, self.rx(rt))?;
            }
            self.pc = next;
            return Ok(());
        }

        // Exclusive forms.
        if l == 1 {
            // LDXR / LDAXR / LDXP / LDAXP — load and set the monitor.
            if o1 == 1 {
                let v1 = self.read(addr, width, Access::Load)?;
                let v2 = self.read(addr.wrapping_add(width as u64), width, Access::Load)?;
                self.wx_sz(rt, v1, dst64);
                self.wx_sz(rt2, v2, dst64);
            } else {
                let v = self.read(addr, width, Access::Load)?;
                self.wx_sz(rt, v, dst64);
            }
            self.excl = Some(addr);
        } else {
            // STXR / STLXR / STXP / STLXP — store iff the monitor is set, and
            // report success (0) / failure (1) in Ws.
            if self.excl == Some(addr) {
                if o1 == 1 {
                    self.write(addr, width, self.rx(rt))?;
                    self.write(addr.wrapping_add(width as u64), width, self.rx(rt2))?;
                } else {
                    self.write(addr, width, self.rx(rt))?;
                }
                self.excl = None;
                self.wx(rs, 0);
            } else {
                self.wx(rs, 1);
            }
        }
        self.pc = next;
        Ok(())
    }

    /// Evaluate an A64 condition code against `PSTATE.NZCV` (Arm-ARM `ConditionHolds`).
    fn cond_holds(&self, cond: u32) -> bool {
        let f = self.flags;
        let base = match cond >> 1 {
            0b000 => f.z,                  // EQ
            0b001 => f.c,                  // CS/HS
            0b010 => f.n,                  // MI
            0b011 => f.v,                  // VS
            0b100 => f.c && !f.z,          // HI
            0b101 => f.n == f.v,           // GE
            0b110 => (f.n == f.v) && !f.z, // GT
            _ => true,                     // AL
        };
        if cond & 1 == 1 && cond != 0b1111 {
            !base
        } else {
            base
        }
    }
}

/// The addressing mode of a load/store register instruction.
#[derive(Clone, Copy)]
enum LdStMode {
    UnsignedOffset,
    Unscaled,
    PostIndex,
    PreIndex,
    RegisterOffset,
}

// ── pure A64 helpers (the Arm-ARM primitives) ───────────────────────────────

/// `mask(width)` — the low `width` bits set (`width` in 1..=64).
#[inline]
fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// Sign-extend the low `bits` of `v` to 64 bits (Arm-ARM `SignExtend`).
#[inline]
fn sign_extend(v: u64, bits: u32) -> u64 {
    if bits >= 64 {
        return v;
    }
    let shift = 64 - bits;
    (((v << shift) as i64) >> shift) as u64
}

// ── Advanced SIMD lane + element primitives ─────────────────────────────────

/// The low `bits` set, where `bits = 8 << size` — an alias of [`mask`] used by
/// the SIMD lane code where the width comes from an element size.
#[inline]
fn mask_bits(bits: u32) -> u64 {
    mask(bits)
}

/// Read element `idx` (size `1 << size_log2` bytes, i.e. `8 << size_log2` bits)
/// from the little-endian 128-bit register value `v`.
#[inline]
fn lane(v: u128, size_log2: u32, idx: usize) -> u64 {
    let bits = 8u32 << size_log2;
    let shift = idx * bits as usize;
    if bits >= 128 {
        v as u64
    } else {
        ((v >> shift) & ((1u128 << bits) - 1)) as u64
    }
}

/// Write element `idx` (size `8 << size_log2` bits) of `v`, leaving the rest.
#[inline]
fn set_lane(v: &mut u128, size_log2: u32, idx: usize, x: u64) {
    let bits = 8u32 << size_log2;
    let shift = idx * bits as usize;
    let em: u128 = if bits >= 128 {
        u128::MAX
    } else {
        (1u128 << bits) - 1
    };
    *v = (*v & !(em << shift)) | (((x as u128) & em) << shift);
}

/// A per-element boolean result: all-ones (later masked to the element) or zero,
/// matching NEON's compare convention.
#[inline]
fn bool_lane(b: bool) -> u64 {
    if b {
        u64::MAX
    } else {
        0
    }
}

/// Signed comparison of two `esize`-bit lane values.
#[inline]
fn scmp(x: u64, y: u64, esize: u32) -> core::cmp::Ordering {
    (sign_extend(x, esize) as i64).cmp(&(sign_extend(y, esize) as i64))
}

/// Signed max/min of two `esize`-bit lane values, returning the winning bits.
#[inline]
fn smax(x: u64, y: u64, esize: u32) -> u64 {
    if scmp(x, y, esize) != core::cmp::Ordering::Less {
        x
    } else {
        y
    }
}
#[inline]
fn smin(x: u64, y: u64, esize: u32) -> u64 {
    if scmp(x, y, esize) == core::cmp::Ordering::Less {
        x
    } else {
        y
    }
}

/// Signed absolute difference of two `esize`-bit lanes.
#[inline]
fn sabd(x: u64, y: u64, esize: u32) -> u64 {
    let a = sign_extend(x, esize) as i64;
    let b = sign_extend(y, esize) as i64;
    a.abs_diff(b)
}

/// Polynomial (carry-less) multiply of two `esize`-bit lanes (`PMUL`).
#[inline]
fn pmul(x: u64, y: u64, esize: u32) -> u64 {
    let mut r = 0u64;
    for k in 0..esize {
        if (y >> k) & 1 == 1 {
            r ^= x << k;
        }
    }
    r & mask(esize)
}

/// `USHL`/`SSHL` — shift each lane by the signed amount in the low byte of the
/// corresponding `Vm` lane (positive = left, negative = right).
#[inline]
fn ushl_sshl(x: u64, y: u64, esize: u32, unsigned: bool) -> u64 {
    let em = mask(esize);
    let sh = (y as i8) as i32; // SSHL/USHL take the signed low byte
    if sh >= 0 {
        let s = sh as u32;
        if s >= esize {
            0
        } else {
            (x << s) & em
        }
    } else {
        let s = (-sh) as u32;
        if s >= esize {
            if unsigned {
                0
            } else if sign_extend(x, esize) as i64 != 0 && (x >> (esize - 1)) & 1 == 1 {
                em
            } else {
                0
            }
        } else if unsigned {
            (x & em) >> s
        } else {
            (((sign_extend(x, esize) as i64) >> s) as u64) & em
        }
    }
}

/// Saturating add/sub of two `esize`-bit lanes (`UQADD`/`SQADD`, `UQSUB`/`SQSUB`).
#[inline]
fn uqadd_sqadd(x: u64, y: u64, esize: u32, unsigned: bool) -> u64 {
    let em = mask(esize);
    if unsigned {
        ((x & em) + (y & em)).min(em)
    } else {
        let maxv = (em >> 1) as i64;
        let minv = -(maxv + 1);
        let s = (sign_extend(x, esize) as i64) + (sign_extend(y, esize) as i64);
        (s.clamp(minv, maxv) as u64) & em
    }
}
#[inline]
fn uqsub_sqsub(x: u64, y: u64, esize: u32, unsigned: bool) -> u64 {
    let em = mask(esize);
    if unsigned {
        (x & em).saturating_sub(y & em)
    } else {
        let maxv = (em >> 1) as i64;
        let minv = -(maxv + 1);
        let s = (sign_extend(x, esize) as i64) - (sign_extend(y, esize) as i64);
        (s.clamp(minv, maxv) as u64) & em
    }
}

/// Count leading zeros (`CLZ`) or leading sign bits (`CLS`) within `esize` bits.
#[inline]
fn clsz(x: u64, esize: u32, leading_zeros: bool) -> u64 {
    let v = x & mask(esize);
    if leading_zeros {
        if v == 0 {
            esize as u64
        } else {
            (v << (64 - esize)).leading_zeros() as u64
        }
    } else {
        let top = (v >> (esize - 1)) & 1;
        let mut count = 0u64;
        for k in (0..esize - 1).rev() {
            if (v >> k) & 1 == top {
                count += 1;
            } else {
                break;
            }
        }
        count
    }
}

// ── Advanced SIMD / scalar floating-point numeric helpers ───────────────────

/// Rounding mode for floating-point → integer conversion.
#[derive(Clone, Copy)]
enum Round {
    Nearest,
    Zero,
    Plus,
    Minus,
}

/// One floating-point three-same lane op. `xb`/`yb`/`accb` are the element bit
/// patterns; returns the result element bits, or `None` for an unimplemented
/// encoding.
fn fp3(opcode: u32, u: bool, o1: bool, is_f64: bool, xb: u64, yb: u64, accb: u64) -> Option<u64> {
    let rd = |f: f64| -> u64 {
        if is_f64 {
            f.to_bits()
        } else {
            (f as f32).to_bits() as u64
        }
    };
    let to_f = |b: u64| -> f64 {
        if is_f64 {
            f64::from_bits(b)
        } else {
            f32::from_bits(b as u32) as f64
        }
    };
    let a = to_f(xb);
    let b = to_f(yb);
    let cmp_true = if is_f64 { u64::MAX } else { u32::MAX as u64 };
    Some(match (opcode, u) {
        (0b11010, false) => rd(if o1 { a - b } else { a + b }), // FSUB / FADD
        (0b11010, true) => rd((a - b).abs()),                   // FABD
        (0b11011, false) => rd(a * b),                          // FMULX (≈ FMUL here)
        (0b11011, true) => rd(a * b),                           // FMUL
        (0b11111, true) => rd(a / b),                           // FDIV
        (0b11000, false) => rd(if o1 { a.min(b) } else { a.max(b) }), // FMINNM / FMAXNM
        (0b11110, false) => rd(if o1 { a.min(b) } else { a.max(b) }), // FMIN / FMAX
        (0b11001, false) => rd(if o1 {
            accb_sub(accb, a, b, is_f64)
        } else {
            accb_add(accb, a, b, is_f64)
        }), // FMLS / FMLA
        (0b11100, false) => {
            if a == b {
                cmp_true
            } else {
                0
            }
        } // FCMEQ
        (0b11100, true) => {
            if a >= b {
                cmp_true
            } else {
                0
            }
        } // FCMGE
        _ => return None,
    })
}

#[inline]
fn accb_add(accb: u64, a: f64, b: f64, is_f64: bool) -> f64 {
    let acc = if is_f64 {
        f64::from_bits(accb)
    } else {
        f32::from_bits(accb as u32) as f64
    };
    acc + a * b
}
#[inline]
fn accb_sub(accb: u64, a: f64, b: f64, is_f64: bool) -> f64 {
    let acc = if is_f64 {
        f64::from_bits(accb)
    } else {
        f32::from_bits(accb as u32) as f64
    };
    acc - a * b
}

/// `AdvSIMDExpandImm` (Arm-ARM) — expand the `MOVI`/`MVNI`/`ORR`/`BIC`/`FMOV`
/// 8-bit modified immediate into the 128-bit pattern, with flags for whether the
/// op negates (`MVNI`) or accumulates logically (`ORR`/`BIC`).
fn adv_simd_expand_imm(cmode: u32, op: bool, imm8: u8) -> (u128, bool, bool) {
    let i = imm8 as u64;
    let rep32 = |w: u64| (w & 0xffff_ffff) * 0x0000_0001_0000_0001;
    let rep16 = |h: u64| (h & 0xffff) * 0x0001_0001_0001_0001;
    let rep8 = |b: u64| (b & 0xff) * 0x0101_0101_0101_0101;
    let imm64: u64 = match (cmode >> 1) & 0x7 {
        0b000 => rep32(i),
        0b001 => rep32(i << 8),
        0b010 => rep32(i << 16),
        0b011 => rep32(i << 24),
        0b100 => rep16(i),
        0b101 => rep16(i << 8),
        0b110 => {
            if cmode & 1 == 0 {
                rep32((i << 8) | 0xff)
            } else {
                rep32((i << 16) | 0xffff)
            }
        }
        _ => {
            // cmode == 111x
            if cmode & 1 == 0 && !op {
                rep8(i) // MOVI 8-bit
            } else if cmode & 1 == 0 && op {
                // MOVI 64-bit: each bit of imm8 expands to a byte 0x00/0xff.
                let mut v = 0u64;
                for k in 0..8 {
                    if (i >> k) & 1 == 1 {
                        v |= 0xffu64 << (k * 8);
                    }
                }
                v
            } else if cmode & 1 == 1 && !op {
                // FMOV vector single-precision immediate, replicated.
                rep32(vfp_imm32(imm8) as u64)
            } else {
                // FMOV vector double-precision immediate.
                vfp_expand_imm(imm8, 0b01).to_bits()
            }
        }
    };
    let val = (imm64 as u128) | ((imm64 as u128) << 64);
    let is_logical = (cmode & 1 == 1) && ((cmode >> 2) & 0x3 != 0x3);
    let is_mvni = op && !is_logical && cmode != 0b1110 && cmode != 0b1111;
    (val, is_mvni, is_logical)
}

/// Bits of the single-precision `FMOV` 8-bit immediate.
fn vfp_imm32(imm8: u8) -> u32 {
    let i = imm8 as u32;
    let sign = (i >> 7) & 1;
    let b = (i >> 6) & 1;
    let frac = i & 0x3f;
    // The 8-bit exponent field is NOT(b) : b*5 : imm8<5:4>.
    let exp_field = ((!b & 1) << 7) | (replicate_bit(b, 5) << 2) | ((i >> 4) & 0x3);
    (sign << 31) | (exp_field << 23) | (frac << 17)
}

/// Replicate bit `b` `n` times into the low bits.
#[inline]
fn replicate_bit(b: u32, n: u32) -> u32 {
    if b & 1 == 1 {
        (1u32 << n) - 1
    } else {
        0
    }
}

/// `VFPExpandImm` (Arm-ARM) — the scalar `FMOV` 8-bit immediate as an f64
/// (single-precision values widen exactly).
fn vfp_expand_imm(imm8: u8, ftype: u32) -> f64 {
    if ftype == 0b00 {
        f32::from_bits(vfp_imm32(imm8)) as f64
    } else {
        let i = imm8 as u64;
        let sign = (i >> 7) & 1;
        let b = (i >> 6) & 1;
        let frac = i & 0x3f;
        let exp_field = ((!b & 1) << 10) | (replicate_bit_u64(b, 8) << 2) | ((i >> 4) & 0x3);
        let bits = (sign << 63) | (exp_field << 52) | (frac << 46);
        f64::from_bits(bits)
    }
}
#[inline]
fn replicate_bit_u64(b: u64, n: u32) -> u64 {
    if b & 1 == 1 {
        (1u64 << n) - 1
    } else {
        0
    }
}

/// Pack an f64 result back into a SIMD register low element at `ftype` precision.
#[inline]
fn fp_bits(ftype: u32, val: f64) -> u128 {
    if ftype == 0b01 {
        val.to_bits() as u128
    } else {
        (val as f32).to_bits() as u128
    }
}

/// Apply a unary f64 op to a scalar element and repack (`FABS`/`FNEG`).
#[inline]
fn fp_map(ftype: u32, xbits: u128, f: impl Fn(f64) -> f64) -> u128 {
    let v = if ftype == 0b01 {
        f64::from_bits(xbits as u64)
    } else {
        f32::from_bits(xbits as u32) as f64
    };
    fp_bits(ftype, f(v))
}

/// Round a scalar FP value to an integral FP value (`FRINT*`).
#[inline]
fn fp_round(ftype: u32, xbits: u128, mode: Round) -> u128 {
    let v = if ftype == 0b01 {
        f64::from_bits(xbits as u64)
    } else {
        f32::from_bits(xbits as u32) as f64
    };
    fp_bits(ftype, round_f64(v, mode))
}

/// FCMP — set NZCV from a floating-point comparison (Arm-ARM `FPCompare`).
fn fp_compare(a: f64, b: f64) -> Nzcv {
    use core::cmp::Ordering::*;
    match a.partial_cmp(&b) {
        Some(Less) => Nzcv {
            n: true,
            z: false,
            c: false,
            v: false,
        },
        Some(Equal) => Nzcv {
            n: false,
            z: true,
            c: true,
            v: false,
        },
        Some(Greater) => Nzcv {
            n: false,
            z: false,
            c: true,
            v: false,
        },
        None => Nzcv {
            n: false,
            z: false,
            c: true,
            v: true,
        }, // unordered
    }
}

/// Round `f` to an integral value per `mode`, using only `core` float ops.
fn round_f64(f: f64, mode: Round) -> f64 {
    if f.is_nan() || f.is_infinite() {
        return f;
    }
    let t = if f.abs() < 9.0e18 {
        (f as i64) as f64
    } else {
        f
    }; // truncate toward zero
    let diff = f - t;
    match mode {
        Round::Zero => t,
        Round::Minus => {
            if diff < 0.0 {
                t - 1.0
            } else {
                t
            }
        }
        Round::Plus => {
            if diff > 0.0 {
                t + 1.0
            } else {
                t
            }
        }
        Round::Nearest => {
            let ad = diff.abs();
            if ad < 0.5 {
                t
            } else if ad > 0.5 {
                t + if diff < 0.0 { -1.0 } else { 1.0 }
            } else {
                // tie → round to even
                let up = t + if diff < 0.0 { -1.0 } else { 1.0 };
                if (t as i64) % 2 == 0 {
                    t
                } else {
                    up
                }
            }
        }
    }
}

/// Convert a float to an integer with rounding + saturation (`FCVT*`). Rust's
/// `as` casts already saturate out-of-range and map NaN→0.
fn fp_to_int(f: f64, mode: Round, signed: bool, is64: bool) -> u64 {
    let r = round_f64(f, mode);
    match (signed, is64) {
        (true, true) => r as i64 as u64,
        (true, false) => (r as i32 as u32) as u64,
        (false, true) => r as u64,
        (false, false) => (r as u32) as u64,
    }
}

/// Rotate the low `width` bits of `x` right by `shift` (Arm-ARM `ROR`).
#[inline]
fn ror(x: u64, shift: u32, width: u32) -> u64 {
    let x = x & mask(width);
    let s = shift % width;
    if s == 0 {
        x
    } else {
        ((x >> s) | (x << (width - s))) & mask(width)
    }
}

/// Apply a register shift (`LSL`/`LSR`/`ASR`/`ROR`) of `amount` to the low
/// `width` bits of `v` (Arm-ARM `ShiftReg`).
fn shift_reg(v: u64, shift_type: u32, amount: u32, width: u32) -> u64 {
    let v = v & mask(width);
    let amount = amount % 64;
    match shift_type {
        0b00 => (v << amount) & mask(width), // LSL
        0b01 => v >> amount,                 // LSR
        0b10 => {
            // ASR within `width`.
            let s = sign_extend(v, width);
            ((s as i64) >> amount) as u64 & mask(width)
        }
        _ => ror(v, amount, width), // ROR
    }
}

/// `AddWithCarry` (Arm-ARM): the `width`-bit sum of `x`, `y`, and `carry_in`,
/// with the `NZCV` flags it produces. Subtraction is `add_with_carry(x, !y, 1)`.
fn add_with_carry(x: u64, y: u64, carry_in: bool, sf: bool) -> (u64, Nzcv) {
    let width = if sf { 64 } else { 32 };
    let (x, y) = (x & mask(width), y & mask(width));
    let cin = u128::from(carry_in);
    let usum = u128::from(x) + u128::from(y) + cin;
    let result = (usum as u64) & mask(width);
    let sx = i128::from(sign_extend(x, width) as i64);
    let sy = i128::from(sign_extend(y, width) as i64);
    let ssum = sx + sy + cin as i128;
    let n = (result >> (width - 1)) & 1 == 1;
    let z = result == 0;
    let c = (usum >> width) & 1 == 1;
    let v = i128::from(sign_extend(result, width) as i64) != ssum;
    (result, Nzcv { n, z, c, v })
}

/// The `NZCV` a logical operation (`ANDS`/`BICS`/`TST`) sets: `N`/`Z` from the
/// result, `C` and `V` cleared.
fn logical_flags(result: u64, sf: bool) -> Nzcv {
    let width = if sf { 64 } else { 32 };
    Nzcv {
        n: (result >> (width - 1)) & 1 == 1,
        z: result & mask(width) == 0,
        c: false,
        v: false,
    }
}

/// Extend register `v` per an add/sub extended-register `option` then `LSL` by
/// `shift` (Arm-ARM `ExtendReg`). The destination size is `sf`.
fn extend_reg(v: u64, option: u32, shift: u32, sf: bool) -> u64 {
    let (bits, signed) = match option {
        0b000 => (8, false),  // UXTB
        0b001 => (16, false), // UXTH
        0b010 => (32, false), // UXTW
        0b011 => (64, false), // UXTX
        0b100 => (8, true),   // SXTB
        0b101 => (16, true),  // SXTH
        0b110 => (32, true),  // SXTW
        _ => (64, true),      // SXTX
    };
    let extracted = v & mask(bits);
    let ext = if signed {
        sign_extend(extracted, bits)
    } else {
        extracted
    };
    let out = ext << shift;
    if sf {
        out
    } else {
        out & 0xffff_ffff
    }
}

/// The address-form extend (the register-offset load/store): a 64-bit address
/// offset, with `UXTX`/`SXTX`/`LSL` and the `UXTW`/`SXTW` word forms.
fn extend_reg_for_addr(v: u64, option: u32, shift: u32) -> u64 {
    let (bits, signed) = match option {
        0b010 => (32, false), // UXTW
        0b011 => (64, false), // LSL/UXTX
        0b110 => (32, true),  // SXTW
        0b111 => (64, true),  // SXTX
        _ => (64, false),     // (reserved address extends behave as UXTX)
    };
    let ext = if signed {
        sign_extend(v & mask(bits), bits)
    } else {
        v & mask(bits)
    };
    ext << shift
}

/// `DecodeBitMasks` (Arm-ARM): decode an `(N, imms, immr)` bitmask immediate into
/// the work mask and the (bitfield) tail mask, replicated across 64 bits.
/// `immediate` rejects the reserved logical-immediate `imms == levels` form.
fn decode_bit_masks(
    immn: u32,
    imms: u32,
    immr: u32,
    m: u32,
    immediate: bool,
) -> Option<(u64, u64)> {
    let combined = ((immn & 1) << 6) | ((!imms) & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros(); // HighestSetBit
    if len < 1 || (1u32 << len) > m {
        return None;
    }
    let levels = (1u32 << len) - 1;
    if immediate && (imms & levels) == levels {
        return None;
    }
    let s = imms & levels;
    let r = immr & levels;
    let diff = s.wrapping_sub(r) & levels;
    let esize = 1u32 << len;
    let welem = ones(s + 1);
    let telem = ones(diff + 1);
    let wmask = replicate(ror(welem, r, esize), esize);
    let tmask = replicate(telem, esize);
    Some((wmask, tmask))
}

/// `n` low bits set (`n` in 0..=64).
#[inline]
fn ones(n: u32) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// Replicate the low `esize` bits of `pattern` across the full 64-bit width.
fn replicate(pattern: u64, esize: u32) -> u64 {
    let pat = pattern & mask(esize);
    let mut out = 0u64;
    let mut i = 0;
    while i < 64 {
        out |= pat << i;
        i += esize;
    }
    out
}

/// Signed `width`-bit divide with the A64 result for divide-by-zero (0) and the
/// `INT_MIN / -1` overflow (`INT_MIN`).
fn sdiv(a: u64, b: u64, sf: bool) -> u64 {
    let width = if sf { 64 } else { 32 };
    let (a, b) = (sign_extend(a, width) as i64, sign_extend(b, width) as i64);
    if b == 0 {
        0
    } else {
        (a.wrapping_div(b)) as u64 & mask(width)
    }
}

/// Unsigned `width`-bit divide; divide-by-zero yields 0 (the A64 result).
fn udiv(a: u64, b: u64, sf: bool) -> u64 {
    let width = if sf { 64 } else { 32 };
    let (a, b) = (a & mask(width), b & mask(width));
    // Divide-by-zero yields 0 (the A64 result), never a host panic.
    a.checked_div(b).unwrap_or(0)
}

/// Signed 32×32 → 64 (the `SMADDL`/`SMSUBL` product).
fn smull(a: u64, b: u64) -> u64 {
    ((a as i32 as i64) * (b as i32 as i64)) as u64
}

/// Unsigned 32×32 → 64 (the `UMADDL`/`UMSUBL` product).
fn umull(a: u64, b: u64) -> u64 {
    (a as u32 as u64) * (b as u32 as u64)
}

/// The high 64 bits of a signed 64×64 product (`SMULH`).
fn smulh(a: u64, b: u64) -> u64 {
    (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64
}

/// The high 64 bits of an unsigned 64×64 product (`UMULH`).
fn umulh(a: u64, b: u64) -> u64 {
    (((a as u128) * (b as u128)) >> 64) as u64
}

/// Reverse the low `width` bits of `v` (`RBIT`).
fn rbit(v: u64, width: u32) -> u64 {
    let v = v & mask(width);
    v.reverse_bits() >> (64 - width)
}

/// Reverse the byte order of each `group`-byte unit of `v` (the `REV*` family).
fn rev_bytes(v: u64, group: u32) -> u64 {
    let mut out = 0u64;
    let bytes = group;
    for i in 0..bytes {
        let b = (v >> (8 * i)) & 0xff;
        out |= b << (8 * (bytes - 1 - i));
    }
    out
}

/// `REV16` — reverse bytes within each 16-bit halfword of the low `width` bits.
fn rev16(v: u64, width: u32) -> u64 {
    let mut out = 0u64;
    let mut i = 0;
    while i < width {
        let h = (v >> i) & 0xffff;
        let r = ((h & 0xff) << 8) | (h >> 8);
        out |= r << i;
        i += 16;
    }
    out & mask(width)
}

/// `REV32` (64-bit) — reverse bytes within each 32-bit word.
fn rev32(v: u64) -> u64 {
    let lo = rev_bytes(v & 0xffff_ffff, 4);
    let hi = rev_bytes(v >> 32, 4);
    (hi << 32) | lo
}

/// Count leading zeros within `width` bits (`CLZ`).
fn clz(v: u64, width: u32) -> u32 {
    let v = v & mask(width);
    if v == 0 {
        width
    } else {
        v.leading_zeros() - (64 - width)
    }
}

/// Count leading sign bits within `width` bits (`CLS`) — leading bits equal to
/// the sign bit, minus one.
fn cls(v: u64, width: u32) -> u32 {
    let v = v & mask(width);
    let sign = (v >> (width - 1)) & 1;
    let mut count = 0;
    let mut i = width - 1;
    loop {
        if i == 0 {
            break;
        }
        i -= 1;
        if (v >> i) & 1 == sign {
            count += 1;
        } else {
            break;
        }
    }
    count
}

// ════════════════════════════════════════════════════════════════════════════
//  CC-36 — the privileged AArch64 system: EL0/EL1 exception model, VMSAv8-64
//  paging, and the ARM `virt` platform (GICv2, the generic timer, a PL011
//  console, PSCI over SMC). With this installed (`Cpu::sys = Some`), the integer
//  core boots a real, unmodified `arm64` Linux to userspace — byte-identical to
//  `qemu-system-aarch64 -M virt` on the same image. The flat `CC-35` core
//  (`Cpu::sys = None`) is untouched.
// ════════════════════════════════════════════════════════════════════════════

// The ARM `virt` memory map (matching `qemu-system-aarch64 -M virt`, so the
// emulator and the qemu oracle line up). holospaces generates the devicetree
// describing it, so the same kernel boots on both.
const GICD_BASE: u64 = 0x0800_0000;
const GICD_END: u64 = 0x0801_0000;
const GICC_BASE: u64 = 0x0801_0000;
const GICC_END: u64 = 0x0802_0000;
const UART_BASE: u64 = 0x0900_0000;
const UART_END: u64 = 0x0900_1000;
const RAM_BASE: u64 = 0x4000_0000;
/// The kernel Image loads `text_offset` (0x80000) above the base of RAM, with the
/// devicetree placed safely above it (the kernel maps it by the physical address
/// in `x0`).
const KERNEL_OFFSET: u64 = 0x0008_0000;
const DTB_OFFSET: u64 = 0x0400_0000;

// The `virtio-mmio` transport slots (matching `qemu-system-aarch64 -M virt`:
// 0x0a00_0000, stride 0x200, SPI 16+). holospaces attaches the same κ-disk /
// 9p / network devices the RISC-V machine does (the shared [`devbus`]), here
// raising the GIC instead of the PLIC.
const VIRTIO_BLK_BASE: u64 = 0x0a00_0000;
const VIRTIO_BLK_END: u64 = 0x0a00_0200;
/// The second `virtio-mmio` slot (stride 0x200) — the VirtIO **9P** device (the
/// shared workspace filesystem, `CC-15`), serviced by the shared [`devbus`].
const VIRTIO_9P_BASE: u64 = 0x0a00_0200;
const VIRTIO_9P_END: u64 = 0x0a00_0400;
/// The third `virtio-mmio` slot — the VirtIO **network** device (`CC-16`): the
/// guest's frames terminate in the shared userspace TCP/IP NAT (`net`).
const VIRTIO_NET_BASE: u64 = 0x0a00_0400;
const VIRTIO_NET_END: u64 = 0x0a00_0600;

// GIC interrupt IDs. PPIs are 16..32 (the generic-timer lines); SPIs are 32+.
const INTID_CNTP: u32 = 30; // the EL1 physical timer (PPI 14)
const INTID_CNTV: u32 = 27; // the virtual timer (PPI 11)
                            // SPIs start at INTID 32; the devicetree `GIC_SPI n` maps to INTID 32 + n.
const SPI_BASE_INTID: u32 = 32;
// The `virtio-blk` SPI (devicetree `interrupts = <GIC_SPI 16 …>` → INTID 48).
const INTID_VIRTIO_BLK: u32 = 48;
// The `virtio-9p` SPI (`GIC_SPI 17` → INTID 49) and `virtio-net` (`GIC_SPI 18`
// → INTID 50) — the next two `virtio-mmio` slots, each its own GIC source.
const INTID_VIRTIO_9P: u32 = 49;
const INTID_VIRTIO_NET: u32 = 50;

/// The packed identifier of an AArch64 system register `(op0,op1,CRn,CRm,op2)` —
/// the `MRS`/`MSR` operand. A `u32` key for the dispatch table.
const fn sr(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    (op0 << 16) | (op1 << 13) | (crn << 9) | (crm << 5) | op2
}

// The boot-critical system registers (Arm-ARM D17).
const SR_MIDR: u32 = sr(3, 0, 0, 0, 0);
const SR_MPIDR: u32 = sr(3, 0, 0, 0, 5);
const SR_ID_AA64PFR0: u32 = sr(3, 0, 0, 4, 0);
const SR_ID_AA64DFR0: u32 = sr(3, 0, 0, 5, 0);
const SR_ID_AA64ISAR0: u32 = sr(3, 0, 0, 6, 0);
const SR_ID_AA64MMFR0: u32 = sr(3, 0, 0, 7, 0);
const SR_CTR: u32 = sr(3, 3, 0, 0, 1);
const SR_DCZID: u32 = sr(3, 3, 0, 0, 7);
const SR_SCTLR: u32 = sr(3, 0, 1, 0, 0);
const SR_TTBR0: u32 = sr(3, 0, 2, 0, 0);
const SR_TTBR1: u32 = sr(3, 0, 2, 0, 1);
const SR_TCR: u32 = sr(3, 0, 2, 0, 2);
const SR_VBAR: u32 = sr(3, 0, 12, 0, 0);
const SR_ESR: u32 = sr(3, 0, 5, 2, 0);
const SR_FAR: u32 = sr(3, 0, 6, 0, 0);
const SR_ELR: u32 = sr(3, 0, 4, 0, 1);
const SR_SPSR: u32 = sr(3, 0, 4, 0, 0);
const SR_SP_EL0: u32 = sr(3, 0, 4, 1, 0);
const SR_SP_EL1: u32 = sr(3, 4, 4, 1, 0);
const SR_CURRENTEL: u32 = sr(3, 0, 4, 2, 2);
const SR_DAIF: u32 = sr(3, 3, 4, 2, 1);
const SR_NZCV: u32 = sr(3, 3, 4, 2, 0);
const SR_SPSEL: u32 = sr(3, 0, 4, 2, 0);
const SR_CNTFRQ: u32 = sr(3, 3, 14, 0, 0);
const SR_CNTPCT: u32 = sr(3, 3, 14, 0, 1);
const SR_CNTVCT: u32 = sr(3, 3, 14, 0, 2);
const SR_CNTP_TVAL: u32 = sr(3, 3, 14, 2, 0);
const SR_CNTP_CTL: u32 = sr(3, 3, 14, 2, 1);
const SR_CNTP_CVAL: u32 = sr(3, 3, 14, 2, 2);
const SR_CNTV_TVAL: u32 = sr(3, 3, 14, 3, 0);
const SR_CNTV_CTL: u32 = sr(3, 3, 14, 3, 1);
const SR_CNTV_CVAL: u32 = sr(3, 3, 14, 3, 2);
const SR_PAR: u32 = sr(3, 0, 7, 4, 0);

/// The exception class an EL1 entry was taken for (selects the `ESR_EL1.EC` and
/// the vector offset).
#[derive(Clone, Copy)]
enum ExcKind {
    /// `SVC` from AArch64 (a syscall) — `EC` 0x15.
    Svc,
}

/// A simple direct-mapped software TLB over [`Cpu::translate`] — a cache of
/// virtual-page → physical-frame translations so a hot loop does not re-walk the
/// page table every access. Flushed (by bumping `gen`) on `TLBI`, a `TTBR`/`TCR`/
/// `SCTLR` write, and every exception/`ERET` (a context change).
#[derive(Clone, Copy)]
struct TlbEntry {
    tag: u64,
    frame: u64,
    gen: u64,
    /// The page is writable (descriptor `AP[2]` == 0). A store to a read-only
    /// page raises a permission fault — the write-protect fault the kernel uses
    /// for copy-on-write *and* dirty-bit tracking (no hardware AF/DBM here).
    writable: bool,
}

const TLB_SETS: usize = 1024;

/// The GICv2 distributor + CPU interface state (Arm GICv2 spec). Only the parts a
/// booting kernel drives are modelled: per-INTID enable/pending/active, the
/// distributor/CPU enables, and the priority mask — enough to deliver the timer
/// tick and the `virtio` SPIs.
struct Gic {
    dist_enable: bool,
    cpu_enable: bool,
    pmr: u32,
    enabled: [u64; 16],
    pending: [u64; 16],
    active: [u64; 16],
    cfg: [u32; 32],
}

impl Gic {
    fn new() -> Self {
        Gic {
            dist_enable: false,
            cpu_enable: false,
            pmr: 0,
            enabled: [0; 16],
            pending: [0; 16],
            active: [0; 16],
            cfg: [0; 32],
        }
    }
    fn set_bit(set: &mut [u64; 16], id: u32, on: bool) {
        let (w, b) = ((id / 64) as usize, id % 64);
        if w < 16 {
            if on {
                set[w] |= 1 << b;
            } else {
                set[w] &= !(1 << b);
            }
        }
    }
    fn get_bit(set: &[u64; 16], id: u32) -> bool {
        let (w, b) = ((id / 64) as usize, id % 64);
        w < 16 && set[w] & (1 << b) != 0
    }
    /// Raise an edge-triggered source (a `virtio` SPI): latch it pending.
    fn raise(&mut self, id: u32) {
        Self::set_bit(&mut self.pending, id, true);
    }
    /// Drive a level-triggered source (the generic timer) to `on`.
    fn set_level(&mut self, id: u32, on: bool) {
        Self::set_bit(&mut self.pending, id, on);
    }
    /// The highest-priority pending+enabled+inactive INTID, or `None`. Iterates
    /// only the 16 deliverable-mask words (`pending & enabled & !active`), using
    /// `trailing_zeros` to jump straight to the lowest set bit in each — instead of
    /// scanning all 1020 INTIDs one at a time on every interrupt check (the hot
    /// path: `irq_pending` runs per-instruction).
    fn best(&self) -> Option<u32> {
        for w in 0..16 {
            let deliverable = self.pending[w] & self.enabled[w] & !self.active[w];
            if deliverable != 0 {
                let id = (w as u32) * 64 + deliverable.trailing_zeros();
                // The top words may include bits past the 1020-INTID range; reject
                // those so a stray high bit cannot report a non-existent INTID.
                if id < 1020 {
                    return Some(id);
                }
            }
        }
        None
    }
    /// Whether the CPU IRQ line is asserted.
    fn irq_pending(&self) -> bool {
        self.dist_enable && self.cpu_enable && self.pmr > 0 && self.best().is_some()
    }
}

/// The PL011 UART (the console). Only the data + flag registers and the AMBA
/// PrimeCell identification registers matter — the latter so the kernel's `amba`
/// bus binds the full `ttyAMA0` driver (not just earlycon), giving `/dev/console`
/// for PID 1's stdout.
struct Pl011 {
    /// Pending input bytes (the terminal channel), and the next unread index.
    input: Vec<u8>,
    in_cursor: usize,
}

/// The privileged-mode state (`CC-36`): EL/PSTATE, the EL1 system registers, the
/// VMSAv8-64 translation context + TLB, and the `virt` platform devices.
struct Sys {
    el: u8,
    spsel: bool,
    /// `PSTATE.DAIF` held in bits [9:6] (`D,A,I,F`).
    daif: u64,
    sp_el0: u64,
    sp_el1: u64,
    elr_el1: u64,
    spsr_el1: u64,
    esr_el1: u64,
    far_el1: u64,
    vbar_el1: u64,
    sctlr_el1: u64,
    ttbr0_el1: u64,
    ttbr1_el1: u64,
    tcr_el1: u64,
    par_el1: u64,
    /// The long tail of EL1 control registers the kernel writes then reads
    /// (`MAIR`, `TPIDR*`, `CNTKCTL`, the performance-monitor controls, …), keyed
    /// by the packed system-register id.
    regs: BTreeMap<u32, u64>,
    /// A translation faulted: `(faulting VA, is-write)` — consumed by the abort.
    fault: Option<(u64, bool, bool)>,
    cntfrq: u64,
    counter: u64,
    cntp_ctl: u64,
    cntp_cval: u64,
    cntv_ctl: u64,
    cntv_cval: u64,
    cntvoff: u64,
    gic: Gic,
    uart: Pl011,
    /// The `virtio-blk` device — the κ-disk rootfs (`CC-7`), when a disk is
    /// attached (`CC-37`); shared with the RISC-V machine via the shared device bus (`devbus`).
    virtio: Option<super::VirtioBlk>,
    /// The `virtio-9p` device serving the shared workspace filesystem, when
    /// attached (`CC-15` parity, `CC-46`); `None` otherwise. The same shared
    /// [`devbus`] services it — no per-ISA re-implementation (Law L4).
    virtio9p: Option<super::Virtio9p>,
    /// The `virtio-net` device + the userspace TCP/IP NAT, when networking is
    /// attached (`CC-16` parity, `CC-46`); `None` for an offline machine.
    virtionet: Option<super::VirtioNet>,
    /// The host side of the in-process loopback bridge (ADR-020, `CC-33` parity),
    /// when the workbench dials guest listeners; `None` until
    /// [`Cpu::enable_loopback`] attaches it.
    loopback: Option<super::net::LoopbackHandle>,
    tlb: Vec<TlbEntry>,
    tlb_gen: u64,
    halted: bool,
    halt_status: u64,
}

impl Sys {
    fn new() -> Self {
        Sys {
            el: 1,
            spsel: true,    // EL1h
            daif: 0xf << 6, // start with interrupts masked, as on reset
            sp_el0: 0,
            sp_el1: 0,
            elr_el1: 0,
            spsr_el1: 0,
            esr_el1: 0,
            far_el1: 0,
            vbar_el1: 0,
            sctlr_el1: 0,
            ttbr0_el1: 0,
            ttbr1_el1: 0,
            tcr_el1: 0,
            par_el1: 0,
            regs: BTreeMap::new(),
            fault: None,
            cntfrq: 62_500_000,
            counter: 0,
            cntp_ctl: 0,
            cntp_cval: 0,
            cntv_ctl: 0,
            cntv_cval: 0,
            cntvoff: 0,
            gic: Gic::new(),
            virtio: None,
            virtio9p: None,
            virtionet: None,
            loopback: None,
            uart: Pl011 {
                input: Vec::new(),
                in_cursor: 0,
            },
            tlb: vec![
                TlbEntry {
                    tag: 0,
                    frame: 0,
                    gen: 0,
                    writable: false,
                };
                TLB_SETS
            ],
            tlb_gen: 1,
            halted: false,
            halt_status: 0,
        }
    }
}

impl Cpu {
    /// Boot a real, unmodified `arm64` Linux kernel `Image` to userspace on the
    /// ARM `virt` platform (`CC-36`). `bootargs` is the kernel command line. The
    /// emulator generates the devicetree from its own memory map (one source of
    /// truth), loads the kernel at `RAM_BASE + 0x80000` with `x0` = the DTB
    /// physical address, and enters at EL1 with the MMU off — the arm64 boot
    /// protocol (Documentation/arm64/booting.rst). Drive it with
    /// [`Cpu::run`] (a `PSCI SYSTEM_OFF` returns [`Halt::Exit`]).
    #[must_use]
    pub fn boot_linux(ram_bytes: usize, kernel: &[u8], bootargs: &str) -> Self {
        Self::boot_linux_inner(ram_bytes, kernel, bootargs, None, None, None)
    }

    /// Boot like [`Cpu::boot_linux`], additionally attaching a **`virtio-blk`
    /// root filesystem** (`CC-37`): `rootfs` is the assembled `ext4` image, taken
    /// as κ-addressed content into the κ-disk (`CC-7`), and
    /// the guest mounts it over `/dev/vda`. The shared the shared device bus (`devbus`) services
    /// the device — the same κ-disk the RISC-V machine boots, with no per-ISA
    /// re-implementation. Use a `bootargs` with `root=/dev/vda`.
    #[must_use]
    pub fn boot_linux_disk(
        ram_bytes: usize,
        kernel: &[u8],
        rootfs: Vec<u8>,
        bootargs: &str,
    ) -> Self {
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            bootargs,
            Some(super::VirtioBlk::new(rootfs)),
            None,
            None,
        )
    }

    /// Boot like [`Cpu::boot_linux_disk`], but page the κ-disk from a supplied
    /// [`KappaStore`](hologram_substrate_core::KappaStore) by **streaming**
    /// `sector_count` sectors from `read` (no full image in RAM) — the browser
    /// peer reads each sector from the OPFS rootfs into the OPFS-backed store, so a
    /// real arm64 image boots without ever materializing the whole `Vec` (the
    /// paged κ-disk, the same `KappaBacking` the RISC-V machine uses, `CC-7`).
    #[must_use]
    pub fn boot_linux_disk_streamed<R: FnMut(u64, &mut [u8])>(
        ram_bytes: usize,
        kernel: &[u8],
        bootargs: &str,
        store: alloc::boxed::Box<dyn hologram_substrate_core::KappaStore>,
        sector_count: u64,
        read: R,
    ) -> Self {
        let backing = super::KappaBacking::from_sectors(store, sector_count, read);
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            bootargs,
            Some(super::VirtioBlk::with_backing(backing)),
            None,
            None,
        )
    }

    /// Boot a real arm64 Linux over the **shared `emulator::devbus`** with the
    /// full devcontainer device complement — the `virtio-blk` κ-disk root **and**
    /// the shared `virtio-9p` workspace (`CC-15`) **and** the `virtio-net` device
    /// plus the userspace NAT (`CC-16`), each advertised in the devicetree so a real
    /// kernel probes and drives it. This is the AArch64 analogue of the RISC-V
    /// machine's [`MachineSpec::boot_workspace_net`](crate::machine::MachineSpec):
    /// the same one devbus, only the MMIO transport (GIC vs PLIC) differs (Law
    /// L4). The caller enables the in-process bridge (`CC-33`) with
    /// [`Cpu::enable_loopback`]. Use a `bootargs` with `root=/dev/vda`, `ip=dhcp`,
    /// and an `init` that mounts the 9p workspace and uses the network.
    #[must_use]
    pub fn boot_linux_devbus(
        ram_bytes: usize,
        kernel: &[u8],
        rootfs: Vec<u8>,
        seed: &[(&str, &[u8])],
        egress: Box<dyn super::net::Egress>,
        ingress: Box<dyn super::net::Ingress>,
        bootargs: &str,
    ) -> Self {
        let mut fs = super::ninep::Fs9p::new();
        for (name, data) in seed {
            fs.seed_file(name, data);
        }
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            bootargs,
            Some(super::VirtioBlk::new(rootfs)),
            Some(super::Virtio9p::new(fs, super::WORKSPACE_TAG)),
            Some(super::VirtioNet::new(egress, ingress)),
        )
    }

    fn boot_linux_inner(
        ram_bytes: usize,
        kernel: &[u8],
        bootargs: &str,
        disk: Option<super::VirtioBlk>,
        virtio9p: Option<super::Virtio9p>,
        virtionet: Option<super::VirtioNet>,
    ) -> Self {
        let mut cpu = Cpu::new(RAM_BASE, ram_bytes);
        // Load the kernel image at text_offset above the base of RAM.
        let load = KERNEL_OFFSET as usize;
        let n = kernel.len().min(cpu.ram.len() - load);
        cpu.ram[load..load + n].copy_from_slice(&kernel[..n]);
        // Place the devicetree above the kernel and hand its address in x0. Each
        // `virtio-mmio` slot is advertised only when its device is attached (an
        // unattached slot makes the kernel read magic 0 and stall).
        let has_blk = disk.is_some();
        let has_9p = virtio9p.is_some();
        let has_net = virtionet.is_some();
        let dtb = arm64_virt_dtb(
            RAM_BASE,
            ram_bytes as u64,
            bootargs,
            has_blk,
            has_9p,
            has_net,
        );
        let dtb_off = DTB_OFFSET as usize;
        let dn = dtb.len().min(cpu.ram.len() - dtb_off);
        cpu.ram[dtb_off..dtb_off + dn].copy_from_slice(&dtb[..dn]);
        cpu.x[0] = RAM_BASE + DTB_OFFSET;
        cpu.pc = RAM_BASE + KERNEL_OFFSET;
        cpu.sp = RAM_BASE + KERNEL_OFFSET;
        let mut sys = Sys::new();
        sys.virtio = disk;
        sys.virtio9p = virtio9p;
        sys.virtionet = virtionet;
        cpu.sys = Some(Box::new(sys));
        cpu
    }

    /// The current contents of the `virtio-blk` κ-disk as a flat image (the guest
    /// writes are reflected — observing the devcontainer's filesystem state).
    #[must_use]
    pub fn disk_image(&self) -> Option<Vec<u8>> {
        self.sys
            .as_ref()
            .and_then(|s| s.virtio.as_ref())
            .map(|d| d.disk.to_image())
    }

    /// Feed terminal input to the booted guest's console (the `CC-11` input
    /// channel, the AArch64 analogue): the bytes become readable at the PL011.
    pub fn feed_console(&mut self, bytes: &[u8]) {
        if let Some(sys) = self.sys.as_mut() {
            sys.uart.input.extend_from_slice(bytes);
        }
    }

    // ── shared workspace filesystem (virtio-9p; CC-15 parity, CC-46) ─────────

    /// Attach a shared **workspace filesystem** to the machine's VirtIO 9P device
    /// (`CC-15` parity). `seed` is the files holospaces places on the share (name
    /// → bytes); the guest mounts it (`-t 9p`, tag `hsworkspace`) and the editor
    /// and the running OS read and write the *same* files. The same shared
    /// `devbus` serves it as on the RISC-V machine — one 9P
    /// implementation, here over the GIC. No-op on the flat (`CC-35`) core.
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
    /// the edits the guest made over 9P (`CC-15`). `None` if no 9P device is
    /// attached or the file is absent.
    #[must_use]
    pub fn workspace_file(&self, name: &str) -> Option<&[u8]> {
        self.sys
            .as_ref()
            .and_then(|s| s.virtio9p.as_ref())
            .and_then(|d| d.fs.read_file(name))
    }

    /// List the shared workspace's root entries — `(name, is_dir, size)` — the
    /// editor's directory view over the running holospace (`CC-17`).
    #[must_use]
    pub fn workspace_list(&self) -> Vec<(String, bool, usize)> {
        self.sys
            .as_ref()
            .and_then(|s| s.virtio9p.as_ref())
            .map(|d| d.fs.list_root())
            .unwrap_or_default()
    }

    /// Write a file into the shared workspace — the editor saving content the
    /// running OS reads over `virtio-9p` (one content, Law L1; `CC-17`).
    pub fn workspace_write(&mut self, name: &str, data: &[u8]) {
        if let Some(d) = self.sys.as_mut().and_then(|s| s.virtio9p.as_mut()) {
            d.fs.write_file(name, data);
        }
    }

    // ── network device + the in-process bridge (CC-16/CC-33 parity, CC-46) ───

    /// Attach the **VirtIO network device** + the userspace TCP/IP NAT (`CC-16`
    /// parity): the guest drives a real NIC, its frames terminate in the shared
    /// [`net`](super::net) NAT and stream out over `egress`. No-op on the flat
    /// (`CC-35`) core.
    pub fn attach_net(&mut self, egress: Box<dyn super::net::Egress>) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtionet = Some(super::VirtioNet::new(
                egress,
                Box::new(super::net::NoIngress),
            ));
        }
    }

    /// Attach the network device with both an `egress` (outbound) and an
    /// `ingress` (forwarded-port inbound) transport (`CC-16` + `CC-21` parity).
    pub fn attach_net_forward(
        &mut self,
        egress: Box<dyn super::net::Egress>,
        ingress: Box<dyn super::net::Ingress>,
    ) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtionet = Some(super::VirtioNet::new(egress, ingress));
        }
    }

    /// Enable the **in-process loopback bridge** (ADR-020, `CC-33` parity) on the
    /// already-attached network device: the workbench (same process) can
    /// `dial`/`send`/`recv`/`close` a connection to a server *inside* the guest —
    /// the inward in-process dual of the egress relay. Keeps the existing egress.
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
    /// loopback ingress is not enabled. Pump the machine (`run`) so the NAT opens
    /// the connection toward the guest and the byte stream flows.
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

    /// Drain the guest server's reply bytes on a loopback connection (empty if
    /// none have arrived yet — pump the machine to advance the stream).
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

    /// Whether a loopback connection is still usable (the guest has not closed it,
    /// or has but unread bytes remain).
    #[must_use]
    pub fn guest_is_open(&self, id: u32) -> bool {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .is_some_and(|h| h.is_open(id))
    }

    /// **Live** network reconfiguration (`CC-28` parity): begin forwarding
    /// `guest_port` on the running machine. Returns the host port, or `None` if
    /// the machine has no network device or its ingress cannot add a forward.
    pub fn forward_port(&mut self, guest_port: u16) -> Option<u16> {
        self.sys
            .as_mut()
            .and_then(|s| s.virtionet.as_mut())
            .and_then(|n| n.ingress.add_forward(guest_port))
    }

    #[inline]
    fn sys(&self) -> &Sys {
        self.sys.as_ref().expect("system mode")
    }
    #[inline]
    fn sys_mut(&mut self) -> &mut Sys {
        self.sys.as_mut().expect("system mode")
    }

    // ── physical memory + device MMIO ───────────────────────────────────────

    fn in_ram(&self, pa: u64, width: usize) -> bool {
        pa >= self.base && pa.wrapping_add(width as u64) <= self.base + self.ram.len() as u64
    }

    fn phys_read(&mut self, pa: u64, width: usize) -> u64 {
        if self.in_ram(pa, width) {
            let o = (pa - self.base) as usize;
            let mut v = 0u64;
            for i in 0..width {
                v |= u64::from(self.ram[o + i]) << (8 * i);
            }
            return v;
        }
        if (UART_BASE..UART_END).contains(&pa) {
            return self.uart_read(pa - UART_BASE);
        }
        if (GICD_BASE..GICD_END).contains(&pa) {
            return self.gicd_read(pa - GICD_BASE);
        }
        if (GICC_BASE..GICC_END).contains(&pa) {
            return self.gicc_read(pa - GICC_BASE);
        }
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

    fn phys_write(&mut self, pa: u64, width: usize, value: u64) {
        if self.in_ram(pa, width) {
            let o = (pa - self.base) as usize;
            for i in 0..width {
                self.ram[o + i] = (value >> (8 * i)) as u8;
            }
            return;
        }
        if (UART_BASE..UART_END).contains(&pa) {
            self.uart_write(pa - UART_BASE, value);
        } else if (GICD_BASE..GICD_END).contains(&pa) {
            self.gicd_write(pa - GICD_BASE, value as u32);
        } else if (GICC_BASE..GICC_END).contains(&pa) {
            self.gicc_write(pa - GICC_BASE, value as u32);
        } else if (VIRTIO_BLK_BASE..VIRTIO_BLK_END).contains(&pa) {
            self.virtio_blk_write(pa - VIRTIO_BLK_BASE, value as u32);
        } else if (VIRTIO_9P_BASE..VIRTIO_9P_END).contains(&pa) {
            self.virtio_9p_write(pa - VIRTIO_9P_BASE, value as u32);
        } else if (VIRTIO_NET_BASE..VIRTIO_NET_END).contains(&pa) {
            self.virtio_net_write(pa - VIRTIO_NET_BASE, value as u32);
        }
    }

    /// A `virtio-blk` MMIO register write; a `QueueNotify` services the queue
    /// against the κ-disk (the shared the shared device bus (`devbus`)) and latches the GIC SPI.
    fn virtio_blk_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio.take() else {
            return;
        };
        let notify = super::devbus::blk_mmio_write(&mut dev, off, value);
        let mut raise = false;
        if notify {
            let mut mem = super::devbus::GuestRam {
                ram: &mut self.ram,
                base: self.base,
            };
            raise = super::devbus::blk_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio = Some(dev);
        if raise {
            self.sys_mut().gic.raise(INTID_VIRTIO_BLK);
        }
    }

    /// A `virtio-9p` MMIO register write; a `QueueNotify` services the workspace
    /// filesystem queue through the shared [`devbus`] and latches the GIC SPI —
    /// the same servicing the RISC-V machine drives, here over the GIC (`CC-46`).
    fn virtio_9p_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio9p.take() else {
            return;
        };
        let notify = super::devbus::p9_mmio_write(&mut dev, off, value);
        let mut raise = false;
        if notify {
            let mut mem = super::devbus::GuestRam {
                ram: &mut self.ram,
                base: self.base,
            };
            raise = super::devbus::p9_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio9p = Some(dev);
        if raise {
            self.sys_mut().gic.raise(INTID_VIRTIO_9P);
        }
    }

    /// A `virtio-net` MMIO register write; a `QueueNotify` services the transmit
    /// queue or pumps the NAT through the shared [`devbus`] and latches the GIC
    /// SPI (`CC-46`).
    fn virtio_net_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtionet.take() else {
            return;
        };
        let notify = super::devbus::net_mmio_write(&mut dev, off, value);
        let mut raise = false;
        match notify {
            super::devbus::NetNotify::Transmit => {
                let mut mem = super::devbus::GuestRam {
                    ram: &mut self.ram,
                    base: self.base,
                };
                raise |= super::devbus::net_service_tx(&mut mem, &mut dev);
                raise |= super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::Receive => {
                let mut mem = super::devbus::GuestRam {
                    ram: &mut self.ram,
                    base: self.base,
                };
                raise |= super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::None => {}
        }
        self.sys_mut().virtionet = Some(dev);
        if raise {
            self.sys_mut().gic.raise(INTID_VIRTIO_NET);
        }
    }

    /// Pump the NAT and deliver pending receive frames into the guest's receive
    /// queue — called periodically from the run loop so host-side data arrives
    /// without the guest having to transmit first (the AArch64 analogue of the
    /// RISC-V `virtio_net_pump`, the same shared [`devbus`]).
    fn virtio_net_pump(&mut self) {
        let Some(mut dev) = self.sys_mut().virtionet.take() else {
            return;
        };
        let mut mem = super::devbus::GuestRam {
            ram: &mut self.ram,
            base: self.base,
        };
        let raise = super::devbus::net_pump(&mut mem, &mut dev);
        self.sys_mut().virtionet = Some(dev);
        if raise {
            self.sys_mut().gic.raise(INTID_VIRTIO_NET);
        }
    }

    /// Read a 64-bit little-endian word from physical RAM (the page-table walker).
    fn phys_read64(&self, pa: u64) -> u64 {
        if pa >= self.base && pa + 8 <= self.base + self.ram.len() as u64 {
            let o = (pa - self.base) as usize;
            u64::from_le_bytes(self.ram[o..o + 8].try_into().unwrap())
        } else {
            0
        }
    }

    // ── PL011 UART ──────────────────────────────────────────────────────────

    fn uart_read(&mut self, off: u64) -> u64 {
        match off {
            0x00 => {
                // DR — the next input byte (0 if none).
                let sys = self.sys_mut();
                if sys.uart.in_cursor < sys.uart.input.len() {
                    let b = sys.uart.input[sys.uart.in_cursor];
                    sys.uart.in_cursor += 1;
                    u64::from(b)
                } else {
                    0
                }
            }
            0x18 => {
                // FR — flags: RXFE (bit4) set when no input; TX always ready.
                let sys = self.sys();
                let rxfe = if sys.uart.in_cursor < sys.uart.input.len() {
                    0
                } else {
                    1 << 4
                };
                (1 << 7) | rxfe // TXFE | maybe RXFE
            }
            // AMBA PrimeCell ID — so the kernel's `amba` bus binds `ttyAMA0`.
            0xfe0 => 0x11,
            0xfe4 => 0x10,
            0xfe8 => 0x14,
            0xfec => 0x00,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 0x05,
            0xffc => 0xb1,
            _ => 0,
        }
    }

    fn uart_write(&mut self, off: u64, value: u64) {
        if off == 0x00 {
            // DR — emit the byte to the console.
            self.console.push(value as u8);
        }
        // The control/baud/interrupt registers are accepted and ignored — the
        // model is a polled console (no UART interrupt needed to boot).
    }

    // ── GICv2 ───────────────────────────────────────────────────────────────

    fn gicd_read(&mut self, off: u64) -> u64 {
        let g = &self.sys().gic;
        match off {
            0x000 => u64::from(g.dist_enable), // GICD_CTLR
            0x004 => 0x0000_0001,              // GICD_TYPER: ITLinesNumber=1 → 64 INTIDs
            0x008 => 0x0200_043b,              // GICD_IIDR (an ARM GICv2)
            0x100..=0x17c => {
                // GICD_ISENABLER<n>
                let n = (off - 0x100) / 4;
                self.gic_bits(&self.sys().gic.enabled, n)
            }
            0x180..=0x1fc => {
                let n = (off - 0x180) / 4;
                self.gic_bits(&self.sys().gic.enabled, n)
            }
            0xc00..=0xc7c => {
                let i = ((off - 0xc00) / 4) as usize;
                u64::from(self.sys().gic.cfg.get(i).copied().unwrap_or(0))
            }
            _ => 0,
        }
    }

    fn gic_bits(&self, set: &[u64; 16], n: u64) -> u64 {
        let base = (n * 32) as u32;
        let mut v = 0u64;
        for i in 0..32 {
            if Gic::get_bit(set, base + i) {
                v |= 1 << i;
            }
        }
        v
    }

    fn gicd_write(&mut self, off: u64, value: u32) {
        match off {
            0x000 => self.sys_mut().gic.dist_enable = value & 1 != 0,
            0x100..=0x17c => {
                let base = (((off - 0x100) / 4) * 32) as u32;
                for i in 0..32 {
                    if value & (1 << i) != 0 {
                        Gic::set_bit(&mut self.sys_mut().gic.enabled, base + i, true);
                    }
                }
            }
            0x180..=0x1fc => {
                let base = (((off - 0x180) / 4) * 32) as u32;
                for i in 0..32 {
                    if value & (1 << i) != 0 {
                        Gic::set_bit(&mut self.sys_mut().gic.enabled, base + i, false);
                    }
                }
            }
            0x200..=0x27c => {
                let base = (((off - 0x200) / 4) * 32) as u32;
                for i in 0..32 {
                    if value & (1 << i) != 0 {
                        Gic::set_bit(&mut self.sys_mut().gic.pending, base + i, true);
                    }
                }
            }
            0x280..=0x2fc => {
                let base = (((off - 0x280) / 4) * 32) as u32;
                for i in 0..32 {
                    if value & (1 << i) != 0 {
                        Gic::set_bit(&mut self.sys_mut().gic.pending, base + i, false);
                    }
                }
            }
            0xc00..=0xc7c => {
                let i = ((off - 0xc00) / 4) as usize;
                if i < 32 {
                    self.sys_mut().gic.cfg[i] = value;
                }
            }
            _ => {}
        }
    }

    fn gicc_read(&mut self, off: u64) -> u64 {
        match off {
            0x00 => u64::from(self.sys().gic.cpu_enable), // GICC_CTLR
            0x04 => u64::from(self.sys().gic.pmr),        // GICC_PMR
            0x0c => {
                // GICC_IAR — acknowledge the highest pending interrupt.
                match self.sys().gic.best() {
                    Some(id) => {
                        Gic::set_bit(&mut self.sys_mut().gic.active, id, true);
                        // Edge sources clear on acknowledge; level sources are
                        // re-evaluated by `sys_tick`.
                        Gic::set_bit(&mut self.sys_mut().gic.pending, id, false);
                        u64::from(id)
                    }
                    None => 1023, // spurious
                }
            }
            0xfc => 0x0000_043b, // GICC_IIDR
            _ => 0,
        }
    }

    fn gicc_write(&mut self, off: u64, value: u32) {
        match off {
            0x00 => self.sys_mut().gic.cpu_enable = value & 1 != 0,
            0x04 => self.sys_mut().gic.pmr = value,
            0x10 => {
                // GICC_EOIR — end of interrupt: clear the active bit.
                let id = value & 0x3ff;
                Gic::set_bit(&mut self.sys_mut().gic.active, id, false);
            }
            _ => {}
        }
    }

    // ── the generic timer + interrupt delivery ──────────────────────────────

    /// Advance the architected counter and re-evaluate the timer interrupt lines
    /// (level-triggered through the GIC). Called once per instruction step.
    fn sys_tick(&mut self) {
        let sys = self.sys_mut();
        sys.counter = sys.counter.wrapping_add(1);
        let counter = sys.counter;
        // EL1 physical timer (CNTP) → PPI INTID 30.
        let p_fire = sys.cntp_ctl & 1 != 0 && sys.cntp_ctl & 2 == 0 && counter >= sys.cntp_cval;
        if p_fire {
            sys.cntp_ctl |= 4;
        } else {
            sys.cntp_ctl &= !4;
        }
        // Virtual timer (CNTV) → PPI INTID 27.
        let vcount = counter.wrapping_sub(sys.cntvoff);
        let v_fire = sys.cntv_ctl & 1 != 0 && sys.cntv_ctl & 2 == 0 && vcount >= sys.cntv_cval;
        if v_fire {
            sys.cntv_ctl |= 4;
        } else {
            sys.cntv_ctl &= !4;
        }
        sys.gic.set_level(INTID_CNTP, p_fire);
        sys.gic.set_level(INTID_CNTV, v_fire);
    }

    /// Deliver a pending IRQ if the line is asserted and `PSTATE.I` is clear.
    /// Returns `true` when an exception was taken (the caller re-fetches from the
    /// vector).
    fn take_pending_interrupt(&mut self) -> bool {
        if self.sys().halted {
            return false;
        }
        let i_masked = self.sys().daif & (1 << 7) != 0;
        if i_masked || !self.sys().gic.irq_pending() {
            return false;
        }
        let ret = self.pc;
        self.take_to_el1(None, 0, ret, 0x80); // IRQ vector offset
        true
    }

    // ── the EL1 exception model ─────────────────────────────────────────────

    /// `PSTATE` packed into the `SPSR_EL1` layout (`NZCV` + `DAIF` + the mode
    /// field `M[3:0]`).
    fn pack_pstate(&self) -> u64 {
        let f = self.flags;
        let nzcv = (u64::from(f.n) << 31)
            | (u64::from(f.z) << 30)
            | (u64::from(f.c) << 29)
            | (u64::from(f.v) << 28);
        let el = self.sys().el as u64;
        let m = (el << 2) | u64::from(el == 1 && self.sys().spsel);
        nzcv | self.sys().daif | m
    }

    /// Switch the current (EL, SPSel) context, banking `SP` (`SP_EL0`/`SP_EL1`).
    fn set_context(&mut self, new_el: u8, new_spsel: bool) {
        let cur_uses_el0 = self.sys().el == 0 || !self.sys().spsel;
        if cur_uses_el0 {
            self.sys_mut().sp_el0 = self.sp;
        } else {
            self.sys_mut().sp_el1 = self.sp;
        }
        self.sys_mut().el = new_el;
        self.sys_mut().spsel = new_spsel;
        let new_uses_el0 = new_el == 0 || !new_spsel;
        self.sp = if new_uses_el0 {
            self.sys().sp_el0
        } else {
            self.sys().sp_el1
        };
    }

    /// Take an exception to EL1: bank PSTATE into `SPSR_EL1`/`ELR_EL1`, set
    /// `ESR_EL1` (for a synchronous entry), switch to EL1h with interrupts masked,
    /// and branch to the right `VBAR_EL1` vector. `class_off` selects sync (0x00)
    /// vs IRQ (0x80) within the source-EL group.
    fn take_to_el1(&mut self, kind: Option<ExcKind>, iss: u32, ret: u64, class_off: u64) {
        let from_el0 = self.sys().el == 0;
        let spsr = self.pack_pstate();
        let group = if from_el0 {
            0x400
        } else if self.sys().spsel {
            0x200
        } else {
            0x000
        };
        let target = self.sys().vbar_el1.wrapping_add(group + class_off);
        self.sys_mut().elr_el1 = ret;
        self.sys_mut().spsr_el1 = spsr;
        if let Some(k) = kind {
            let ec = match k {
                ExcKind::Svc => 0x15u64,
            };
            self.sys_mut().esr_el1 = (ec << 26) | (1 << 25) | (u64::from(iss) & 0x1ff_ffff);
        }
        self.excl = None;
        self.set_context(1, true);
        self.sys_mut().daif = 0xf << 6; // mask D,A,I,F on entry
        self.bump_tlb();
        self.pc = target;
    }

    /// Take a memory abort (instruction or data) to EL1, building `ESR_EL1`/`FAR_EL1`.
    fn take_mem_abort(
        &mut self,
        ret: u64,
        far: u64,
        is_fetch: bool,
        is_write: bool,
        is_perm: bool,
    ) {
        let from_el0 = self.sys().el == 0;
        let spsr = self.pack_pstate();
        let group = if from_el0 {
            0x400
        } else if self.sys().spsel {
            0x200
        } else {
            0x000
        };
        let target = self.sys().vbar_el1.wrapping_add(group);
        // EC: instruction abort 0x20/0x21, data abort 0x24/0x25 (lower/same EL).
        let ec: u64 = match (is_fetch, from_el0) {
            (true, true) => 0x20,
            (true, false) => 0x21,
            (false, true) => 0x24,
            (false, false) => 0x25,
        };
        // ISS DFSC: a permission fault (level 3) for a write-protect violation
        // (copy-on-write / dirty tracking), otherwise a translation fault (level
        // 1). The write bit (WnR) is set for write data aborts.
        let dfsc = if is_perm { 0b001111u64 } else { 0b000101u64 };
        let iss = dfsc | (if !is_fetch && is_write { 1 << 6 } else { 0 });
        self.sys_mut().elr_el1 = ret;
        self.sys_mut().spsr_el1 = spsr;
        self.sys_mut().esr_el1 = (ec << 26) | (1 << 25) | iss;
        self.sys_mut().far_el1 = far;
        self.sys_mut().fault = None;
        self.excl = None;
        self.set_context(1, true);
        self.sys_mut().daif = 0xf << 6;
        self.bump_tlb();
        self.pc = target;
    }

    /// Take an "unknown instruction" exception (`EC` 0x00) to EL1.
    fn take_undef(&mut self, ret: u64, _inst: u32) {
        let from_el0 = self.sys().el == 0;
        let spsr = self.pack_pstate();
        let group = if from_el0 {
            0x400
        } else if self.sys().spsel {
            0x200
        } else {
            0x000
        };
        let target = self.sys().vbar_el1.wrapping_add(group);
        self.sys_mut().elr_el1 = ret;
        self.sys_mut().spsr_el1 = spsr;
        self.sys_mut().esr_el1 = 1 << 25; // EC=0 (unknown), IL=1
        self.excl = None;
        self.set_context(1, true);
        self.sys_mut().daif = 0xf << 6;
        self.bump_tlb();
        self.pc = target;
    }

    /// Route an `exec` trap in system mode to the right exception: a translation
    /// fault recorded by the MMU becomes a data abort; anything else is undefined.
    fn take_exec_trap(&mut self, pc: u64, inst: u32, _t: Trap) {
        if let Some((far, is_write, is_perm)) = self.sys().fault {
            self.take_mem_abort(pc, far, false, is_write, is_perm);
        } else {
            self.take_undef(pc, inst);
        }
    }

    /// `ERET` — return from EL1 to the EL/PSTATE saved in `SPSR_EL1`, at `ELR_EL1`.
    fn eret(&mut self) {
        let spsr = self.sys().spsr_el1;
        let m = spsr & 0x1f;
        let new_el = ((m >> 2) & 0x3) as u8;
        let new_spsel = m & 1 == 1;
        self.flags = Nzcv {
            n: spsr & (1 << 31) != 0,
            z: spsr & (1 << 30) != 0,
            c: spsr & (1 << 29) != 0,
            v: spsr & (1 << 28) != 0,
        };
        self.sys_mut().daif = spsr & (0xf << 6);
        self.excl = None;
        self.set_context(new_el, new_spsel);
        self.bump_tlb();
        self.pc = self.sys().elr_el1;
    }

    // ── the VMSAv8-64 MMU ───────────────────────────────────────────────────

    fn bump_tlb(&mut self) {
        self.sys_mut().tlb_gen = self.sys_mut().tlb_gen.wrapping_add(1);
    }

    /// Translate a virtual address to a physical address (Arm-ARM VMSAv8-64, 4 KiB
    /// granule). With the MMU off (`SCTLR_EL1.M == 0`) translation is the identity
    /// — the boot protocol's initial state. Permission/attribute checks are not
    /// enforced (the kernel's own mappings are correct, and the boot path reaches
    /// userspace without relying on them) — only translation faults are raised, so
    /// the kernel's demand-paging handler runs.
    fn translate(&mut self, va: u64, acc: Access) -> Result<u64, Trap> {
        if self.sys().sctlr_el1 & 1 == 0 {
            return Ok(va); // MMU off → identity
        }
        let page = va & !0xfff;
        let set = (page >> 12) as usize & (TLB_SETS - 1);
        let gen = self.sys().tlb_gen;
        let e = self.sys().tlb[set];
        if e.gen == gen && e.tag == page {
            if acc == Access::Store && !e.writable {
                // Write to a read-only page → write-permission fault (copy-on-write
                // / dirty-bit tracking). The kernel resolves it and re-executes.
                self.sys_mut().fault = Some((va, true, true));
                return Err(Trap::AccessFault(va));
            }
            return Ok(e.frame | (va & 0xfff));
        }
        match self.walk(va) {
            Some((pa, writable)) => {
                self.sys_mut().tlb[set] = TlbEntry {
                    tag: page,
                    frame: pa & !0xfff,
                    gen,
                    writable,
                };
                if acc == Access::Store && !writable {
                    self.sys_mut().fault = Some((va, true, true));
                    return Err(Trap::AccessFault(va));
                }
                Ok(pa)
            }
            None => {
                let is_write = acc == Access::Store;
                self.sys_mut().fault = Some((va, is_write, false));
                Err(Trap::AccessFault(va))
            }
        }
    }

    /// Walk the page tables (4 KiB granule). Returns the physical address, or
    /// `None` on a translation fault.
    /// Walk the page tables for `va`, returning `(physical address, writable)`.
    /// `writable` is the descriptor's `AP[2]` bit inverted (0 = read/write).
    fn walk(&self, va: u64) -> Option<(u64, bool)> {
        let tcr = self.sys().tcr_el1;
        let t0sz = (tcr & 0x3f) as u32;
        let t1sz = ((tcr >> 16) & 0x3f) as u32;
        // Select TTBR by the top bits: all-zero → TTBR0, all-one → TTBR1.
        let (ttbr, tnsz) = if va >> 63 == 0 {
            (self.sys().ttbr0_el1, t0sz)
        } else {
            (self.sys().ttbr1_el1, t1sz)
        };
        let va_bits = 64 - tnsz;
        // The initial lookup level (4 KiB granule): resolve 9 bits per level.
        let n_levels = (va_bits - 12).div_ceil(9);
        let start = 4u32.saturating_sub(n_levels);
        let mut table = ttbr & 0x0000_ffff_ffff_f000;
        let mut level = start;
        loop {
            let shift = 12 + 9 * (3 - level);
            let idx = (va >> shift) & 0x1ff;
            let desc = self.phys_read64(table + idx * 8);
            if desc & 1 == 0 {
                return None; // invalid descriptor → translation fault
            }
            let is_block = desc & 2 == 0; // bits[1:0]==01 block, ==11 table/page
            if level == 3 {
                // Level 3: a page descriptor (bits[1:0] must be 11).
                if desc & 2 == 0 {
                    return None;
                }
                let out = desc & 0x0000_ffff_ffff_f000;
                return Some((out | (va & 0xfff), (desc >> 7) & 1 == 0));
            }
            if is_block {
                let blk_mask = (1u64 << shift) - 1;
                let out = desc & !blk_mask & 0x0000_ffff_ffff_ffff;
                return Some((out | (va & blk_mask), (desc >> 7) & 1 == 0));
            }
            table = desc & 0x0000_ffff_ffff_f000;
            level += 1;
        }
    }

    // ── the System instruction class (MRS/MSR/SYS/hints/barriers) ───────────

    fn system_instr(&mut self, inst: u32, next: u64) -> Result<(), Halt> {
        let l = (inst >> 21) & 1;
        let op0 = (inst >> 19) & 0x3;
        let op1 = (inst >> 16) & 0x7;
        let crn = (inst >> 12) & 0xf;
        let crm = (inst >> 8) & 0xf;
        let op2 = (inst >> 5) & 0x7;
        let rt = inst & 0x1f;

        if op0 == 0 && l == 0 {
            // MSR (immediate), hints, and barriers.
            match crn {
                0x2 => {
                    // HINT space: WFI/WFE/YIELD/NOP/…
                    if crm == 0 && op2 == 3 {
                        self.wfi();
                    }
                    self.pc = next;
                    return Ok(());
                }
                0x3 => {
                    // Barriers (DSB/DMB/ISB/CLREX/SB): ordering no-ops; CLREX
                    // clears the monitor.
                    if op2 == 2 {
                        self.excl = None;
                    }
                    self.pc = next;
                    return Ok(());
                }
                0x4 => {
                    // MSR (immediate) to a PSTATE field.
                    if op1 == 0 && op2 == 5 {
                        // SPSel
                        self.set_spsel(crm & 1 == 1);
                    } else if op1 == 3 && op2 == 6 {
                        // DAIFSet — set the masked bits.
                        let mut bits = 0u64;
                        for i in 0..4 {
                            if crm & (1 << i) != 0 {
                                bits |= 1 << (6 + i);
                            }
                        }
                        self.sys_mut().daif |= bits;
                    } else if op1 == 3 && op2 == 7 {
                        // DAIFClr
                        let mut bits = 0u64;
                        for i in 0..4 {
                            if crm & (1 << i) != 0 {
                                bits |= 1 << (6 + i);
                            }
                        }
                        self.sys_mut().daif &= !bits;
                    }
                    self.pc = next;
                    return Ok(());
                }
                _ => {
                    self.pc = next;
                    return Ok(());
                }
            }
        }

        if op0 == 1 {
            // SYS/SYSL: TLBI / IC / DC / AT.
            if crn == 0x8 {
                // TLBI — invalidate the TLB.
                self.bump_tlb();
            } else if crn == 0x7 && (crm == 0x8 || crm == 0x9) {
                // AT (address translate) — record the result in PAR_EL1.
                let va = self.rx(rt);
                match self.walk(va) {
                    Some((pa, _)) => self.sys_mut().par_el1 = pa & 0x0000_ffff_ffff_f000,
                    None => self.sys_mut().par_el1 = 1, // F bit: translation aborted
                }
            } else if crn == 0x7 && crm == 0x4 && op2 == 1 {
                // DC ZVA — Data Cache Zero by VA. Unlike the other cache ops this
                // *writes memory*: it zeroes the DCZID-block (64-byte, BS=4)
                // containing the address in Xt. The ecosystem's `memset` and the
                // kernel's `clear_page` use it on the fast path (DCZID.DZP=0), so
                // it MUST zero — a no-op leaves heap/.bss/stack pages full of
                // garbage and corrupts stack canaries (CC-37).
                let base = self.rx(rt) & !63;
                for i in 0..8u64 {
                    if let Err(t) = self.write(base.wrapping_add(i * 8), 8, 0) {
                        // Fault on an unmapped page → take the data abort; the
                        // kernel demand-pages and re-executes DC ZVA.
                        let (far, is_perm) =
                            self.sys().fault.map_or((base, false), |(f, _, p)| (f, p));
                        self.take_mem_abort(self.pc, far, false, true, is_perm);
                        let _ = t;
                        return Ok(());
                    }
                }
            }
            // Remaining IC/DC cache ops are no-ops on a coherent interpreter.
            self.pc = next;
            return Ok(());
        }

        // MRS / MSR (register).
        let id = sr(op0, op1, crn, crm, op2);
        if l == 1 {
            let v = self.sysreg_read(id);
            self.wx(rt, v);
        } else {
            let v = self.rx(rt);
            self.sysreg_write(id, v);
        }
        self.pc = next;
        Ok(())
    }

    fn set_spsel(&mut self, s: bool) {
        if self.sys().spsel != s {
            let el = self.sys().el;
            self.set_context(el, s);
        }
    }

    /// `WFI` — wait for interrupt. If none is pending, fast-forward the architected
    /// counter to the next enabled timer deadline so the tick fires on the next
    /// step (rather than spinning the idle loop against the step budget).
    fn wfi(&mut self) {
        if self.sys().gic.irq_pending() {
            return;
        }
        let sys = self.sys_mut();
        let mut deadline = u64::MAX;
        if sys.cntp_ctl & 1 != 0 && sys.cntp_ctl & 2 == 0 {
            deadline = deadline.min(sys.cntp_cval);
        }
        if sys.cntv_ctl & 1 != 0 && sys.cntv_ctl & 2 == 0 {
            deadline = deadline.min(sys.cntv_cval.wrapping_add(sys.cntvoff));
        }
        if deadline != u64::MAX && deadline > sys.counter {
            sys.counter = deadline - 1; // the next sys_tick increments past it
        }
    }

    fn sysreg_read(&mut self, id: u32) -> u64 {
        match id {
            SR_MIDR => 0x411f_d070,         // Cortex-A57 r1p0
            SR_MPIDR => 0x8000_0000,        // affinity 0, RES1 bit31
            SR_ID_AA64PFR0 => 0x0000_0011,  // EL0/EL1 AArch64; FP+SIMD present
            SR_ID_AA64DFR0 => 0x0000_0006,  // ARMv8 debug architecture
            SR_ID_AA64ISAR0 => 0,           // no LSE atomics → LL/SC locks
            SR_ID_AA64MMFR0 => 0x0000_1122, // 40-bit PA, 16-bit ASID, 4 KiB granule
            SR_CTR => 0x8444_8004,          // cache type (Cortex-A57)
            SR_DCZID => 0x4, // DC ZVA permitted (DZP=0), block = 4<<4 = 64 bytes (BS=4, Cortex-A57)
            SR_SCTLR => self.sys().sctlr_el1,
            SR_TTBR0 => self.sys().ttbr0_el1,
            SR_TTBR1 => self.sys().ttbr1_el1,
            SR_TCR => self.sys().tcr_el1,
            SR_VBAR => self.sys().vbar_el1,
            SR_ESR => self.sys().esr_el1,
            SR_FAR => self.sys().far_el1,
            SR_ELR => self.sys().elr_el1,
            SR_SPSR => self.sys().spsr_el1,
            SR_PAR => self.sys().par_el1,
            SR_SP_EL0 => {
                if self.sys().el == 1 && self.sys().spsel {
                    self.sys().sp_el0
                } else {
                    self.sp
                }
            }
            SR_SP_EL1 => {
                if self.sys().spsel {
                    self.sp
                } else {
                    self.sys().sp_el1
                }
            }
            SR_CURRENTEL => u64::from(self.sys().el) << 2,
            SR_DAIF => self.sys().daif,
            SR_NZCV => u64::from(self.flags.pack()),
            SR_SPSEL => u64::from(self.sys().spsel),
            SR_CNTFRQ => self.sys().cntfrq,
            SR_CNTPCT => self.sys().counter,
            SR_CNTVCT => self.sys().counter.wrapping_sub(self.sys().cntvoff),
            SR_CNTP_CTL => self.sys().cntp_ctl,
            SR_CNTP_CVAL => self.sys().cntp_cval,
            SR_CNTP_TVAL => {
                (self.sys().cntp_cval.wrapping_sub(self.sys().counter) as i32 as i64) as u64
            }
            SR_CNTV_CTL => self.sys().cntv_ctl,
            SR_CNTV_CVAL => self.sys().cntv_cval,
            SR_CNTV_TVAL => {
                let vc = self.sys().counter.wrapping_sub(self.sys().cntvoff);
                (self.sys().cntv_cval.wrapping_sub(vc) as i32 as i64) as u64
            }
            _ => self.sys().regs.get(&id).copied().unwrap_or(0),
        }
    }

    fn sysreg_write(&mut self, id: u32, v: u64) {
        match id {
            SR_SCTLR => {
                let was = self.sys().sctlr_el1;
                self.sys_mut().sctlr_el1 = v;
                if (was ^ v) & 1 != 0 {
                    self.bump_tlb(); // MMU enable/disable changes translation
                }
            }
            SR_TTBR0 => {
                self.sys_mut().ttbr0_el1 = v;
                self.bump_tlb();
            }
            SR_TTBR1 => {
                self.sys_mut().ttbr1_el1 = v;
                self.bump_tlb();
            }
            SR_TCR => {
                self.sys_mut().tcr_el1 = v;
                self.bump_tlb();
            }
            SR_VBAR => self.sys_mut().vbar_el1 = v,
            SR_ESR => self.sys_mut().esr_el1 = v,
            SR_FAR => self.sys_mut().far_el1 = v,
            SR_ELR => self.sys_mut().elr_el1 = v,
            SR_SPSR => self.sys_mut().spsr_el1 = v,
            SR_PAR => self.sys_mut().par_el1 = v,
            SR_SP_EL0 => {
                if self.sys().el == 1 && self.sys().spsel {
                    self.sys_mut().sp_el0 = v;
                } else {
                    self.sp = v;
                }
            }
            SR_SP_EL1 => {
                if self.sys().spsel {
                    self.sp = v;
                } else {
                    self.sys_mut().sp_el1 = v;
                }
            }
            SR_DAIF => self.sys_mut().daif = v & (0xf << 6),
            SR_NZCV => {
                self.flags = Nzcv {
                    n: v & (1 << 31) != 0,
                    z: v & (1 << 30) != 0,
                    c: v & (1 << 29) != 0,
                    v: v & (1 << 28) != 0,
                }
            }
            SR_SPSEL => self.set_spsel(v & 1 == 1),
            SR_CNTFRQ => self.sys_mut().cntfrq = v,
            SR_CNTP_CTL => self.sys_mut().cntp_ctl = v & 0x7,
            SR_CNTP_CVAL => self.sys_mut().cntp_cval = v,
            SR_CNTP_TVAL => {
                let c = self.sys().counter;
                self.sys_mut().cntp_cval = c.wrapping_add((v as i32 as i64) as u64);
            }
            SR_CNTV_CTL => self.sys_mut().cntv_ctl = v & 0x7,
            SR_CNTV_CVAL => self.sys_mut().cntv_cval = v,
            SR_CNTV_TVAL => {
                let vc = self.sys().counter.wrapping_sub(self.sys().cntvoff);
                self.sys_mut().cntv_cval = vc.wrapping_add((v as i32 as i64) as u64);
            }
            _ => {
                self.sys_mut().regs.insert(id, v);
            }
        }
    }

    /// `SMC`/`HVC` — the PSCI firmware interface (the emulator is the monitor; with
    /// no EL2/EL3 the conduit is handled here, as `qemu`'s in-machine PSCI does).
    /// `x0` is the PSCI function id; the result returns in `x0`.
    fn psci(&mut self, next: u64) {
        let func = self.x[0] as u32;
        const PSCI_VERSION: u32 = 0x8400_0000;
        const CPU_OFF: u32 = 0x8400_0002;
        const CPU_ON64: u32 = 0xc400_0003;
        const SYSTEM_OFF: u32 = 0x8400_0008;
        const SYSTEM_RESET: u32 = 0x8400_0009;
        const PSCI_FEATURES: u32 = 0x8400_000a;
        const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
        match func {
            PSCI_VERSION => self.x[0] = 0x0001_0000, // PSCI 1.0
            PSCI_FEATURES => self.x[0] = 0u64.wrapping_sub(1), // NOT_SUPPORTED for queried fn
            MIGRATE_INFO_TYPE => self.x[0] = 2,      // migration not required
            CPU_ON64 => self.x[0] = 0u64.wrapping_sub(2), // INVALID_PARAMETERS (single PE)
            CPU_OFF => self.x[0] = 0,
            SYSTEM_OFF | SYSTEM_RESET => {
                self.sys_mut().halted = true;
                self.sys_mut().halt_status = 0;
            }
            _ => self.x[0] = 0u64.wrapping_sub(1), // NOT_SUPPORTED
        }
        self.pc = next;
    }
}

// ── the ARM `virt` devicetree (a minimal FDT the kernel parses) ─────────────

/// Generate the flattened devicetree (DTB) for the holospaces ARM `virt` machine
/// — the blob the guest kernel parses to find its memory, CPU, GIC, generic
/// timer, PSCI, and PL011 console. Emitted from the same memory-map constants the
/// emulator decodes (one source of truth, Law L4).
fn arm64_virt_dtb(
    ram_base: u64,
    ram_size: u64,
    bootargs: &str,
    has_blk: bool,
    has_9p: bool,
    has_net: bool,
) -> Vec<u8> {
    const PH_GIC: u32 = 1;
    const PH_CLK: u32 = 2;
    let mut f = Fdt::new();
    f.begin_node("");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_str("compatible", "linux,dummy-virt");
    f.prop_str("model", "holospaces ARM virt (AArch64)");
    f.prop_u32("interrupt-parent", PH_GIC);

    f.begin_node("chosen");
    f.prop_str("bootargs", bootargs);
    f.prop_str("stdout-path", "/pl011@9000000");
    // Seed the kernel entropy pool (the bootloader's job): without it the boot
    // CRNG is all-zero, so `get_random_bytes` (AT_RANDOM, the stack-canary seed)
    // returns zeros — a 0 canary that any buffer write trips. A real bootloader
    // (qemu included) passes /chosen/rng-seed; we pass a fixed seed so the boot
    // is deterministic AND the canary/ASLR machinery is correctly initialised.
    let seed: [u8; 64] = {
        let mut s = [0u8; 64];
        let mut i = 0;
        while i < 64 {
            // a fixed, non-zero, well-mixed pattern (deterministic entropy seed)
            s[i] = (0x9eu32.wrapping_mul(i as u32 + 1) ^ 0x37) as u8;
            i += 1;
        }
        s
    };
    f.prop("rng-seed", &seed);
    f.end_node();

    f.begin_node("psci");
    f.prop_str_list("compatible", &["arm,psci-1.0", "arm,psci-0.2", "arm,psci"]);
    f.prop_str("method", "smc");
    f.end_node();

    f.begin_node("cpus");
    f.prop_u32("#address-cells", 1);
    f.prop_u32("#size-cells", 0);
    f.begin_node("cpu@0");
    f.prop_str("device_type", "cpu");
    f.prop_str("compatible", "arm,cortex-a57");
    f.prop_u32("reg", 0);
    f.prop_str("enable-method", "psci");
    f.end_node();
    f.end_node();

    f.begin_node("timer");
    f.prop_str("compatible", "arm,armv8-timer");
    // <PPI 13 LEVEL_HIGH(0x104)> sec-phys, <14> phys, <11> virt, <10> hyp.
    f.prop_cells(
        "interrupts",
        &[1, 13, 0x104, 1, 14, 0x104, 1, 11, 0x104, 1, 10, 0x104],
    );
    f.prop_empty("always-on");
    f.end_node();

    f.begin_node(&alloc::format!("memory@{ram_base:x}"));
    f.prop_str("device_type", "memory");
    f.prop_reg(ram_base, ram_size);
    f.end_node();

    // The GICv2: distributor + CPU interface.
    f.begin_node(&alloc::format!("intc@{GICD_BASE:x}"));
    f.prop_str("compatible", "arm,cortex-a15-gic");
    f.prop_u32("#interrupt-cells", 3);
    f.prop_empty("interrupt-controller");
    f.prop_u32("#address-cells", 0);
    f.prop_cells(
        "reg",
        &[
            0,
            GICD_BASE as u32,
            0,
            (GICD_END - GICD_BASE) as u32,
            0,
            GICC_BASE as u32,
            0,
            (GICC_END - GICC_BASE) as u32,
        ],
    );
    f.prop_u32("phandle", PH_GIC);
    f.end_node();

    // A fixed clock for the PL011 (the amba bus needs `apb_pclk`).
    f.begin_node("apb-pclk");
    f.prop_str("compatible", "fixed-clock");
    f.prop_u32("#clock-cells", 0);
    f.prop_u32("clock-frequency", 24_000_000);
    f.prop_str("clock-output-names", "clk24mhz");
    f.prop_u32("phandle", PH_CLK);
    f.end_node();

    f.begin_node(&alloc::format!("pl011@{UART_BASE:x}"));
    f.prop_str_list("compatible", &["arm,pl011", "arm,primecell"]);
    f.prop_reg(UART_BASE, UART_END - UART_BASE);
    // <SPI 1 LEVEL_HIGH> (the UART interrupt; the console is polled, but the
    // node is complete).
    f.prop_cells("interrupts", &[0, 1, 4]);
    f.prop_cells("clocks", &[PH_CLK, PH_CLK]);
    f.prop_str_list("clock-names", &["uartclk", "apb_pclk"]);
    f.end_node();

    // The `virtio-mmio` block device (the κ-disk rootfs, `CC-37`) — declared only
    // when a disk is attached. `interrupts = <GIC_SPI 16 LEVEL_HIGH>` → INTID 48.
    if has_blk {
        f.begin_node(&alloc::format!("virtio_mmio@{VIRTIO_BLK_BASE:x}"));
        f.prop_str("compatible", "virtio,mmio");
        f.prop_reg(VIRTIO_BLK_BASE, VIRTIO_BLK_END - VIRTIO_BLK_BASE);
        f.prop_cells("interrupts", &[0, INTID_VIRTIO_BLK - SPI_BASE_INTID, 4]);
        f.end_node();
    }

    // The `virtio-9p` (shared workspace, `CC-15` parity) slot — declared only
    // when a workspace is attached (an unattached slot makes the kernel read
    // magic 0 and stall). `interrupts = <GIC_SPI 17 LEVEL_HIGH>` → INTID 49.
    if has_9p {
        f.begin_node(&alloc::format!("virtio_mmio@{VIRTIO_9P_BASE:x}"));
        f.prop_str("compatible", "virtio,mmio");
        f.prop_reg(VIRTIO_9P_BASE, VIRTIO_9P_END - VIRTIO_9P_BASE);
        f.prop_cells("interrupts", &[0, INTID_VIRTIO_9P - SPI_BASE_INTID, 4]);
        f.end_node();
    }

    // The `virtio-net` (`CC-16` parity) slot — declared only when networking is
    // attached. `interrupts = <GIC_SPI 18 LEVEL_HIGH>` → INTID 50.
    if has_net {
        f.begin_node(&alloc::format!("virtio_mmio@{VIRTIO_NET_BASE:x}"));
        f.prop_str("compatible", "virtio,mmio");
        f.prop_reg(VIRTIO_NET_BASE, VIRTIO_NET_END - VIRTIO_NET_BASE);
        f.prop_cells("interrupts", &[0, INTID_VIRTIO_NET - SPI_BASE_INTID, 4]);
        f.end_node();
    }

    f.end_node(); // root
    f.finish()
}

// ── a minimal flattened-device-tree (DTB) writer (the DTB spec) ─────────────

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
    fn name_off(&mut self, name: &str) -> u32 {
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
    fn finish(mut self) -> Vec<u8> {
        self.token(FDT_END);
        let header_len = 40u32;
        let memrsv_len = 16u32;
        let off_struct = header_len + memrsv_len;
        let off_strings = off_struct + self.structure.len() as u32;
        let total = off_strings + self.strings.len() as u32;
        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(&FDT_MAGIC.to_be_bytes());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&off_struct.to_be_bytes());
        out.extend_from_slice(&off_strings.to_be_bytes());
        out.extend_from_slice(&header_len.to_be_bytes());
        out.extend_from_slice(&FDT_VERSION.to_be_bytes());
        out.extend_from_slice(&FDT_LAST_COMP_VERSION.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(self.strings.len() as u32).to_be_bytes());
        out.extend_from_slice(&(self.structure.len() as u32).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&self.structure);
        out.extend_from_slice(&self.strings);
        out
    }
}

#[cfg(test)]
// The encoders below mirror the A64 fixed-field instruction layouts: a `0 << n`
// marker shows a zero field at its bit position (`clippy::identity_op`), and the
// register-class encoders take one argument per ISA operand field
// (`clippy::too_many_arguments`). Both are intentional — they keep the encoders
// legible against the Arm-ARM encoding tables.
#[allow(clippy::identity_op, clippy::too_many_arguments)]
mod tests {
    use super::*;

    // ── A64 instruction encoders (the Arm-ARM fixed-field layouts) ──────────
    // Hand-encoding lets a battery assert against the Arm-ARM-defined final
    // state with no external assembler — the same approach the RISC-V core's
    // tests use for hand-assembled programs.

    fn movz(sf: u32, rd: u32, imm: u32, hw: u32) -> u32 {
        (sf << 31) | (0b10 << 29) | (0b100101 << 23) | (hw << 21) | ((imm & 0xffff) << 5) | rd
    }
    fn movn(sf: u32, rd: u32, imm: u32, hw: u32) -> u32 {
        (sf << 31) | (0b00 << 29) | (0b100101 << 23) | (hw << 21) | ((imm & 0xffff) << 5) | rd
    }
    fn movk(sf: u32, rd: u32, imm: u32, hw: u32) -> u32 {
        (sf << 31) | (0b11 << 29) | (0b100101 << 23) | (hw << 21) | ((imm & 0xffff) << 5) | rd
    }
    fn addsub_imm(sf: u32, op: u32, s: u32, rd: u32, rn: u32, imm12: u32, sh: u32) -> u32 {
        (sf << 31)
            | (op << 30)
            | (s << 29)
            | (0b100010 << 23)
            | (sh << 22)
            | ((imm12 & 0xfff) << 10)
            | (rn << 5)
            | rd
    }
    fn logical_imm(sf: u32, opc: u32, rd: u32, rn: u32, n: u32, immr: u32, imms: u32) -> u32 {
        (sf << 31)
            | (opc << 29)
            | (0b100100 << 23)
            | (n << 22)
            | (immr << 16)
            | (imms << 10)
            | (rn << 5)
            | rd
    }
    fn addsub_reg(
        sf: u32,
        op: u32,
        s: u32,
        rd: u32,
        rn: u32,
        rm: u32,
        shift: u32,
        amt: u32,
    ) -> u32 {
        (sf << 31)
            | (op << 30)
            | (s << 29)
            | (0b01011 << 24)
            | (shift << 22)
            | (rm << 16)
            | (amt << 10)
            | (rn << 5)
            | rd
    }
    fn addsub_ext(
        sf: u32,
        op: u32,
        s: u32,
        rd: u32,
        rn: u32,
        rm: u32,
        option: u32,
        imm3: u32,
    ) -> u32 {
        (sf << 31)
            | (op << 30)
            | (s << 29)
            | (0b01011 << 24)
            | (1 << 21)
            | (rm << 16)
            | (option << 13)
            | (imm3 << 10)
            | (rn << 5)
            | rd
    }
    fn logical_reg(
        sf: u32,
        opc: u32,
        n: u32,
        rd: u32,
        rn: u32,
        rm: u32,
        shift: u32,
        amt: u32,
    ) -> u32 {
        (sf << 31)
            | (opc << 29)
            | (0b01010 << 24)
            | (shift << 22)
            | (n << 21)
            | (rm << 16)
            | (amt << 10)
            | (rn << 5)
            | rd
    }
    fn adc_sbc(sf: u32, op: u32, s: u32, rd: u32, rn: u32, rm: u32) -> u32 {
        (sf << 31) | (op << 30) | (s << 29) | (0b11010000 << 21) | (rm << 16) | (rn << 5) | rd
    }
    fn three_src(sf: u32, op31: u32, o0: u32, rd: u32, rn: u32, rm: u32, ra: u32) -> u32 {
        (sf << 31)
            | (0b11011 << 24)
            | (op31 << 21)
            | (rm << 16)
            | (o0 << 15)
            | (ra << 10)
            | (rn << 5)
            | rd
    }
    fn two_src(sf: u32, opcode: u32, rd: u32, rn: u32, rm: u32) -> u32 {
        (sf << 31) | (0b11010110 << 21) | (rm << 16) | (opcode << 10) | (rn << 5) | rd
    }
    fn one_src(sf: u32, opcode: u32, rd: u32, rn: u32) -> u32 {
        (sf << 31) | (1 << 30) | (0b11010110 << 21) | (opcode << 10) | (rn << 5) | rd
    }
    fn csel(sf: u32, op: u32, o2: u32, rd: u32, rn: u32, rm: u32, cond: u32) -> u32 {
        (sf << 31)
            | (op << 30)
            | (0b11010100 << 21)
            | (rm << 16)
            | (cond << 12)
            | (o2 << 10)
            | (rn << 5)
            | rd
    }
    fn ccmp_reg(sf: u32, op: u32, rn: u32, rm: u32, cond: u32, nzcv: u32) -> u32 {
        (sf << 31)
            | (op << 30)
            | (1 << 29)
            | (0b11010010 << 21)
            | (rm << 16)
            | (cond << 12)
            | (rn << 5)
            | nzcv
    }
    fn bitfield(sf: u32, opc: u32, rd: u32, rn: u32, n: u32, immr: u32, imms: u32) -> u32 {
        (sf << 31)
            | (opc << 29)
            | (0b100110 << 23)
            | (n << 22)
            | (immr << 16)
            | (imms << 10)
            | (rn << 5)
            | rd
    }
    fn extr(sf: u32, rd: u32, rn: u32, rm: u32, n: u32, imms: u32) -> u32 {
        (sf << 31) | (0b100111 << 23) | (n << 22) | (rm << 16) | (imms << 10) | (rn << 5) | rd
    }
    fn b(off_words: i32) -> u32 {
        0x1400_0000 | ((off_words as u32) & 0x03ff_ffff)
    }
    fn bcond(cond: u32, off_words: i32) -> u32 {
        0x5400_0000 | (((off_words as u32) & 0x7_ffff) << 5) | cond
    }
    fn cbnz(sf: u32, rt: u32, off_words: i32) -> u32 {
        (sf << 31) | 0x3500_0000 | (((off_words as u32) & 0x7_ffff) << 5) | rt
    }
    fn ldst_uimm(size: u32, opc: u32, rt: u32, rn: u32, imm12: u32) -> u32 {
        (size << 30) | 0x3900_0000 | (opc << 22) | ((imm12 & 0xfff) << 10) | (rn << 5) | rt
    }
    fn ldp_stp(opc: u32, l: u32, rt: u32, rt2: u32, rn: u32, imm7: i32) -> u32 {
        (opc << 30)
            | 0x2900_0000
            | (l << 22)
            | (((imm7 as u32) & 0x7f) << 15)
            | (rt2 << 10)
            | (rn << 5)
            | rt
    }
    fn ret(rn: u32) -> u32 {
        0xd65f_0000 | (rn << 5)
    }
    fn bl(off_words: i32) -> u32 {
        0x9400_0000 | ((off_words as u32) & 0x03ff_ffff)
    }
    fn svc0() -> u32 {
        0xd400_0001
    }

    const BASE: u64 = 0x4000_0000;

    /// Assemble, load at `BASE`, run to exit, and return the `exit` status — the
    /// value the battery left in `x0`. Panics if the program faults or runs away.
    fn exit_code(prog: &[u32]) -> u64 {
        let mut cpu = Cpu::new(BASE, 0x1_0000);
        let mut img = Vec::new();
        for w in prog {
            img.extend_from_slice(&w.to_le_bytes());
        }
        cpu.load_image(&img);
        match cpu.run(1_000_000) {
            Halt::Exit(code) => code,
            other => panic!("battery did not exit cleanly: {other:?}"),
        }
    }

    /// The two-instruction epilogue: `MOV w8, #93 ; SVC #0` — exit with the
    /// verdict already in `x0`.
    fn exit_seq() -> [u32; 2] {
        [movz(0, 8, 93, 0), svc0()]
    }

    fn prog(mut body: Vec<u32>) -> Vec<u32> {
        body.extend_from_slice(&exit_seq());
        body
    }

    #[test]
    fn movz_movk_movn_compose_a_64bit_constant() {
        // x0 = 0x1122_3344_5566_7788 via MOVZ + three MOVK.
        let code = exit_code(&prog(vec![
            movz(1, 0, 0x7788, 0),
            movk(1, 0, 0x5566, 1),
            movk(1, 0, 0x3344, 2),
            movk(1, 0, 0x1122, 3),
        ]));
        assert_eq!(code, 0x1122_3344_5566_7788);
        // MOVN w0, #0 -> 0xffff_ffff (32-bit, zero-extended).
        assert_eq!(exit_code(&prog(vec![movn(0, 0, 0, 0)])), 0xffff_ffff);
    }

    #[test]
    fn add_sub_immediate_and_flags() {
        // 100 + 23 = 123.
        assert_eq!(
            exit_code(&prog(vec![
                movz(1, 0, 100, 0),
                addsub_imm(1, 0, 0, 0, 0, 23, 0)
            ])),
            123
        );
        // SUBS computing 5 - 5 sets Z; CSET via CSINC of XZR reads it back.
        // x0 = (5-5==0) ? 1 : 0  → use SUBS then CSINC x0, xzr, xzr, NE (cond inverted).
        let code = exit_code(&prog(vec![
            movz(1, 1, 5, 0),
            addsub_imm(1, 1, 1, 31, 1, 5, 0), // SUBS xzr, x1, #5  (CMP x1,#5)
            csel(1, 0, 1, 0, 31, 31, 0b0001), // CSINC x0, xzr, xzr, NE → x0 = NE? 0 : 1 = Z
        ]));
        assert_eq!(code, 1, "Z flag set after 5-5");
        // ADD with LSL #12 shift of the immediate (Rn=x1=0, since Rn=31 would be SP).
        assert_eq!(
            exit_code(&prog(vec![
                movz(1, 1, 0, 0),
                addsub_imm(1, 0, 0, 0, 1, 1, 1)
            ])),
            0x1000
        );
    }

    #[test]
    fn add_sub_shifted_and_extended_register() {
        // x0 = (3 << 4) + 5 via ADD shifted register (LSL 4).
        let code = exit_code(&prog(vec![
            movz(1, 1, 3, 0),
            movz(1, 2, 5, 0),
            addsub_reg(1, 0, 0, 0, 2, 1, 0b00, 4),
        ]));
        assert_eq!(code, (3 << 4) + 5);
        // Extended register: ADD x0, x1, w2, SXTB — sign-extend a byte then add.
        let code = exit_code(&prog(vec![
            movz(1, 1, 100, 0),
            movn(0, 2, 0, 0), // w2 = 0xffff_ffff (byte 0xff = -1)
            addsub_ext(1, 0, 0, 0, 1, 2, 0b100, 0), // SXTB → -1
        ]));
        assert_eq!(code, 99);
    }

    #[test]
    fn logical_register_and_immediate() {
        // ORR/AND/EOR shifted register.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0xff00, 0),
            movz(1, 2, 0x00ff, 0),
            logical_reg(1, 0b01, 0, 0, 1, 2, 0, 0), // ORR
        ]));
        assert_eq!(code, 0xffff);
        // BIC: x0 = 0xff & ~0x0f = 0xf0 (logical_reg with N=1).
        let code = exit_code(&prog(vec![
            movz(1, 1, 0xff, 0),
            movz(1, 2, 0x0f, 0),
            logical_reg(1, 0b00, 1, 0, 1, 2, 0, 0),
        ]));
        assert_eq!(code, 0xf0);
        // AND immediate: x0 = 0x1234 & 0xff = 0x34. Encoding N=1,immr=0,imms=7 = 0xff.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0x1234, 0),
            logical_imm(1, 0b00, 0, 1, 1, 0, 7),
        ]));
        assert_eq!(code, 0x34);
    }

    #[test]
    fn multiply_divide_and_high_products() {
        // MUL (MADD with Ra=XZR): 7 * 6 = 42.
        let code = exit_code(&prog(vec![
            movz(1, 1, 7, 0),
            movz(1, 2, 6, 0),
            three_src(1, 0b000, 0, 0, 1, 2, 31),
        ]));
        assert_eq!(code, 42);
        // MADD: 7*6 + 8 = 50.
        let code = exit_code(&prog(vec![
            movz(1, 1, 7, 0),
            movz(1, 2, 6, 0),
            movz(1, 3, 8, 0),
            three_src(1, 0b000, 0, 0, 1, 2, 3),
        ]));
        assert_eq!(code, 50);
        // UMULH of (1<<63)*2 = 1.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0x8000, 3), // x1 = 1<<63
            movz(1, 2, 2, 0),
            three_src(1, 0b110, 0, 0, 1, 2, 31),
        ]));
        assert_eq!(code, 1);
        // SDIV: -20 / 3 = -6 (truncated toward zero).
        let code = exit_code(&prog(vec![
            movn(1, 1, 19, 0), // x1 = ~19 = -20
            movz(1, 2, 3, 0),
            two_src(1, 0b000011, 0, 1, 2),
        ]));
        assert_eq!(code as i64, -6);
        // UDIV by zero = 0 (the A64 result).
        let code = exit_code(&prog(vec![
            movz(1, 1, 5, 0),
            two_src(1, 0b000010, 0, 1, 31),
        ]));
        assert_eq!(code, 0);
    }

    #[test]
    fn variable_shifts_bitfield_and_extract() {
        // LSLV: 1 << 40.
        let code = exit_code(&prog(vec![
            movz(1, 1, 1, 0),
            movz(1, 2, 40, 0),
            two_src(1, 0b001000, 0, 1, 2),
        ]));
        assert_eq!(code, 1u64 << 40);
        // UBFX x0, x1, #8, #8  (UBFM immr=8, imms=15) extracts byte 1 of 0xAB_CD_EF.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0xcdef, 0),
            movk(1, 1, 0x00ab, 1),
            bitfield(1, 0b10, 0, 1, 1, 8, 15),
        ]));
        assert_eq!(code, 0xcd);
        // ASR via SBFM: -256 >> 4 = -16.
        let code = exit_code(&prog(vec![
            movn(1, 1, 0xff, 0),               // x1 = ~0xff = -256
            bitfield(1, 0b00, 0, 1, 1, 4, 63), // SBFM immr=4, imms=63 → ASR #4
        ]));
        assert_eq!(code as i64, -16);
        // EXTR x0, x1, x2, #16 — bottom 16 of x1 become top, top 48 of x2 the rest.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0xaaaa, 0),
            movz(1, 2, 0xbbbb, 0),
            extr(1, 0, 1, 2, 1, 16),
        ]));
        assert_eq!(code, (0xaaaau64 << 48) | (0xbbbbu64 >> 16));
    }

    #[test]
    fn one_source_bit_ops() {
        // CLZ of (1<<60) = 3.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0x1000, 3), // 1<<60
            one_src(1, 0b000100, 0, 1),
        ]));
        assert_eq!(code, 3);
        // RBIT of 1 = 1<<63.
        let code = exit_code(&prog(vec![movz(1, 1, 1, 0), one_src(1, 0b000000, 0, 1)]));
        assert_eq!(code, 1u64 << 63);
        // REV (64-bit) of 0x1122_3344_5566_7788.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0x7788, 0),
            movk(1, 1, 0x5566, 1),
            movk(1, 1, 0x3344, 2),
            movk(1, 1, 0x1122, 3),
            one_src(1, 0b000011, 0, 1),
        ]));
        assert_eq!(code, 0x8877_6655_4433_2211);
    }

    #[test]
    fn add_with_carry_chains_128bit() {
        // (0xffff_ffff_ffff_ffff + 1) as a 128-bit add: low = 0, high carries to 1.
        // x1:x0 = -1 ; add 1 to low (ADDS sets C), ADC high (0)+0+C → 1.
        let code = exit_code(&prog(vec![
            movn(1, 0, 0, 0),                        // x0 = 0xffff...ffff
            movz(1, 1, 0, 0),                        // x1 = 0 (high)
            addsub_imm(1, 0, 1, 0, 0, 1, 0),         // ADDS x0, x0, #1 → x0=0, C=1
            adc_sbc(1, 0, 0, 2, 1, 31),              // ADC x2, x1, xzr → x2 = 0+0+C = 1
            logical_reg(1, 0b01, 0, 0, 2, 31, 0, 0), // ORR x0, x2, xzr (MOV x0,x2)
        ]));
        assert_eq!(code, 1);
    }

    #[test]
    fn conditional_compare_select() {
        // CMP x1,#10 ; CCMP x1,#20,#0,EQ — EQ false so flags = #0 (no flags). Then
        // verify GT path via CSEL.  Simpler: x0 = (5 > 3) ? 111 : 222.
        let code = exit_code(&prog(vec![
            movz(1, 1, 5, 0),
            movz(1, 2, 3, 0),
            addsub_reg(1, 1, 1, 31, 1, 2, 0, 0), // SUBS xzr, x1, x2  (CMP)
            movz(1, 3, 111, 0),
            movz(1, 4, 222, 0),
            csel(1, 0, 0, 0, 3, 4, 0b1100), // CSEL x0, x3, x4, GT
        ]));
        assert_eq!(code, 111);
        // CCMP register: CMP x1,#1 (NE for !=) then CCMP feeding a fixed nzcv when
        // the first condition is false.
        let code = exit_code(&prog(vec![
            movz(1, 1, 2, 0),
            addsub_imm(1, 1, 1, 31, 1, 2, 0), // CMP x1,#2 → Z=1 (EQ)
            ccmp_reg(1, 1, 1, 1, 0b0000, 0b0010), // CCMP x1,x1,#2,EQ → cond true → flags of x1-x1: Z=1,C=1
            csel(1, 0, 1, 0, 31, 31, 0b0001),     // CSINC x0,xzr,xzr,NE → Z? 1:0
        ]));
        assert_eq!(code, 1);
    }

    #[test]
    fn loads_stores_round_trip() {
        // Store 0xdeadbeef to [sp,#-16]! style via a scratch area in RAM, reload.
        // Use x1 as a base pointer into RAM (BASE + 0x800), store a dword, load it.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0x0800, 0),
            movk(1, 1, 0x4000, 1), // x1 = BASE + 0x800
            movz(1, 2, 0xbeef, 0),
            movk(1, 2, 0xdead, 1),    // x2 = 0xdead_beef
            ldst_uimm(3, 0, 2, 1, 0), // STR x2, [x1]
            ldst_uimm(3, 1, 0, 1, 0), // LDR x0, [x1]
        ]));
        assert_eq!(code, 0xdead_beef);
        // STRB then LDRSB sign-extends 0xff to -1.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0x0820, 0),
            movk(1, 1, 0x4000, 1),
            movn(1, 2, 0, 0),         // x2 = 0xffff...
            ldst_uimm(0, 0, 2, 1, 0), // STRB w2, [x1]
            ldst_uimm(0, 2, 0, 1, 0), // LDRSB x0, [x1]
        ]));
        assert_eq!(code as i64, -1);
        // STP/LDP pair round trip.
        let code = exit_code(&prog(vec![
            movz(1, 1, 0x0840, 0),
            movk(1, 1, 0x4000, 1),
            movz(1, 2, 0x1111, 0),
            movz(1, 3, 0x2222, 0),
            ldp_stp(0b10, 0, 2, 3, 1, 0),        // STP x2, x3, [x1]
            ldp_stp(0b10, 1, 4, 5, 1, 0),        // LDP x4, x5, [x1]
            three_src(1, 0b000, 0, 0, 4, 31, 5), // x0 = x4*0 + x5? no: use ADD
        ]));
        // x4=0x1111, x5=0x2222; the MADD above gives x4*xzr + x5 = 0x2222.
        assert_eq!(code, 0x2222);
    }

    #[test]
    fn control_flow_loop_and_call() {
        // Sum 1..=10 = 55 via a CBNZ countdown loop.
        let code = exit_code(&prog(vec![
            movz(1, 0, 0, 0),                   // 0: acc = 0
            movz(1, 1, 10, 0),                  // 1: i = 10
            addsub_reg(1, 0, 0, 0, 0, 1, 0, 0), // 2: acc += i   (loop top)
            addsub_imm(1, 1, 0, 1, 1, 1, 0),    // 3: i -= 1
            cbnz(1, 1, -2),                     // 4: if i != 0 → back to index 2
        ]));
        assert_eq!(code, 55);
        // BL/RET: a subroutine that adds 1.
        let code = exit_code(&prog(vec![
            movz(1, 0, 41, 0),               // 0
            bl(3),                           // 1: call index 4
            movz(1, 8, 93, 0),               // 2: epilogue (exit)
            svc0(),                          // 3
            addsub_imm(1, 0, 0, 0, 0, 1, 0), // 4: x0 += 1
            ret(30),                         // 5
        ]));
        assert_eq!(code, 42);
    }

    #[test]
    fn unconditional_and_conditional_immediate_branches() {
        // B skips a poison MOV.
        let code = exit_code(&prog(vec![
            movz(1, 0, 7, 0),   // 0
            b(2),               // 1: → index 3 (the exit epilogue)
            movz(1, 0, 999, 0), // 2: skipped
        ]));
        assert_eq!(code, 7);
        // CMP then B.GT over a poison MOV.
        let code = exit_code(&prog(vec![
            movz(1, 1, 5, 0),
            movz(1, 2, 3, 0),
            addsub_reg(1, 1, 1, 31, 1, 2, 0, 0), // CMP x1, x2 → GT
            bcond(0b1100, 2),                    // B.GT → index 5
            movz(1, 0, 999, 0),                  // 4: skipped
            movz(1, 0, 42, 0),                   // 5
        ]));
        assert_eq!(code, 42);
    }

    #[test]
    fn tbz_tbnz_branches() {
        // TBNZ on bit 3 of 0b1000 should branch (skip a SUB that would corrupt x0).
        // Encode TBNZ x1, #3, +2 ; (skipped) MOVZ x0,#999 ; MOVZ x0,#7
        let tbnz = (0u32 << 31) | 0x3700_0000 | (3 << 19) | (((2i32 as u32) & 0x3fff) << 5) | 1;
        let code = exit_code(&prog(vec![
            movz(1, 1, 0b1000, 0),
            tbnz,
            movz(1, 0, 999, 0),
            movz(1, 0, 7, 0),
        ]));
        assert_eq!(code, 7);
    }

    #[test]
    fn console_write_syscall() {
        // write(1, msg, 2) where msg points at a 2-byte literal in RAM we store.
        let mut cpu = Cpu::new(BASE, 0x1_0000);
        let prog = prog(vec![
            // store "OK" at BASE+0x900
            movz(1, 1, 0x0900, 0),
            movk(1, 1, 0x4000, 1),
            movz(1, 2, 0x4b4f, 0),    // 'O'=0x4f,'K'=0x4b little-endian
            ldst_uimm(1, 0, 2, 1, 0), // STRH w2,[x1]
            movz(1, 0, 1, 0),         // fd = 1
            logical_reg(1, 0b01, 0, 1, 1, 31, 0, 0), // x1 = buf (already), MOV via ORR
            movz(1, 2, 2, 0),         // len = 2
            movz(1, 8, 64, 0),        // write
            svc0(),
        ]);
        let mut img = Vec::new();
        for w in &prog {
            img.extend_from_slice(&w.to_le_bytes());
        }
        cpu.load_image(&img);
        assert_eq!(cpu.run(10_000), Halt::Exit(2));
        assert_eq!(cpu.console(), b"OK");
    }

    #[test]
    fn adr_yields_its_own_pc() {
        // ADR x0, . → x0 = BASE (the instruction's address).
        let adr = (0u32 << 31) | (0 << 29) | (0b10000 << 24) | (0 << 5);
        assert_eq!(exit_code(&prog(vec![adr])), BASE);
    }

    #[test]
    fn decode_bit_masks_matches_known_immediates() {
        // 0xff: N=1, immr=0, imms=7.
        assert_eq!(decode_bit_masks(1, 7, 0, 64, true).unwrap().0, 0xff);
        // 0x5555_5555_5555_5555: N=0, imms=0x3c, immr=0 → esize-2 pattern `01` repeated.
        assert_eq!(
            decode_bit_masks(0, 0x3c, 0, 64, true).unwrap().0,
            0x5555_5555_5555_5555
        );
        // N=0, imms=0, immr=0 → esize-32 pattern with a single low bit set.
        assert_eq!(
            decode_bit_masks(0, 0, 0, 64, true).unwrap().0,
            0x0000_0001_0000_0001
        );
        // The all-ones reserved logical immediate is rejected.
        assert!(decode_bit_masks(1, 63, 0, 64, true).is_none());
    }

    /// The word-scanning `Gic::best()` selects the same lowest deliverable INTID
    /// (pending & enabled & !active) the per-INTID scan did, and returns `None`
    /// when nothing is deliverable — only faster (it skips empty words and jumps
    /// to the lowest set bit). Spot-checks across word boundaries and the masks.
    #[test]
    fn gic_best_picks_the_lowest_deliverable_intid() {
        let mut gic = Gic::new();
        assert_eq!(gic.best(), None, "nothing pending → None");

        // Pending but not enabled → not deliverable.
        gic.raise(42);
        assert_eq!(gic.best(), None);
        // Enable it → now deliverable.
        Gic::set_bit(&mut gic.enabled, 42, true);
        assert_eq!(gic.best(), Some(42));

        // A lower enabled+pending INTID in an earlier word wins.
        gic.raise(5);
        Gic::set_bit(&mut gic.enabled, 5, true);
        assert_eq!(gic.best(), Some(5), "the lowest deliverable INTID wins");

        // Marking the lowest active skips it to the next deliverable one.
        Gic::set_bit(&mut gic.active, 5, true);
        assert_eq!(gic.best(), Some(42), "an active INTID is skipped");

        // A high INTID near the top of the valid range is found (word-boundary).
        let mut g2 = Gic::new();
        g2.raise(1000);
        Gic::set_bit(&mut g2.enabled, 1000, true);
        assert_eq!(g2.best(), Some(1000));

        // A bit at/above 1020 (the reserved range) is never reported.
        let mut g3 = Gic::new();
        g3.raise(1021);
        Gic::set_bit(&mut g3.enabled, 1021, true);
        assert_eq!(
            g3.best(),
            None,
            "INTIDs ≥ 1020 are reserved, not deliverable"
        );
    }
}
