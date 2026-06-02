//! **Layer Assembler** — OCI image layers → a bootable `ext4` root filesystem.
//!
//! Realizes the *Rootfs Assembly* sub-process of the conceptual model's in-zoom
//! OPD *SD5 — Devcontainer Provisioning* and the *Layer Assembler* component of
//! the Boot Layer (arc42 chapter 5). It is the connector the gap analysis named:
//! [`crate::oci`] ingests and verifies an image's layers (`CC-10`); the
//! [`crate::disk`] κ-disk holds a finished filesystem image (`CC-7`); this module
//! turns the former into the latter.
//!
//! The pipeline is three real stages, each a published-spec authority:
//!   1. *decompress + untar* each layer — the GNU/POSIX **tar** (USTAR + PAX)
//!      format, gzip per RFC 1952 (the OCI layer media types
//!      `application/vnd.oci.image.layer.v1.tar` and `…tar+gzip`);
//!   2. *overlay* the layers in order applying the OCI image-layer **whiteout /
//!      opaque-directory** rules (the OCI image-spec "Layer" section) into one
//!      unified filesystem tree;
//!   3. *serialize* that tree into a real **ext4** image (the in-crate writer in
//!      the `ext4` submodule) — written to a κ-disk, every sector κ-addressed
//!      content over the store (Law L4: no second medium; the writer is the
//!      projection from the κ-tree to the block device).
//!
//! Nothing here shells out to `tar`, `gzip`, or `mke2fs` — the whole flow is
//! holospaces operating on its own content (Law L4). e2fsprogs (`e2fsck`,
//! `dumpe2fs`) and the Linux kernel mount are the *V&V oracles* that verify the
//! produced image, not runtime dependencies (`CC-14`).

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub mod ext4;

/// An ingested image layer to overlay: its OCI media type and the (verified)
/// blob bytes as held in the store (`CC-10`).
pub struct Layer<'a> {
    /// The OCI layer media type (selects the decompression: plain tar, gzip).
    pub media_type: &'a str,
    /// The layer blob — the bytes the store holds for the layer's κ.
    pub blob: &'a [u8],
}

/// What can go wrong assembling a rootfs from layers.
#[derive(Debug, PartialEq, Eq)]
pub enum AssemblyError {
    /// A layer's media type is not a tar variant this assembler decompresses.
    UnsupportedMediaType(String),
    /// The gzip stream is malformed (bad magic / method / truncated).
    BadGzip(String),
    /// The zstd stream is malformed (bad frame / truncated).
    BadZstd(String),
    /// The tar stream is malformed (bad header / truncated / bad numeric field).
    BadTar(String),
    /// A whiteout or entry names a path that escapes the root (`..` / absolute).
    BadPath(String),
    /// The ext4 writer could not serialize the tree (see [`ext4::Ext4Error`]).
    Ext4(ext4::Ext4Error),
}

impl core::fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AssemblyError::UnsupportedMediaType(m) => {
                write!(f, "unsupported layer media type: {m}")
            }
            AssemblyError::BadGzip(m) => write!(f, "malformed gzip: {m}"),
            AssemblyError::BadZstd(m) => write!(f, "malformed zstd: {m}"),
            AssemblyError::BadTar(m) => write!(f, "malformed tar: {m}"),
            AssemblyError::BadPath(m) => write!(f, "path escapes root: {m}"),
            AssemblyError::Ext4(e) => write!(f, "ext4 serialization: {e}"),
        }
    }
}

impl From<ext4::Ext4Error> for AssemblyError {
    fn from(e: ext4::Ext4Error) -> Self {
        AssemblyError::Ext4(e)
    }
}

// ── The unified filesystem tree ────────────────────────────────────────────
//
// The overlay's output is a tree of inodes. Files carry a *content id* so that
// hard links share one inode (POSIX semantics, e2fsck-clean link counts), and
// the actual bytes live in [`Tree::contents`] keyed by that id.

/// A POSIX file-type + metadata, shared by every node kind.
#[derive(Clone)]
pub struct Meta {
    /// The low 12 bits are the permission bits; the type is implied by the node.
    pub mode: u16,
    /// Owner user id.
    pub uid: u32,
    /// Owner group id.
    pub gid: u32,
    /// Modification time (Unix seconds); used for atime/ctime/mtime/crtime.
    pub mtime: u32,
}

/// One node of the unified filesystem tree.
pub enum Node {
    /// A directory and its entries (name → child), ordered for reproducibility.
    Dir {
        /// The directory's metadata.
        meta: Meta,
        /// The directory's entries, by name.
        children: BTreeMap<String, Node>,
    },
    /// A regular file; `content` indexes [`Tree::contents`] (shared by hard links).
    File {
        /// The file's metadata.
        meta: Meta,
        /// Key into [`Tree::contents`] for the file's bytes (shared by hard links).
        content: u32,
    },
    /// A symbolic link to `target` (raw bytes, may be relative or absolute).
    Symlink {
        /// The symlink's metadata.
        meta: Meta,
        /// The link target bytes.
        target: Vec<u8>,
    },
    /// A device / fifo / socket special file. `rdev` packs major/minor.
    Special {
        /// The special file's metadata.
        meta: Meta,
        /// The inode mode's *type* bits (S_IFCHR / S_IFBLK / S_IFIFO / S_IFSOCK).
        ifmt: u16,
        /// The device number (major/minor) for char/block devices; 0 otherwise.
        rdev: u32,
    },
}

