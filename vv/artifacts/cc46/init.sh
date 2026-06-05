#!/bin/busybox sh
# CC-46 — the arm64 devcontainer's PID 1 for the **real-kernel devbus parity**
# witness: the **stock linux-arm64 busybox** exercises all three substrate
# devices the shared `emulator::devbus` serves to the AArch64 core, at the same
# real-boot caliber as the RISC-V CC-15/CC-16/CC-33 rows:
#
#   • CC-15 (9p workspace): mount the holospaces-served virtio-9p share
#     (tag `hsworkspace`), read the file holospaces seeded, write a file back.
#   • CC-16 (network):      open an outbound TCP/HTTP flow over virtio-net
#     through the userspace NAT (wget to the NAT redirect target).
#   • CC-33 (bridge):       start a real server (busybox httpd on :8080)
#     reachable from the host over the in-process bridge.
#
# The kernel autoconfigures its virtio-net interface via DHCP (ip=dhcp on the
# cmdline). The init does the 9p round-trip and the outbound fetch first, then
# starts httpd and idles so the host can dial the listener over the bridge — it
# does NOT power off (the witness drops the machine once the bridge round-trip
# completes; a power-off would race the host's dial).
/bin/busybox mkdir -p /proc /sys /dev /bin /sbin /usr/bin /usr/sbin /mnt /www
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null
/bin/busybox --install -s
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
echo "CC46-DEVCONTAINER-UP"
echo "CC46-ARCH:$(uname -m)"

# ── CC-15: mount the shared 9p workspace and round-trip a file ───────────────
if mount -t 9p -o trans=virtio,version=9p2000.L hsworkspace /mnt 2>/dev/null; then
	echo "CC46-9P-MOUNTED"
	if [ -f /mnt/from-holospaces.txt ]; then
		echo "CC46-9P-READ:$(cat /mnt/from-holospaces.txt)"
	else
		echo "CC46-9P-READ-MISSING"
	fi
	echo "GUEST-WROTE-THIS" > /mnt/from-guest.txt && echo "CC46-9P-WROTE"
else
	echo "CC46-9P-MOUNT-FAILED"
fi

# ── CC-16: outbound TCP/HTTP through the NAT (virtio-net + userspace NAT) ────
# The NAT redirects 10.0.2.9:7777 to the host server (the witness's egress
# redirect). A successful fetch carries the host server's marker back.
ifconfig 2>&1 | grep -E 'inet |Link'
wget -O /tmp/out.txt http://10.0.2.9:7777/ 2>/tmp/wgeterr
echo "CC46-WGET-RC:$?"
echo "CC46-WGET-ERR:$(cat /tmp/wgeterr 2>/dev/null)"
if [ -s /tmp/out.txt ]; then
	echo "CC46-NET-RECV:$(cat /tmp/out.txt)"
else
	echo "CC46-NET-FETCH-FAILED"
fi
echo "CC46-NET-DONE"

# ── CC-33: a real guest server reachable over the in-process bridge ─────────
echo "HELLO-FROM-GUEST-SERVER" > /www/index.html
httpd -p 0.0.0.0:8080 -h /www
echo "CC46-SERVER-LISTENING"

# Idle so the host can dial the listener over the bridge (no power-off race).
while true; do
	sleep 1
done
