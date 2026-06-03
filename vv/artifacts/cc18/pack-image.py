#!/usr/bin/env python3
# Repackage the CC-18 OCI image (vv/artifacts/cc18/image/) with the freshly-built
# `lsp-demo` riscv64 static binary, deterministically (so the digests are
# reproducible). The layer is busybox (the CC-22 shell, reused byte-for-byte) +
# /usr/bin/lsp-demo. See SOURCE.txt for the binary build command.
#
# Usage: python3 vv/artifacts/cc18/pack-image.py
import json, gzip, tarfile, hashlib, io, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
IMG = os.path.join(HERE, "image")
BLOBS = os.path.join(IMG, "blobs", "sha256")
NEW_BIN = os.path.join(HERE, "lsp-demo", "target", "riscv64gc-unknown-linux-gnu", "release", "lsp-demo")

def read_blob(digest):
    return open(os.path.join(BLOBS, digest.split(":")[1]), "rb").read()

def write_blob(data: bytes) -> str:
    d = hashlib.sha256(data).hexdigest()
    open(os.path.join(BLOBS, d), "wb").write(data)
    return "sha256:" + d

# Pull the current layer's members (preserve busybox + the dir entries/modes).
idx = json.load(open(os.path.join(IMG, "index.json")))
mf = json.loads(read_blob(idx["manifests"][0]["digest"]))
old_layer = idx_layer = mf["layers"][0]["digest"]
src = tarfile.open(fileobj=gzip.open(io.BytesIO(read_blob(old_layer))))
members = {m.name: (m, src.extractfile(m).read() if m.isfile() else None) for m in src.getmembers()}

new_bin = open(NEW_BIN, "rb").read()
print(f"lsp-demo: {len(members['./usr/bin/lsp-demo'][1])} -> {len(new_bin)} bytes")

# Rebuild the layer tar deterministically (member order preserved; mtime/uid/gid 0).
order = ["./", "./bin", "./bin/busybox", "./usr", "./usr/bin", "./usr/bin/lsp-demo"]
raw = io.BytesIO()
with tarfile.open(fileobj=raw, mode="w") as tar:
    for name in order:
        key = name if name in members else name.rstrip("/")
        m, data = members[key]
        ti = tarfile.TarInfo(name=m.name)
        ti.mode, ti.uid, ti.gid, ti.mtime = m.mode, 0, 0, 0
        ti.uname, ti.gname = "", ""
        if m.isdir():
            ti.type = tarfile.DIRTYPE
            tar.addfile(ti)
        else:
            payload = new_bin if m.name.endswith("/lsp-demo") else data
            ti.size = len(payload)
            tar.addfile(ti, io.BytesIO(payload))
tar_bytes = raw.getvalue()
diff_id = "sha256:" + hashlib.sha256(tar_bytes).hexdigest()  # uncompressed digest
gz = io.BytesIO()
with gzip.GzipFile(fileobj=gz, mode="wb", mtime=0) as g:
    g.write(tar_bytes)
gz_bytes = gz.getvalue()

# Rewrite config (diff_ids), manifest (config + layer), index (manifest).
config = json.loads(read_blob(mf["config"]["digest"]))
config["rootfs"]["diff_ids"] = [diff_id]
config_bytes = json.dumps(config, separators=(",", ":"), sort_keys=True).encode()
config_digest = write_blob(config_bytes)
layer_digest = write_blob(gz_bytes)

mf["config"] = {"mediaType": mf["config"]["mediaType"], "digest": config_digest, "size": len(config_bytes)}
mf["layers"] = [{"mediaType": mf["layers"][0]["mediaType"], "digest": layer_digest, "size": len(gz_bytes)}]
mf_bytes = json.dumps(mf, separators=(",", ":"), sort_keys=True).encode()
mf_digest = write_blob(mf_bytes)

idx["manifests"][0] = {"mediaType": idx["manifests"][0]["mediaType"], "digest": mf_digest, "size": len(mf_bytes)}
if "platform" in mf or "platform" in str(idx["manifests"]):
    pass
open(os.path.join(IMG, "index.json"), "w").write(json.dumps(idx, separators=(",", ":"), sort_keys=True))

# Drop now-orphaned blobs (old layer/config/manifest) to keep the tree clean.
keep = {config_digest.split(":")[1], layer_digest.split(":")[1], mf_digest.split(":")[1]}
for f in os.listdir(BLOBS):
    if f not in keep:
        os.remove(os.path.join(BLOBS, f))

print("layer  ", layer_digest, len(gz_bytes))
print("config ", config_digest)
print("manifest", mf_digest)
print("lsp-demo sha256:", hashlib.sha256(new_bin).hexdigest())