/// The assembled, overlaid root filesystem — the input to the ext4 writer.
pub struct Tree {
    /// The root directory node (always a `Node::Dir`).
    pub root: Node,
    /// File contents, keyed by content id; hard-linked files share an id.
    pub contents: BTreeMap<u32, Vec<u8>>,
}

// ── Stage 1 + 2: decompress, untar, overlay ────────────────────────────────

/// Assemble OCI image `layers` (lowest first) into a complete, bootable `ext4`
/// filesystem image — the bytes a [`crate::disk::KappaDisk`] holds (`CC-7`) and
/// the emulator's `virtio-blk` reads (`CC-14`). The whole flow (decompress →
/// untar → overlay → ext4) is in-crate (Law L4).
pub fn assemble_ext4(layers: &[Layer]) -> Result<Vec<u8>, AssemblyError> {
    let tree = overlay_layers(layers)?;
    Ok(ext4::write_image(&tree)?)
}

/// Assemble `layers` into an ext4 image, but with `/init` set to `init_bytes`
/// (mode `0755`) — the Boot Orchestrator's hook for injecting the **dev-container
/// lifecycle runner** (`CC-22`): a script that runs the config's lifecycle
/// commands (`postCreateCommand`, …) in the booted OS so the environment is ready
/// on entry. Overrides any `/init` the base image carried.
pub fn assemble_ext4_with_init(
    layers: &[Layer],
    init_bytes: &[u8],
) -> Result<Vec<u8>, AssemblyError> {
    assemble_ext4_with_files(layers, &[("init", 0o755, init_bytes)])
}

/// Assemble `layers` into an ext4 image with extra `files` injected — each an
/// `(absolute path, mode, bytes)` — creating intermediate directories as needed
/// and overriding any colliding entry. The Boot Orchestrator's hook for placing
/// the lifecycle runner (`/init`, `CC-22`) and the operator's dotfiles
/// (`/root/.gitconfig`, …) and entry runner into the booted OS so personalization
/// is applied on entry (`CC-23`).
pub fn assemble_ext4_with_files(
    layers: &[Layer],
    files: &[(&str, u16, &[u8])],
) -> Result<Vec<u8>, AssemblyError> {
    let mut tree = overlay_layers(layers)?;
    let base = tree.contents.keys().copied().max().map_or(0, |m| m + 1);
    for (i, (path, mode, bytes)) in files.iter().enumerate() {
        let id = base + i as u32;
        tree.contents.insert(id, bytes.to_vec());
        insert_file_at(&mut tree.root, path.trim_start_matches('/'), *mode, id);
    }
    Ok(ext4::write_image(&tree)?)
}

/// Insert a file at `rel` (a `/`-separated path relative to `dir`), creating any
/// missing intermediate directories (mode `0755`) and replacing a colliding
/// non-directory along the way. A no-op if `dir` is not a directory.
fn insert_file_at(dir: &mut Node, rel: &str, mode: u16, content: u32) {
    let Node::Dir { children, .. } = dir else {
        return;
    };
    match rel.split_once('/') {
        None => {
            let meta = Meta {
                mode,
                uid: 0,
                gid: 0,
                mtime: 0,
            };
            children.insert(rel.into(), Node::File { meta, content });
        }
        Some((head, tail)) => {
            let entry = children.entry(head.into()).or_insert_with(|| Node::Dir {
                meta: Meta {
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    mtime: 0,
                },
                children: BTreeMap::new(),
            });
            if !matches!(entry, Node::Dir { .. }) {
                *entry = Node::Dir {
                    meta: Meta {
                        mode: 0o755,
                        uid: 0,
                        gid: 0,
                        mtime: 0,
                    },
                    children: BTreeMap::new(),
                };
            }
            insert_file_at(entry, tail, mode, content);
        }
    }
}

/// Find the Dev Container config inside a repository archive `layer` (a `tar` or
/// `tar+gzip`, e.g. a git-host archive). Matches `.devcontainer/devcontainer.json`
/// or a top-level `.devcontainer.json`, ignoring the archive's leading directory
/// component (git hosts wrap the tree in `<repo>-<ref>/…`). Returns the config
/// bytes, or `None` if the repository declares no devcontainer.
pub fn find_devcontainer_json(layer: &Layer) -> Result<Option<Vec<u8>>, AssemblyError> {
    let tar = decompress(layer)?;
    for entry in tar::Reader::new(&tar) {
        let entry = entry?;
        if entry.kind != tar::Kind::File {
            continue;
        }
        // Strip the archive's leading path component (e.g. "repo-main/").
        let rel = match entry.path.split_once('/') {
            Some((_, rest)) => rest,
            None => entry.path.as_str(),
        };
        if rel == ".devcontainer/devcontainer.json" || rel == ".devcontainer.json" {
            return Ok(Some(entry.data));
        }
    }
    Ok(None)
}

