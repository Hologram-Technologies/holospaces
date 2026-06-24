#!/usr/bin/env bash
# Regenerate the CC-45 amd64 (x86-64) devcontainer artifacts from their pinned
# sources — the x86-64 analogue of the CC-37 arm64 recipe
# (vv/artifacts/cc37/build.sh): a real amd64 Linux kernel with **no embedded
# initramfs** (so it mounts a real virtio-blk root), a **stock static
# linux-amd64 busybox**, and the OCI layer the in-crate Layer Assembler (CC-7)
# overlays into the ext4 rootfs.
#
# Built natively on an x86-64 host (no cross toolchain): gcc + the kernel build
# deps (bc bison flex libssl-dev libelf-dev) + cpio + wget. See SOURCE.txt.
#
# Two kernel outputs are committed:
#   • vmlinux.gz  — the gzip-compressed *uncompressed* ELF kernel. The holospaces
#                   x86-64 core loads its PT_LOAD segments and enters `startup_64`
#                   directly (the 64-bit boot protocol) — the image the emulator
#                   witness boots.
#   • bzImage     — the standard self-decompressing kernel, the image
#                   qemu-system-x86_64 -kernel boots for the differential oracle.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"; cd "$work"

echo "== [1/3] cc45 amd64 kernel (no initramfs, virtio-blk root) =="
git clone --depth 1 --branch v6.6 https://github.com/torvalds/linux.git linux >/dev/null 2>&1
( cd linux
  export ARCH=x86_64
  make x86_64_defconfig >/dev/null
  # No embedded initramfs → the kernel mounts the real virtio-blk root the
  # Layer Assembler produced. The κ-disk is discovered via the cmdline
  # `virtio_mmio.device=...` (VIRTIO_MMIO_CMDLINE_DEVICES) — x86 has no DTB.
  ./scripts/config --set-str INITRAMFS_SOURCE "" \
      --disable BLK_DEV_INITRD \
      --enable  VIRTIO --enable VIRTIO_MMIO \
      --enable  VIRTIO_MMIO_CMDLINE_DEVICES --enable VIRTIO_BLK \
      --enable  VIRTIO_NET --enable NET_9P --enable NET_9P_VIRTIO --enable 9P_FS \
      --enable  EXT4_FS --enable IP_PNP --enable IP_PNP_DHCP \
      --enable  SERIAL_8250 --enable SERIAL_8250_CONSOLE
  # The holospaces x86-64 core implements 4-level paging (no LA57/5-level); a
  # 4-level kernel is a completely standard x86-64 Linux. Disabling 5-level makes
  # the kernel and the emulator MMU agree on the paging mode. FTRACE off for build
  # speed. Deterministic, reproducible build env.
  ./scripts/config --disable FTRACE --disable FUNCTION_TRACER
  ./scripts/config --disable X86_5LEVEL
  echo 0 > .version
  export KBUILD_BUILD_TIMESTAMP="Fri 31 May 2026 00:00:00 UTC" \
         KBUILD_BUILD_USER=holospaces KBUILD_BUILD_HOST=cc45
  make olddefconfig >/dev/null
  make -j"$(nproc)" bzImage >/dev/null 2>&1 )
gzip -9 -c linux/vmlinux            > "$here/linux/vmlinux.gz"
cp linux/arch/x86/boot/bzImage        "$here/linux/bzImage"
cp linux/.config                      "$here/linux/kernel.config"
echo "kernel: vmlinux.gz $(stat -c%s "$here/linux/vmlinux.gz") bytes; bzImage $(stat -c%s "$here/linux/bzImage") bytes"

echo "== [2/4] static busybox amd64 (the stock binary) =="
BBVER=1.36.1
wget -q "https://busybox.net/downloads/busybox-$BBVER.tar.bz2"
tar xf "busybox-$BBVER.tar.bz2"
( cd "busybox-$BBVER"
  make ARCH=x86_64 defconfig >/dev/null
  sed -i 's/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/' .config
  # Disprefer features needing extra libs to keep the static link clean.
  sed -i 's/^CONFIG_TC=y/# CONFIG_TC is not set/' .config || true
  # `|| true`: under `set -o pipefail`, `yes` is killed by SIGPIPE (141) when make
  # finishes reading config answers — that is expected, not a failure. defconfig
  # already produced a valid .config; oldconfig only re-resolves the two flipped
  # bools, taking the default (empty line) for any dependent prompt.
  yes "" | make ARCH=x86_64 oldconfig >/dev/null 2>&1 || true
  make ARCH=x86_64 -j"$(nproc)" >/dev/null 2>&1
  strip busybox )
