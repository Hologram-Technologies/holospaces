//! The simple-framebuffer region: the guest writes RGBA at the top of RAM; the host reads it back.
use holospaces::emulator::aarch64::Cpu;
#[test]
fn framebuffer_region_roundtrips_at_top_of_ram() {
    let mut cpu = Cpu::new(0x4000_0000, 512 << 20);          // 512 MiB at the arm64 virt RAM base
    // a test pattern the size of the scanout
    let mut pat = vec![0u8; Cpu::FB_SIZE];
    for i in 0..Cpu::FB_SIZE { pat[i] = ((i * 31 + 7) & 0xff) as u8; }
    cpu.write_framebuffer(&pat);
    assert_eq!(cpu.read_framebuffer(), pat, "framebuffer read-back == what was written");
    // the advertised base is inside RAM, FB_SIZE from the top
    assert_eq!(cpu.fb_phys_base(), 0x4000_0000 + (512u64 << 20) - Cpu::FB_SIZE as u64);
    assert_eq!(Cpu::FB_W * Cpu::FB_H * 4, Cpu::FB_SIZE);
}