/// Read a single file at `want` (a `/`-separated path) from a repository
/// `archive` layer (a `tar`/`tar+gzip`), ignoring the archive's leading directory
/// component (git hosts wrap the tree in `<repo>-<ref>/…`). Returns the file
/// bytes, or `None` if absent. Used to read a Dockerfile and its `COPY` sources
/// from the build context (`CC-26`). `want` may itself be prefixed `./`.
pub fn read_archive_file(archive: &Layer, want: &str) -> Result<Option<Vec<u8>>, AssemblyError> {
    let want = want.trim_start_matches("./").trim_start_matches('/');
    let tar = decompress(archive)?;
    for entry in tar::Reader::new(&tar) {
        let entry = entry?;
        if entry.kind != tar::Kind::File {
            continue;
        }
        let rel = match entry.path.split_once('/') {
            Some((_, rest)) => rest,
            None => entry.path.as_str(),
        };
        if rel.trim_start_matches("./") == want {
            return Ok(Some(entry.data));
        }
    }
    Ok(None)
}

/// Extract every regular file from an OCI artifact `layer` (a `tar` or
/// `tar+gzip`) as `(path, mode, bytes)`, with any leading `./` stripped. The Boot
/// Orchestrator uses this to unpack a Dev Container *feature* artifact (its
/// `install.sh` + `devcontainer-feature.json` + any helpers) so it can place the
/// feature into the rootfs to be installed in the OS (`CC-25`).
pub fn extract_layer_files(layer: &Layer) -> Result<Vec<(String, u16, Vec<u8>)>, AssemblyError> {
    let tar = decompress(layer)?;
    let mut files = Vec::new();
    for entry in tar::Reader::new(&tar) {
        let entry = entry?;
        if entry.kind != tar::Kind::File {
            continue;
        }
        let path = entry.path.trim_start_matches("./").to_string();
        if !path.is_empty() {
            files.push((path, entry.mode, entry.data));
        }
    }
    Ok(files)
}

/// Overlay `layers` (lowest first) into one unified filesystem [`Tree`],
/// applying the OCI whiteout / opaque-directory rules.
pub fn overlay_layers(layers: &[Layer]) -> Result<Tree, AssemblyError> {
    let mut builder = TreeBuilder::new();
    for layer in layers {
        let tar = decompress(layer)?;
        builder.apply_layer(&tar)?;
    }
    Ok(builder.finish())
}

/// Decompress a layer (or repository archive) blob to its raw tar bytes. The
/// codec is chosen from the compression magic first (authoritative — gzip's
/// `1f 8b`, zstd's `28 b5 2f fd`), then the media type — so an OCI `tar+gzip`
/// layer, a `tar+zstd` layer, a bare `application/gzip` repository archive, and a
/// plain `tar` all work.
fn decompress(layer: &Layer) -> Result<Vec<u8>, AssemblyError> {
    let b = layer.blob;
    let gz = b.len() >= 2 && b[0] == 0x1f && b[1] == 0x8b;
    let zst = b.len() >= 4 && b[0] == 0x28 && b[1] == 0xb5 && b[2] == 0x2f && b[3] == 0xfd;
    let mt = layer.media_type;
    if gz || mt.contains("gzip") {
        gunzip(b)
    } else if zst || mt.contains("zstd") {
        unzstd(b, mt)
    } else if mt.contains("tar") || mt.is_empty() {
        Ok(b.to_vec()) // plain (uncompressed) tar
    } else {
        Err(AssemblyError::UnsupportedMediaType(mt.to_string()))
    }
}

/// Decode a Zstandard frame (RFC 8878) to bytes — OCI `tar+zstd` layers. A `std`
/// surface (the realized ingestion peers build with std); the bare-metal core
/// reports it unsupported rather than silently mis-reading the layer.
#[cfg(feature = "std")]
fn unzstd(data: &[u8], _mt: &str) -> Result<Vec<u8>, AssemblyError> {
    use std::io::Read;
    let mut dec = ruzstd::StreamingDecoder::new(data)
        .map_err(|e| AssemblyError::BadZstd(alloc::format!("{e}")))?;
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| AssemblyError::BadZstd(alloc::format!("{e}")))?;
    Ok(out)
}

#[cfg(not(feature = "std"))]
fn unzstd(_data: &[u8], mt: &str) -> Result<Vec<u8>, AssemblyError> {
    // The bare-metal peer core links no zstd decoder; report it loudly.
    Err(AssemblyError::UnsupportedMediaType(mt.to_string()))
}