cp "busybox-$BBVER/busybox" "$here/rootfs/busybox"
echo "busybox: $(stat -c%s "$here/rootfs/busybox") bytes  ($(file -b "$here/rootfs/busybox" | cut -d, -f1-2))"

echo "== [3/5] OCI image layer (tar+gzip of /bin/busybox) =="
ldir="$(mktemp -d)"; mkdir -p "$ldir/bin"
cp "$here/rootfs/busybox" "$ldir/bin/busybox"
( cd "$ldir" && tar --numeric-owner --owner=0 --group=0 --mtime='2026-05-31 00:00:00 UTC' --sort=name -cf - bin ) | gzip -9 -n > "$here/rootfs/layer.tar.gz"

echo "== [4/5] static linux-amd64 TinyCC (the in-guest toolchain — build-capable) =="
# A real, self-contained C compiler the guest runs to BUILD software in-image
# (the decisive CC-45 acceptance: a toolchain compiles a program to a runnable
# binary inside the devcontainer). Pinned TinyCC mob; statically linked so it runs
# in the libc-free busybox rootfs. `make tcc` only (the cross/test targets are not
# needed and some fail on a modern host — the `tcc` binary is the artifact).
TCCREV=a338258d309c888bde96b2d1f206299231a54ddf # tcc mob, 0.9.28rc
git clone https://repo.or.cz/tinycc.git tinycc >/dev/null 2>&1
( cd tinycc
  git checkout -q "$TCCREV"
  ./configure --extra-ldflags="-static" >/dev/null 2>&1
  make -j"$(nproc)" tcc >/dev/null 2>&1
  strip tcc )
mkdir -p "$here/build"
cp "tinycc/tcc" "$here/build/tcc"
echo "tcc: $(stat -c%s "$here/build/tcc") bytes  ($(file -b "$here/build/tcc" | cut -d, -f1-3))"
# build/hello.c is a committed source (a freestanding program), not generated.

echo "== [5/5] real linux/amd64 OCI image-layout (the registry format) =="
# Wrap the busybox layer as a genuine OCI image-layout — every blob named by its
# sha256 digest (== its κ-label) — so the witness ingests it through the production
# `oci::ingest_image` path (the exact resolution the deployed Manager's
# DevcontainerProvision drives) and boots it on x86-64. Built from the layer above.
python3 - "$here" <<'PY'
import json, hashlib, gzip, os, sys
base = sys.argv[1]
layer = open(f"{base}/rootfs/layer.tar.gz", "rb").read()
ld = hashlib.sha256(layer).hexdigest()
diff = hashlib.sha256(gzip.decompress(layer)).hexdigest()
cfg = json.dumps({"architecture": "amd64", "os": "linux",
                  "config": {"Cmd": ["/bin/busybox", "sh"]},
                  "rootfs": {"type": "layers", "diff_ids": [f"sha256:{diff}"]}},
                 separators=(",", ":"), sort_keys=True).encode()
cd = hashlib.sha256(cfg).hexdigest()
man = json.dumps({"schemaVersion": 2,
                  "mediaType": "application/vnd.oci.image.manifest.v1+json",
                  "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": f"sha256:{cd}", "size": len(cfg)},
                  "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": f"sha256:{ld}", "size": len(layer)}]},
                 separators=(",", ":"), sort_keys=True).encode()
md = hashlib.sha256(man).hexdigest()
idx = json.dumps({"schemaVersion": 2,
                  "mediaType": "application/vnd.oci.image.index.v1+json",
                  "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": f"sha256:{md}", "size": len(man),
                                 "platform": {"architecture": "amd64", "os": "linux"}}]},
                 separators=(",", ":"), sort_keys=True).encode()
img = f"{base}/image"
os.makedirs(f"{img}/blobs/sha256", exist_ok=True)
for dig, data in [(ld, layer), (cd, cfg), (md, man)]:
    open(f"{img}/blobs/sha256/{dig}", "wb").write(data)
open(f"{img}/index.json", "wb").write(idx)
open(f"{img}/oci-layout", "wb").write(b'{"imageLayoutVersion":"1.0.0"}')
print(f"OCI image: amd64, layer {ld[:12]}, manifest {md[:12]}")
PY

( cd "$here" && sha256sum linux/vmlinux.gz linux/bzImage linux/kernel.config \
    rootfs/busybox rootfs/layer.tar.gz init.sh build/tcc build/hello.c \
    image/oci-layout image/index.json image/blobs/sha256/* > cc45.sha256 )
echo "DONE"