/// Inflate a gzip member (RFC 1952 framing + RFC 1951 DEFLATE) to bytes.
fn gunzip(data: &[u8]) -> Result<Vec<u8>, AssemblyError> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b {
        return Err(AssemblyError::BadGzip("bad magic".to_string()));
    }
    if data[2] != 0x08 {
        return Err(AssemblyError::BadGzip("not DEFLATE".to_string()));
    }
    let flg = data[3];
    let mut pos = 10usize; // fixed header
    if flg & 0x04 != 0 {
        // FEXTRA: 2-byte length + that many bytes.
        if pos + 2 > data.len() {
            return Err(AssemblyError::BadGzip("truncated FEXTRA".to_string()));
        }
        let xlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2 + xlen;
    }
    if flg & 0x08 != 0 {
        // FNAME: NUL-terminated.
        pos = skip_cstr(data, pos)?;
    }
    if flg & 0x10 != 0 {
        // FCOMMENT: NUL-terminated.
        pos = skip_cstr(data, pos)?;
    }
    if flg & 0x02 != 0 {
        pos += 2; // FHCRC
    }
    if pos > data.len() {
        return Err(AssemblyError::BadGzip("truncated header".to_string()));
    }
    // The DEFLATE stream is between the header and the 8-byte trailer (CRC32 + ISIZE).
    let deflate = &data[pos..data.len() - 8];
    miniz_oxide::inflate::decompress_to_vec(deflate)
        .map_err(|e| AssemblyError::BadGzip(alloc::format!("inflate: {:?}", e.status)))
}

fn skip_cstr(data: &[u8], mut pos: usize) -> Result<usize, AssemblyError> {
    while pos < data.len() && data[pos] != 0 {
        pos += 1;
    }
    if pos >= data.len() {
        return Err(AssemblyError::BadGzip(
            "unterminated header string".to_string(),
        ));
    }
    Ok(pos + 1)
}

// ── The overlay builder ────────────────────────────────────────────────────

/// Accumulates layers into the unified tree. The root is a directory; entries
/// are applied in tar order; whiteouts delete; opaque markers clear lower
/// children; hard links share a content id.
struct TreeBuilder {
    root: DirEnt,
    contents: BTreeMap<u32, Vec<u8>>,
    /// content id of an already-extracted regular file, by absolute path —
    /// so a hard link to it reuses the same inode (id).
    file_ids: BTreeMap<String, u32>,
    next_id: u32,
}

/// Mutable tree node used while building (converted to [`Node`] at the end).
enum DirEnt {
    Dir {
        meta: Meta,
        children: BTreeMap<String, DirEnt>,
    },
    File {
        meta: Meta,
        content: u32,
    },
    Symlink {
        meta: Meta,
        target: Vec<u8>,
    },
    Special {
        meta: Meta,
        ifmt: u16,
        rdev: u32,
    },
}

impl TreeBuilder {
    fn new() -> Self {
        TreeBuilder {
            root: DirEnt::Dir {
                meta: Meta {
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    mtime: 0,
                },
                children: BTreeMap::new(),
            },
            contents: BTreeMap::new(),
            file_ids: BTreeMap::new(),
            next_id: 0,
        }
    }

    fn apply_layer(&mut self, tar: &[u8]) -> Result<(), AssemblyError> {
        for entry in tar::Reader::new(tar) {
            let entry = entry?;
            self.apply_entry(entry)?;
        }
        Ok(())
    }

    fn apply_entry(&mut self, e: tar::Entry) -> Result<(), AssemblyError> {
        let comps = split_path(&e.path)?;
        if comps.is_empty() {
            return Ok(()); // the root itself / "./"
        }
        let (parent_comps, name) = comps.split_at(comps.len() - 1);
        let name = name[0].clone();

        // OCI whiteouts (the basename carries the marker).
        const WH: &str = ".wh.";
        const OPQ: &str = ".wh..wh..opq";
        if name == OPQ {
            // Opaque: drop everything the lower layers put in this directory.
            let dir = self.dir_at(parent_comps)?;
            if let DirEnt::Dir { children, .. } = dir {
                children.clear();
            }
            return Ok(());
        }
        if let Some(target) = name.strip_prefix(WH) {
            // Whiteout: remove `target` from the parent directory.
            let dir = self.dir_at(parent_comps)?;
            if let DirEnt::Dir { children, .. } = dir {
                children.remove(target);
            }
            return Ok(());
        }

        let meta = Meta {
            mode: e.mode,
            uid: e.uid,
            gid: e.gid,
            mtime: e.mtime,
        };
        let node = match e.kind {
            tar::Kind::Dir => {
                // Merge: if the directory already exists, keep its children and
                // just refresh its metadata (an upper layer may re-list a dir).
                let parent = self.dir_at(parent_comps)?;
                if let DirEnt::Dir { children, .. } = parent {
                    match children.get_mut(&name) {
                        Some(DirEnt::Dir { meta: m, .. }) => {
                            *m = meta;
                            return Ok(());
                        }
                        _ => DirEnt::Dir {
                            meta,
                            children: BTreeMap::new(),
                        },
                    }
                } else {
                    return Err(AssemblyError::BadPath(e.path.clone()));
                }
            }
            tar::Kind::File => {
                let id = self.next_id;
                self.next_id += 1;
                self.contents.insert(id, e.data);
                self.file_ids.insert(e.path.clone(), id);
                DirEnt::File { meta, content: id }
            }
            tar::Kind::HardLink => {
                // Share the content id of the (already-seen) link target.
                let target = normalize(&e.link)?;
                let id = self.file_ids.get(&target).copied().ok_or_else(|| {
                    AssemblyError::BadTar(alloc::format!("dangling hard link to {target}"))
                })?;
                self.file_ids.insert(e.path.clone(), id);
                DirEnt::File { meta, content: id }
            }
            tar::Kind::Symlink => DirEnt::Symlink {
                meta,
                target: e.link.into_bytes(),
            },
            tar::Kind::Char | tar::Kind::Block | tar::Kind::Fifo => {
                let ifmt = match e.kind {
                    tar::Kind::Char => ext4::S_IFCHR,
                    tar::Kind::Block => ext4::S_IFBLK,
                    _ => ext4::S_IFIFO,
                };
                DirEnt::Special {
                    meta,
                    ifmt,
                    rdev: makedev(e.devmajor, e.devminor),
                }
            }
        };

        let parent = self.dir_at(parent_comps)?;
        if let DirEnt::Dir { children, .. } = parent {
            children.insert(name, node);
            Ok(())
        } else {
            Err(AssemblyError::BadPath(e.path.clone()))
        }
    }

    /// Resolve (creating intermediate directories) to the directory at `comps`.
    fn dir_at(&mut self, comps: &[String]) -> Result<&mut DirEnt, AssemblyError> {
        let mut cur = &mut self.root;
        for c in comps {
            let children = match cur {
                DirEnt::Dir { children, .. } => children,
                _ => return Err(AssemblyError::BadPath(c.clone())),
            };
            cur = children.entry(c.clone()).or_insert_with(|| DirEnt::Dir {
                meta: Meta {
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    mtime: 0,
                },
                children: BTreeMap::new(),
            });
        }
        Ok(cur)
    }

    fn finish(self) -> Tree {
        Tree {
            root: to_node(self.root),
            contents: self.contents,
        }
    }
}

fn to_node(d: DirEnt) -> Node {
    match d {
        DirEnt::Dir { meta, children } => Node::Dir {
            meta,
            children: children.into_iter().map(|(k, v)| (k, to_node(v))).collect(),
        },
        DirEnt::File { meta, content } => Node::File { meta, content },
        DirEnt::Symlink { meta, target } => Node::Symlink { meta, target },
        DirEnt::Special { meta, ifmt, rdev } => Node::Special { meta, ifmt, rdev },
    }
}

fn makedev(major: u32, minor: u32) -> u32 {
    // Linux legacy 16-bit device encoding (kept small; OCI specials are tiny).
    ((major & 0xfff) << 8) | (minor & 0xff) | ((minor & 0xfff00) << 12)
}

/// Split a tar path into clean components, rejecting `..` and absolute escapes.
fn split_path(path: &str) -> Result<Vec<String>, AssemblyError> {
    let mut out = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(AssemblyError::BadPath(path.to_string())),
            p => out.push(p.to_string()),
        }
    }
    Ok(out)
}

/// Normalize a path to the canonical `a/b/c` form used as the hard-link key.
fn normalize(path: &str) -> Result<String, AssemblyError> {
    Ok(split_path(path)?.join("/"))
}

// ── The tar reader (USTAR + PAX + GNU long names) ──────────────────────────

mod tar {
    use super::{AssemblyError, String, ToString, Vec};
    use alloc::vec;

    /// The node kinds a tar entry can denote (the subset a rootfs uses).
    #[derive(PartialEq, Eq, Clone, Copy)]
    pub enum Kind {
        File,
        Dir,
        Symlink,
        HardLink,
        Char,
        Block,
        Fifo,
    }

    /// One decoded tar member.
    pub struct Entry {
        pub path: String,
        pub link: String,
        pub kind: Kind,
        pub mode: u16,
        pub uid: u32,
        pub gid: u32,
        pub mtime: u32,
        pub devmajor: u32,
        pub devminor: u32,
        pub data: Vec<u8>,
    }

    /// A streaming reader over the 512-byte-blocked tar format.
    pub struct Reader<'a> {
        data: &'a [u8],
        pos: usize,
        done: bool,
        // PAX / GNU overrides that apply to the *next* regular header.
        pax_path: Option<String>,
        pax_link: Option<String>,
        pax_size: Option<u64>,
        gnu_long_name: Option<String>,
        gnu_long_link: Option<String>,
    }

    impl<'a> Reader<'a> {
        pub fn new(data: &'a [u8]) -> Self {
            Reader {
                data,
                pos: 0,
                done: false,
                pax_path: None,
                pax_link: None,
                pax_size: None,
                gnu_long_name: None,
                gnu_long_link: None,
            }
        }

        fn block(&self, off: usize) -> Option<&'a [u8]> {
            self.data.get(off..off + 512)
        }
    }

    impl<'a> Iterator for Reader<'a> {
        type Item = Result<Entry, AssemblyError>;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                if self.done {
                    return None;
                }
                // Ran out of blocks — tolerate a missing end marker.
                let hdr = self.block(self.pos)?;
                // Two consecutive zero blocks (here: one all-zero header) = end.
                if hdr.iter().all(|&b| b == 0) {
                    self.done = true;
                    return None;
                }
                self.pos += 512;

                let typeflag = hdr[156];
                let size = match self.pax_size.take().or_else(|| parse_size(hdr).ok()) {
                    Some(s) => s as usize,
                    None => return Some(Err(AssemblyError::BadTar("bad size field".to_string()))),
                };
                let body_end = self.pos + size;
                let data = match self.data.get(self.pos..body_end) {
                    Some(d) => d.to_vec(),
                    None => return Some(Err(AssemblyError::BadTar("truncated body".to_string()))),
                };
                // Advance past the body, rounded up to the 512 block.
                self.pos = body_end.div_ceil(512) * 512;

                match typeflag {
                    b'x' | b'X' => {
                        // PAX extended header: records apply to the next entry.
                        if let Err(e) = self.parse_pax(&data) {
                            return Some(Err(e));
                        }
                        continue;
                    }
                    b'g' => continue, // PAX global header — ignored
                    b'L' => {
                        self.gnu_long_name = Some(cstr(&data));
                        continue;
                    }
                    b'K' => {
                        self.gnu_long_link = Some(cstr(&data));
                        continue;
                    }
                    _ => {}
                }

                let kind = match typeflag {
                    b'0' | 0 | b'7' => Kind::File,
                    b'5' => Kind::Dir,
                    b'2' => Kind::Symlink,
                    b'1' => Kind::HardLink,
                    b'3' => Kind::Char,
                    b'4' => Kind::Block,
                    b'6' => Kind::Fifo,
                    other => {
                        return Some(Err(AssemblyError::BadTar(alloc::format!(
                            "unsupported typeflag {other:#x}"
                        ))))
                    }
                };

                let path = self
                    .pax_path
                    .take()
                    .or_else(|| self.gnu_long_name.take())
                    .unwrap_or_else(|| header_name(hdr));
                let link = self
                    .pax_link
                    .take()
                    .or_else(|| self.gnu_long_link.take())
                    .unwrap_or_else(|| cstr_field(&hdr[157..257]));

                let entry = Entry {
                    path,
                    link,
                    kind,
                    mode: (octal(&hdr[100..108]).unwrap_or(0) & 0o7777) as u16,
                    uid: octal(&hdr[108..116]).unwrap_or(0) as u32,
                    gid: octal(&hdr[116..124]).unwrap_or(0) as u32,
                    mtime: octal(&hdr[136..148]).unwrap_or(0) as u32,
                    devmajor: octal(&hdr[329..337]).unwrap_or(0) as u32,
                    devminor: octal(&hdr[337..345]).unwrap_or(0) as u32,
                    data: if kind == Kind::File { data } else { vec![] },
                };
                return Some(Ok(entry));
            }
        }
    }

    impl<'a> Reader<'a> {
        fn parse_pax(&mut self, data: &[u8]) -> Result<(), AssemblyError> {
            // Records: "<len> <key>=<value>\n", len counts the whole record.
            let mut i = 0;
            while i < data.len() {
                let start = i;
                // Read the decimal length prefix.
                let mut len = 0usize;
                while i < data.len() && data[i].is_ascii_digit() {
                    len = len * 10 + (data[i] - b'0') as usize;
                    i += 1;
                }
                if len == 0 || start + len > data.len() || i >= data.len() || data[i] != b' ' {
                    break; // tolerate trailing padding
                }
                let record = &data[start..start + len];
                // Skip "<len> "
                let kv = &record[(i - start + 1)..record.len().saturating_sub(1)]; // drop trailing \n
                if let Some(eq) = kv.iter().position(|&b| b == b'=') {
                    let key = &kv[..eq];
                    let val = &kv[eq + 1..];
                    match key {
                        b"path" => self.pax_path = Some(String::from_utf8_lossy(val).into_owned()),
                        b"linkpath" => {
                            self.pax_link = Some(String::from_utf8_lossy(val).into_owned())
                        }
                        b"size" => {
                            self.pax_size = core::str::from_utf8(val)
                                .ok()
                                .and_then(|s| s.parse::<u64>().ok());
                        }
                        _ => {}
                    }
                }
                i = start + len;
            }
            Ok(())
        }
    }

    fn header_name(hdr: &[u8]) -> String {
        let name = cstr_field(&hdr[0..100]);
        let prefix = cstr_field(&hdr[345..500]);
        if prefix.is_empty() {
            name
        } else {
            alloc::format!("{prefix}/{name}")
        }
    }

    /// A NUL-terminated (or full-length) fixed field as a String.
    fn cstr_field(b: &[u8]) -> String {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).into_owned()
    }

    /// A NUL-terminated value from a variable buffer (GNU long name body).
    fn cstr(b: &[u8]) -> String {
        cstr_field(b)
    }

    /// Parse an octal numeric field, or GNU base-256 (high bit of byte 0 set).
    fn octal(b: &[u8]) -> Option<u64> {
        if b.is_empty() {
            return None;
        }
        if b[0] & 0x80 != 0 {
            // GNU base-256 big-endian (low 7 bits of the first byte included).
            let mut v: u64 = (b[0] & 0x7f) as u64;
            for &x in &b[1..] {
                v = (v << 8) | x as u64;
            }
            return Some(v);
        }
        let s: Vec<u8> = b.iter().copied().filter(|&c| c != 0 && c != b' ').collect();
        if s.is_empty() {
            return Some(0);
        }
        let mut v = 0u64;
        for &c in &s {
            if !(b'0'..=b'7').contains(&c) {
                return None;
            }
            v = v * 8 + (c - b'0') as u64;
        }
        Some(v)
    }

    fn parse_size(hdr: &[u8]) -> Result<u64, AssemblyError> {
        octal(&hdr[124..136]).ok_or_else(|| AssemblyError::BadTar("bad size".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gunzip_round_trips_a_real_gzip_member() {
        // A tiny gzip of "hello\n" produced by gzip(1) — fixed bytes.
        let gz: &[u8] = &[
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xcb, 0x48, 0xcd, 0xc9,
            0xc9, 0xe7, 0x02, 0x00, 0x20, 0x30, 0x3a, 0x36, 0x06, 0x00, 0x00, 0x00,
        ];
        assert_eq!(gunzip(gz).unwrap(), b"hello\n");
    }

    #[test]
    fn decompresses_a_real_zstd_layer() {
        // A real Zstandard frame of "hello zstd layer\n" (produced by zstd(1)).
        // OCI `tar+zstd` layers (BuildKit / modern registries) must decode, not
        // be rejected. Detection is by magic (`28 b5 2f fd`) and by media type.
        let frame: &[u8] = &[
            0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x58, 0x89, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f,
            0x20, 0x7a, 0x73, 0x74, 0x64, 0x20, 0x6c, 0x61, 0x79, 0x65, 0x72, 0x0a, 0x59, 0xd0,
            0x02, 0xda,
        ];
        let by_magic = decompress(&Layer {
            media_type: "application/octet-stream",
            blob: frame,
        })
        .unwrap();
        assert_eq!(by_magic, b"hello zstd layer\n");
        let by_media = decompress(&Layer {
            media_type: "application/vnd.oci.image.layer.v1.tar+zstd",
            blob: frame,
        })
        .unwrap();
        assert_eq!(by_media, b"hello zstd layer\n");
    }

    #[test]
    fn a_name_over_255_bytes_is_an_explicit_error_not_truncated() {
        // ext4's directory-entry name length is one byte; a 300-byte name cannot
        // be represented and must be reported loudly, not truncated to a collision.
        let long = "x".repeat(300);
        let err = assemble_ext4_with_files(&[], &[(long.as_str(), 0o644, b"data")]).unwrap_err();
        match err {
            AssemblyError::Ext4(ext4::Ext4Error::NameTooLong(n)) => assert_eq!(n.len(), 300),
            other => panic!("expected NameTooLong, got {other:?}"),
        }
        // A 255-byte name is at the limit and is accepted.
        let ok = "y".repeat(255);
        assert!(assemble_ext4_with_files(&[], &[(ok.as_str(), 0o644, b"data")]).is_ok());
    }

    #[test]
    fn overlays_a_tar_into_a_tree() {
        // Build a minimal uncompressed tar in memory: /etc/ (dir) + /etc/hi (file).
        let tar = make_tar(&[(b"etc/", b'5', b"", b""), (b"etc/hi", b'0', b"", b"hello")]);
        let layers = [Layer {
            media_type: "application/vnd.oci.image.layer.v1.tar",
            blob: &tar,
        }];
        let tree = overlay_layers(&layers).unwrap();
        let Node::Dir { children, .. } = &tree.root else {
            panic!("root not a dir")
        };
        let Node::Dir { children: etc, .. } = &children["etc"] else {
            panic!("etc not a dir")
        };
        let Node::File { content, .. } = &etc["hi"] else {
            panic!("hi not a file")
        };
        assert_eq!(tree.contents[content], b"hello");
    }

    #[test]
    fn whiteout_removes_a_lower_entry() {
        let lower = make_tar(&[(b"a", b'0', b"", b"1"), (b"b", b'0', b"", b"2")]);
        let upper = make_tar(&[(b".wh.a", b'0', b"", b"")]);
        let tree = overlay_layers(&[
            Layer {
                media_type: "tar",
                blob: &lower,
            },
            Layer {
                media_type: "tar",
                blob: &upper,
            },
        ])
        .unwrap();
        let Node::Dir { children, .. } = &tree.root else {
            panic!()
        };
        assert!(!children.contains_key("a"), "whiteout should remove a");
        assert!(children.contains_key("b"), "b should remain");
    }

    /// A test tar entry: (name, typeflag, linkname, data).
    type TarSpec<'a> = (&'a [u8], u8, &'a [u8], &'a [u8]);

    /// Assemble a minimal uncompressed USTAR archive for tests.
    fn make_tar(entries: &[TarSpec]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, typeflag, link, data) in entries {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name);
            write_octal(&mut h[100..108], 0o644);
            write_octal(&mut h[108..116], 0);
            write_octal(&mut h[116..124], 0);
            write_octal(&mut h[124..136], data.len() as u64);
            write_octal(&mut h[136..148], 0);
            h[156] = *typeflag;
            h[157..157 + link.len()].copy_from_slice(link);
            h[257..263].copy_from_slice(b"ustar\0");
            h[263] = b'0';
            h[264] = b'0';
            // checksum: sum of bytes with the checksum field as spaces.
            for c in h.iter_mut().skip(148).take(8) {
                *c = b' ';
            }
            let sum: u32 = h.iter().map(|&b| b as u32).sum();
            write_octal(&mut h[148..155], sum as u64);
            h[155] = b' ';
            out.extend_from_slice(&h);
            out.extend_from_slice(data);
            let pad = data.len().div_ceil(512) * 512 - data.len();
            out.resize(out.len() + pad, 0);
        }
        out.extend([0u8; 1024]); // two zero end blocks
        out
    }

    fn write_octal(field: &mut [u8], v: u64) {
        let s = alloc::format!("{:0width$o}", v, width = field.len() - 1);
        field[..s.len()].copy_from_slice(s.as_bytes());
    }

    /// Emit a representative ext4 image to /tmp for the external oracles
    /// (`e2fsck`, loopback `mount`). Run on demand: `cargo test -p holospaces
    /// writes_a_mountable_ext4_image -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn writes_a_mountable_ext4_image() {
        use super::ext4;
        use alloc::collections::BTreeMap;

        let meta = || Meta {
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
        };
        let dmeta = || Meta {
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime: 0,
        };
        let mut contents = BTreeMap::new();
        contents.insert(0u32, b"#!/bin/sh\necho hello from holospaces\n".to_vec());
        contents.insert(1u32, alloc::vec![0x42u8; 200_000]); // multi-block file

        let mut etc = BTreeMap::new();
        etc.insert(
            "hostname".to_string(),
            Node::File {
                meta: meta(),
                content: 0,
            },
        );
        let mut bin = BTreeMap::new();
        bin.insert(
            "run".to_string(),
            Node::File {
                meta: Meta {
                    mode: 0o755,
                    ..meta()
                },
                content: 0,
            },
        );
        bin.insert(
            "blob".to_string(),
            Node::File {
                meta: meta(),
                content: 1,
            },
        );
        bin.insert(
            "sh".to_string(),
            Node::Symlink {
                meta: Meta {
                    mode: 0o777,
                    ..meta()
                },
                target: b"/bin/run".to_vec(),
            },
        );
        let mut root_children = BTreeMap::new();
        root_children.insert(
            "etc".to_string(),
            Node::Dir {
                meta: dmeta(),
                children: etc,
            },
        );
        root_children.insert(
            "bin".to_string(),
            Node::Dir {
                meta: dmeta(),
                children: bin,
            },
        );
        let tree = Tree {
            root: Node::Dir {
                meta: dmeta(),
                children: root_children,
            },
            contents,
        };

        let img = ext4::write_image(&tree).expect("ext4 write");
        std::fs::write("/tmp/holospaces-asm.img", &img).unwrap();
        std::eprintln!("wrote /tmp/holospaces-asm.img ({} bytes)", img.len());
    }

    /// Emit a multi-block-group image (>128 MiB → ≥5 groups, exercising the
    /// geometry sizing loop, backup superblocks, and per-group bitmaps). The
    /// external oracle (`e2fsck`) must find it clean. Run on demand.
    #[test]
    #[ignore]
    fn writes_a_multigroup_ext4_image() {
        use super::ext4;
        use alloc::collections::BTreeMap;

        let mut contents = BTreeMap::new();
        let mut root_children = BTreeMap::new();
        // 200 files × 1 MiB + one 16 MiB file = ~216 MiB → multiple 128 MiB groups.
        for i in 0..200u32 {
            contents.insert(i, alloc::vec![(i & 0xff) as u8; 1024 * 1024]);
            root_children.insert(
                alloc::format!("f{i:03}"),
                Node::File {
                    meta: Meta {
                        mode: 0o644,
                        uid: 0,
                        gid: 0,
                        mtime: 0,
                    },
                    content: i,
                },
            );
        }
        contents.insert(1000, alloc::vec![0x5au8; 16 * 1024 * 1024]);
        root_children.insert(
            "big".to_string(),
            Node::File {
                meta: Meta {
                    mode: 0o644,
                    uid: 0,
                    gid: 0,
                    mtime: 0,
                },
                content: 1000,
            },
        );
        let tree = Tree {
            root: Node::Dir {
                meta: Meta {
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    mtime: 0,
                },
                children: root_children,
            },
            contents,
        };
        let img = ext4::write_image(&tree).expect("ext4 write");
        std::fs::write("/tmp/holospaces-asm-big.img", &img).unwrap();
        std::eprintln!(
            "wrote /tmp/holospaces-asm-big.img ({} bytes, {} blocks)",
            img.len(),
            img.len() / 4096
        );
    }
}
