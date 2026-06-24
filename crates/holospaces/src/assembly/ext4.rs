//! A real **ext4** filesystem writer — serializes an overlaid filesystem
//! [`super::Tree`] into a block image the Linux kernel mounts and
//! `e2fsck` accepts clean.
//!
//! This is the *Rootfs Assembly* serializer (arc42 ch.5, the Layer Assembler):
//! the projection from the κ-addressed file tree to the κ-disk's blocks
//! (Law L4 — holospaces produces the filesystem itself; it does not shell out
//! to `mke2fs`). The on-disk format is the authority — the ext4 layout as
//! implemented by the Linux kernel and `e2fsprogs` (the V&V oracles). The
//! feature set is a clean ext4-without-journal: `filetype` + `extent`
//! (incompat) and `sparse_super` + `large_file` (ro_compat); 4 KiB blocks;
//! 256-byte inodes; classic (non-`flex_bg`) per-group metadata; no checksums.
//!
//! It is general: arbitrarily many block groups, files mapped by real extent
//! trees (inline ≤4, else a depth-1 index), directories as linear `filetype`
//! entries, fast symlinks inline, device/fifo/socket specials, and hard links
//! sharing one inode. No fixed file-count, file-size, or image-size cap.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{Meta, Node, Tree};

// ── POSIX mode type bits (also used by the overlay for specials) ───────────
/// `S_IFSOCK` — a socket inode's mode type bits.
pub const S_IFSOCK: u16 = 0xC000;
/// `S_IFLNK` — a symbolic link's mode type bits.
pub const S_IFLNK: u16 = 0xA000;
/// `S_IFREG` — a regular file's mode type bits.
pub const S_IFREG: u16 = 0x8000;
/// `S_IFBLK` — a block-device special's mode type bits.
pub const S_IFBLK: u16 = 0x6000;
/// `S_IFDIR` — a directory's mode type bits.
pub const S_IFDIR: u16 = 0x4000;
/// `S_IFCHR` — a character-device special's mode type bits.
pub const S_IFCHR: u16 = 0x2000;
/// `S_IFIFO` — a FIFO (named pipe) special's mode type bits.
pub const S_IFIFO: u16 = 0x1000;

const BLOCK_SIZE: u64 = 4096;
/// The ext4 block size in bytes (4 KiB), exposed for a streaming consumer that
/// sizes its block device to whole ext4 blocks ([`stream_image_with_free`]).
pub const BLOCK_SIZE_BYTES: u32 = BLOCK_SIZE as u32;
const LOG_BLOCK_SIZE: u32 = 2; // 1024 << 2 = 4096
const BLOCKS_PER_GROUP: u64 = BLOCK_SIZE * 8; // 32768 (one block bitmap covers it)
const INODE_SIZE: u64 = 256;
const INODES_PER_BLOCK: u64 = BLOCK_SIZE / INODE_SIZE; // 16
const EXTRA_ISIZE: u16 = 32;
const FIRST_INO: u32 = 11; // 1..10 reserved; 11 = lost+found; 12.. = content
const ROOT_INO: u32 = 2;
const LF_INO: u32 = 11;
const SECTORS_PER_BLOCK: u64 = BLOCK_SIZE / 512; // 8

const EXT4_MAGIC: u16 = 0xEF53;
const EXTENTS_FL: u32 = 0x0008_0000;
const EH_MAGIC: u16 = 0xF30A;
const FEATURE_INCOMPAT: u32 = 0x0002 /*filetype*/ | 0x0040 /*extent*/;
const FEATURE_RO_COMPAT: u32 = 0x0001 /*sparse_super*/ | 0x0002 /*large_file*/;

// Directory entry file types.
const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_CHR: u8 = 3;
const FT_BLK: u8 = 4;
const FT_FIFO: u8 = 5;
const FT_SOCK: u8 = 6;
const FT_SYMLINK: u8 = 7;

/// A reproducible volume UUID (the κ of an assembled rootfs must be a function
/// of its content alone, Law L1 — so the UUID is fixed, not random).
const UUID: [u8; 16] = [
    0x0d, 0x15, 0xea, 0x5e, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x14,
];

/// What can go wrong serializing the tree to ext4.
#[derive(Debug, PartialEq, Eq)]
pub enum Ext4Error {
    /// The tree's root was not a directory.
    RootNotDir,
    /// A single file fragmented into more extents than a depth-1 tree can hold
    /// (≈1360 — only reachable by a multi-hundred-TB maximally-fragmented file).
    /// Reported loudly rather than silently truncated.
    TooManyExtents(u32),
    /// A directory-entry name exceeds ext4's 255-byte limit. Reported loudly
    /// rather than silently truncated to a colliding name (the field is one
    /// byte; a longer name cannot be represented and must not be corrupted).
    NameTooLong(String),
}

impl core::fmt::Display for Ext4Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Ext4Error::RootNotDir => write!(f, "filesystem root is not a directory"),
            Ext4Error::TooManyExtents(ino) => {
                write!(f, "inode {ino} needs a deeper extent tree than implemented")
            }
            Ext4Error::NameTooLong(name) => {
                write!(
                    f,
                    "directory entry name exceeds ext4's 255-byte limit ({} bytes): {name}",
                    name.len()
                )
            }
        }
    }
}

/// ext4's directory-entry name length field is one byte: a component longer than
/// 255 bytes cannot be represented and must be reported, never truncated.
const MAX_NAME_LEN: usize = 255;

/// Reject any directory-entry name longer than [`MAX_NAME_LEN`] anywhere in the
/// tree (ext4's per-component limit) before serialization — so a too-long name
/// is an explicit [`Ext4Error::NameTooLong`], never a silent one-byte truncation
/// that would collide two distinct files onto one entry.
fn check_names(node: &Node) -> Result<(), Ext4Error> {
    if let Node::Dir { children, .. } = node {
        for (name, child) in children {
            if name.len() > MAX_NAME_LEN {
                return Err(Ext4Error::NameTooLong(name.clone()));
            }
            check_names(child)?;
        }
    }
    Ok(())
}

// ── The flattened inode model ──────────────────────────────────────────────

enum IBody {
    /// A directory and its entries (name, child inode, file type), in order.
    Dir(Vec<(String, u32, u8)>),
    /// A regular file's bytes (owned once; hard links reference the same inode).
    File(Vec<u8>),
    /// A symlink target.
    Symlink(Vec<u8>),
    /// A device/fifo/socket special (`rdev` is 0 for fifo/socket).
    Special(u32),
}

struct Inode {
    ino: u32,
    mode: u16, // full mode including the S_IF* type
    uid: u32,
    gid: u32,
    mtime: u32,
    links: u16,
    body: IBody,
    // filled during allocation:
    extents: Vec<(u32, u64, u16)>, // (logical_block, physical_block, len)
    blocks_512: u64,               // i_blocks (512-byte sectors)
}

/// Serialize `tree` into a complete ext4 image.
pub fn write_image(tree: &Tree) -> Result<Vec<u8>, Ext4Error> {
    write_image_with_free(tree, 0, 0)
}

/// The geometry of a serialized ext4 image, exposed so a streaming consumer can
/// pre-size its block device without materializing the image first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageGeometry {
    /// The block size in bytes (always [`BLOCK_SIZE_BYTES`] = 4 KiB).
    pub block_size: u64,
    /// The total number of 4 KiB blocks in the image (image size / block size).
    pub total_blocks: u64,
}

impl ImageGeometry {
    /// The image's total size in bytes.
    #[must_use]
    pub fn image_len(&self) -> u64 {
        self.block_size * self.total_blocks
    }
}

/// Serialize `tree` into an ext4 image but emit it **one 4 KiB block at a time**
/// instead of returning a dense [`Vec`]. `emit(block_index, &block_bytes)` is
/// called once for every block whose content is **not all-zero**, in strictly
/// ascending `block_index` order; all-zero blocks are *never materialized* and
/// never emitted (the disk that consumes this leaves them sparse). The returned
/// [`ImageGeometry`] reports the full (dense) image dimensions so the consumer
/// knows how many sectors the device spans.
///
/// This is the *sparse, streaming* projection from the κ-tree to the block
/// device (Law L4, "the KappaStore IS the memory"): peak working memory is bounded
/// by the **non-zero** content (the materialized blocks), independent of the
/// image's total size — a multi-GiB disk whose free space is sparse never costs
/// multi-GiB of RAM to assemble.
///
/// Block-for-block identical to [`write_image_with_free`]: concatenating the
/// emitted blocks at their indices over a zero-filled `image_len()` buffer
/// reproduces the dense image byte-for-byte.
pub fn stream_image_with_free(
    tree: &Tree,
    min_inodes: u32,
    min_blocks: u64,
    mut emit: impl FnMut(u64, &[u8]),
) -> Result<ImageGeometry, Ext4Error> {
    let (geom, blocks) = build_sparse_image(tree, min_inodes, min_blocks)?;
    // Emit the materialized (non-zero) blocks in ascending order. The BTreeMap
    // already orders by block index; a block in the map is non-zero by
    // construction (we only insert non-zero blocks).
    for (idx, bytes) in &blocks {
        emit(*idx, bytes);
    }
    Ok(ImageGeometry {
        block_size: BLOCK_SIZE,
        total_blocks: geom.total_blocks,
    })
}

/// Serialize `tree` to its sparse non-zero blocks (keyed by block index) plus the
/// full [`ImageGeometry`] — one assembly pass, the building block both the dense
/// and streaming consumers share. The returned map holds only materialized
/// (non-zero) blocks, so its footprint tracks content, not the declared size; a
/// consumer streams it into a block device and drops it.
pub fn sparse_blocks_with_free(
    tree: &Tree,
    min_inodes: u32,
    min_blocks: u64,
) -> Result<(ImageGeometry, BTreeMap<u64, Vec<u8>>), Ext4Error> {
    let (geom, blocks) = build_sparse_image(tree, min_inodes, min_blocks)?;
    Ok((
        ImageGeometry {
            block_size: BLOCK_SIZE,
            total_blocks: geom.total_blocks,
        },
        blocks,
    ))
}

/// Serialize `tree` into an ext4 image sized to *at least* `min_inodes` inodes and
/// `min_blocks` 4 KiB blocks — so the filesystem has free capacity for the guest
/// to use at runtime. A minimally-sized image (`write_image`) fits the content
/// exactly with no room to create a single file; a *bootable* devcontainer rootfs
/// is sized to a requested disk size (its init mounts pseudo-filesystems, installs
/// BusyBox applet symlinks, writes to `/tmp`, and the user works in it), driven by
/// the caller's configuration rather than a hidden constant
/// (see [`super::assemble_ext4_bootable`]).
pub fn write_image_with_free(
    tree: &Tree,
    min_inodes: u32,
    min_blocks: u64,
) -> Result<Vec<u8>, Ext4Error> {
    // The dense image is the sparse projection materialized over a zero buffer —
    // exactly the bytes a `stream_image_with_free` consumer would reconstruct, so
    // the two paths are byte-identical by construction (Law L1: one canonical
    // serialization, two consumers).
    let (geom, blocks) = build_sparse_image(tree, min_inodes, min_blocks)?;
    let mut img = vec![0u8; (geom.total_blocks * BLOCK_SIZE) as usize];
    for (idx, bytes) in &blocks {
        let off = (*idx * BLOCK_SIZE) as usize;
        img[off..off + bytes.len()].copy_from_slice(bytes);
    }
    Ok(img)
}

/// Serialize `tree` into the **sparse** set of an ext4 image's non-zero 4 KiB
/// blocks (keyed by block index), together with the full geometry. This is the
/// single canonical ext4 serializer; both the dense [`write_image_with_free`]
/// and the streaming [`stream_image_with_free`] are thin consumers of it, so they
/// are byte-identical by construction.
///
/// Only blocks with at least one non-zero byte are inserted into the returned
/// map — so its memory footprint tracks the image's *content*, not its declared
/// size. A multi-GiB disk whose free space is sparse yields a small map.
fn build_sparse_image(
    tree: &Tree,
    min_inodes: u32,
    min_blocks: u64,
) -> Result<(Geometry, BTreeMap<u64, Vec<u8>>), Ext4Error> {
    let Node::Dir { .. } = &tree.root else {
        return Err(Ext4Error::RootNotDir);
    };
    check_names(&tree.root)?;

    // Pass 1 — assign inode numbers and build the inode list (root=2, lost+found
    // =11, the rest 12..). Hard links (files sharing a content id) share one
    // inode. Directory link counts are 2 + (number of child directories).
    let mut b = Builder {
        inodes: Vec::new(),
        contents: &tree.contents,
        content_to_ino: BTreeMap::new(),
        next_ino: FIRST_INO + 1, // 12
    };
    // lost+found (inode 11), an empty directory under root.
    let lf_meta = Meta {
        mode: 0o700,
        uid: 0,
        gid: 0,
        mtime: 0,
    };
    // Root is inode 2; build it (its children + the injected lost+found).
    let Node::Dir { meta, children } = &tree.root else {
        unreachable!()
    };
    let mut root_entries: Vec<(String, u32, u8)> = Vec::new();
    root_entries.push((".".into(), ROOT_INO, FT_DIR));
    root_entries.push(("..".into(), ROOT_INO, FT_DIR));
    root_entries.push(("lost+found".into(), LF_INO, FT_DIR));
    let mut root_child_dirs = 1u16; // lost+found
    for (name, node) in children {
        let (ino, is_dir) = b.assign(node, ROOT_INO)?;
        let ft = file_type(node);
        if is_dir {
            root_child_dirs += 1;
        }
        root_entries.push((name.clone(), ino, ft));
    }
    // Emit lost+found and root inodes.
    b.inodes.push(Inode {
        ino: LF_INO,
        mode: S_IFDIR | (lf_meta.mode & 0o7777),
        uid: lf_meta.uid,
        gid: lf_meta.gid,
        mtime: lf_meta.mtime,
        links: 2,
        body: IBody::Dir(vec![
            (".".into(), LF_INO, FT_DIR),
            ("..".into(), ROOT_INO, FT_DIR),
        ]),
        extents: Vec::new(),
        blocks_512: 0,
    });
    b.inodes.push(Inode {
        ino: ROOT_INO,
        mode: S_IFDIR | (meta.mode & 0o7777),
        uid: meta.uid,
        gid: meta.gid,
        mtime: meta.mtime,
        links: 2 + root_child_dirs,
        body: IBody::Dir(root_entries),
        extents: Vec::new(),
        blocks_512: 0,
    });

    let mut inodes = b.inodes;
    // Sort by inode number so the inode table is written in order.
    inodes.sort_by_key(|i| i.ino);
    let max_ino = inodes.iter().map(|i| i.ino).max().unwrap_or(FIRST_INO);

    // Pass 2 — render each inode's data (directory blocks now that child inode
    // numbers are known) and count the data blocks each needs.
    let mut rendered: Vec<RenderedBody> = Vec::with_capacity(inodes.len());
    let mut data_blocks_needed: u64 = 0;
    for inode in &inodes {
        let rb = render_body(inode);
        data_blocks_needed += rb.blocks;
        rendered.push(rb);
    }
    // Headroom for extent-tree leaf blocks (only files that fragment past 4
    // extents use any; marked free if unused).
    let tree_slack = (inodes.len() as u64 / 8).max(8);

    // Pass 3 — size the filesystem: choose the inode count and block count, and
    // the block-group geometry, converging on a fixed point. Add the requested
    // spare capacity (extra inodes + blocks left free for the guest to use).
    let geom = Geometry::size(
        max_ino.max(min_inodes),
        (data_blocks_needed + tree_slack).max(min_blocks),
    );

    // Pass 4 — allocate data blocks (per group's data region, as contiguous
    // extents) to each inode, then allocate any extent-tree blocks.
    let mut alloc = Allocator::new(&geom);
    let mut block_use: Vec<bool> = vec![false; geom.total_blocks as usize];
    geom.mark_metadata(&mut block_use);
    let mut extent_tree_blocks: BTreeMap<u32, Vec<(u64, Vec<u8>)>> = BTreeMap::new();

    for (inode, rb) in inodes.iter_mut().zip(rendered.iter()) {
        if rb.blocks == 0 {
            continue;
        }
        let runs = alloc.alloc(rb.blocks, &mut block_use);
        // runs → logical extents
        let mut logical = 0u32;
        for (phys, count) in &runs {
            inode.extents.push((logical, *phys, *count as u16));
            logical += *count as u32;
        }
        inode.blocks_512 = rb.blocks * SECTORS_PER_BLOCK;
        // If the file needs an extent tree (>4 extents), allocate leaf blocks.
        if inode.extents.len() > 4 {
            let leaves = build_extent_tree(inode.ino, &inode.extents)?;
            let mut placed = Vec::new();
            for leaf in leaves {
                let blk = alloc.alloc_one(&mut block_use);
                inode.blocks_512 += SECTORS_PER_BLOCK;
                placed.push((blk, leaf));
            }
            extent_tree_blocks.insert(inode.ino, placed);
        }
    }

    // Pass 5 — emit the image as a sparse map of non-zero 4 KiB blocks. A
    // `Sink` accumulates writes at byte offsets and materializes a block lazily
    // the first time it is touched, so blocks never written (file holes, the
    // disk's free data region) are never allocated — peak memory tracks the
    // image's *content*, not its declared size.
    let mut sink = Sink::new();

    // 5a: data blocks (file contents, directory blocks, symlink spill).
    for (inode, rb) in inodes.iter().zip(rendered.iter()) {
        if rb.blocks == 0 {
            continue;
        }
        sink_extents_data(&mut sink, &inode.extents, &rb.bytes);
    }
    // 5b: extent-tree leaf blocks.
    for placed in extent_tree_blocks.values() {
        for (blk, bytes) in placed {
            sink.write_at(blk * BLOCK_SIZE, bytes);
        }
    }

    // 5c: inode table.
    let mut inode_iter = inodes.iter().peekable();
    for ino in 1..=geom.inodes_count {
        let off = geom.inode_offset(ino) as u64;
        if let Some(inode) = inode_iter.peek() {
            if inode.ino as u64 == ino {
                let bytes = encode_inode(inode, extent_tree_blocks.get(&inode.ino));
                sink.write_at(off, &bytes);
                inode_iter.next();
                continue;
            }
        }
        // reserved/unused inode — left zero (links_count 0 = free).
    }

    // 5d: bitmaps + group descriptors + superblocks.
    geom.sink_bitmaps_descriptors_superblocks(&mut sink, &block_use, &inodes);

    // Drop blocks that ended up all-zero (e.g. a touched-but-only-zeros region),
    // so the sparse contract holds: a block in the map has a non-zero byte.
    let mut blocks = sink.into_blocks();
    blocks.retain(|_, b| b.iter().any(|&x| x != 0));
    Ok((geom, blocks))
}

/// A sparse byte sink: writes land at absolute byte offsets; only the 4 KiB
/// blocks actually touched are materialized (zero-filled on first touch). This is
/// what bounds the assembler's peak memory to the image's content rather than its
/// size — the dense buffer is never allocated.
struct Sink {
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl Sink {
    fn new() -> Sink {
        Sink {
            blocks: BTreeMap::new(),
        }
    }

    /// Materialize (zero-filling on first touch) the block at `idx`.
    fn block_mut(&mut self, idx: u64) -> &mut Vec<u8> {
        self.blocks
            .entry(idx)
            .or_insert_with(|| vec![0u8; BLOCK_SIZE as usize])
    }

    /// Write `data` starting at absolute byte offset `off`, spanning blocks as
    /// needed. Equivalent to `img[off..off+len].copy_from_slice(data)` over the
    /// dense image.
    fn write_at(&mut self, off: u64, data: &[u8]) {
        let mut pos = off;
        let mut rest = data;
        while !rest.is_empty() {
            let idx = pos / BLOCK_SIZE;
            let within = (pos % BLOCK_SIZE) as usize;
            let n = rest.len().min(BLOCK_SIZE as usize - within);
            let block = self.block_mut(idx);
            block[within..within + n].copy_from_slice(&rest[..n]);
            pos += n as u64;
            rest = &rest[n..];
        }
    }

    fn into_blocks(self) -> BTreeMap<u64, Vec<u8>> {
        self.blocks
    }
}

// ── Pass 1 helper: inode numbering ─────────────────────────────────────────

struct Builder<'a> {
    inodes: Vec<Inode>,
    contents: &'a BTreeMap<u32, Vec<u8>>,
    content_to_ino: BTreeMap<u32, u32>,
    next_ino: u32,
}

impl Builder<'_> {
    /// Assign an inode to `node` (whose parent is `parent_ino`); returns
    /// (inode number, is_directory). Recurses into directories.
    fn assign(&mut self, node: &Node, parent_ino: u32) -> Result<(u32, bool), Ext4Error> {
        match node {
            Node::Dir { meta, children } => {
                let ino = self.fresh();
                let mut entries =
                    vec![(".".into(), ino, FT_DIR), ("..".into(), parent_ino, FT_DIR)];
                let mut child_dirs = 0u16;
                for (name, child) in children {
                    let (cino, is_dir) = self.assign(child, ino)?;
                    if is_dir {
                        child_dirs += 1;
                    }
                    entries.push((name.clone(), cino, file_type(child)));
                }
                self.inodes.push(Inode {
                    ino,
                    mode: S_IFDIR | (meta.mode & 0o7777),
                    uid: meta.uid,
                    gid: meta.gid,
                    mtime: meta.mtime,
                    links: 2 + child_dirs,
                    body: IBody::Dir(entries),
                    extents: Vec::new(),
                    blocks_512: 0,
                });
                Ok((ino, true))
            }
            Node::File { meta, content } => {
                // Hard link: a second file with the same content id reuses the
                // first's inode (shared inode, bumped link count).
                if let Some(&ino) = self.content_to_ino.get(content) {
                    for i in &mut self.inodes {
                        if i.ino == ino {
                            i.links += 1;
                            break;
                        }
                    }
                    return Ok((ino, false));
                }
                let ino = self.fresh();
                self.content_to_ino.insert(*content, ino);
                let bytes = self.contents.get(content).cloned().unwrap_or_default();
                self.inodes.push(Inode {
                    ino,
                    mode: S_IFREG | (meta.mode & 0o7777),
                    uid: meta.uid,
                    gid: meta.gid,
                    mtime: meta.mtime,
                    links: 1,
                    body: IBody::File(bytes),
                    extents: Vec::new(),
                    blocks_512: 0,
                });
                Ok((ino, false))
            }
            Node::Symlink { meta, target } => {
                let ino = self.fresh();
                self.inodes.push(Inode {
                    ino,
                    mode: S_IFLNK | (meta.mode & 0o7777),
                    uid: meta.uid,
                    gid: meta.gid,
                    mtime: meta.mtime,
                    links: 1,
                    body: IBody::Symlink(target.clone()),
                    extents: Vec::new(),
                    blocks_512: 0,
                });
                Ok((ino, false))
            }
            Node::Special { meta, ifmt, rdev } => {
                let ino = self.fresh();
                self.inodes.push(Inode {
                    ino,
                    mode: ifmt | (meta.mode & 0o7777),
                    uid: meta.uid,
                    gid: meta.gid,
                    mtime: meta.mtime,
                    links: 1,
                    body: IBody::Special(*rdev),
                    extents: Vec::new(),
                    blocks_512: 0,
                });
                Ok((ino, false))
            }
        }
    }

    fn fresh(&mut self) -> u32 {
        let i = self.next_ino;
        self.next_ino += 1;
        i
    }
}

fn file_type(node: &Node) -> u8 {
    match node {
        Node::Dir { .. } => FT_DIR,
        Node::File { .. } => FT_REG,
        Node::Symlink { .. } => FT_SYMLINK,
        Node::Special { ifmt, .. } => match *ifmt {
            S_IFCHR => FT_CHR,
            S_IFBLK => FT_BLK,
            S_IFIFO => FT_FIFO,
            S_IFSOCK => FT_SOCK,
            _ => FT_REG,
        },
    }
}

// ── Pass 2: render each inode's data bytes + block count ───────────────────

struct RenderedBody {
    bytes: Vec<u8>,
    blocks: u64,
}

fn render_body(inode: &Inode) -> RenderedBody {
    match &inode.body {
        IBody::Dir(entries) => {
            let bytes = build_dir_blocks(entries);
            let blocks = bytes.len() as u64 / BLOCK_SIZE;
            RenderedBody { bytes, blocks }
        }
        IBody::File(data) => {
            let blocks = (data.len() as u64).div_ceil(BLOCK_SIZE);
            RenderedBody {
                bytes: data.clone(),
                blocks,
            }
        }
        IBody::Symlink(target) => {
            if target.len() <= 60 {
                RenderedBody {
                    bytes: Vec::new(),
                    blocks: 0,
                } // fast symlink (inline in i_block)
            } else {
                RenderedBody {
                    bytes: target.clone(),
                    blocks: 1,
                }
            }
        }
        IBody::Special(_) => RenderedBody {
            bytes: Vec::new(),
            blocks: 0,
        },
    }
}

/// Build a directory's data as linear `filetype` entries, one or more blocks.
fn build_dir_blocks(entries: &[(String, u32, u8)]) -> Vec<u8> {
    let mut blocks: Vec<u8> = Vec::new();
    let mut cur = vec![0u8; BLOCK_SIZE as usize];
    let mut pos = 0usize;
    let mut last_entry_off = 0usize;

    for (name, ino, ft) in entries {
        let nlen = name.len();
        let need = 8 + nlen.div_ceil(4) * 4;
        if pos + need > BLOCK_SIZE as usize {
            // Extend the previous entry to fill the rest of this block, flush.
            let rec_len = BLOCK_SIZE as usize - last_entry_off;
            cur[last_entry_off + 4..last_entry_off + 6]
                .copy_from_slice(&(rec_len as u16).to_le_bytes());
            blocks.extend_from_slice(&cur);
            cur = vec![0u8; BLOCK_SIZE as usize];
            pos = 0;
        }
        last_entry_off = pos;
        cur[pos..pos + 4].copy_from_slice(&ino.to_le_bytes());
        cur[pos + 4..pos + 6].copy_from_slice(&(need as u16).to_le_bytes());
        cur[pos + 6] = nlen as u8;
        cur[pos + 7] = *ft;
        cur[pos + 8..pos + 8 + nlen].copy_from_slice(name.as_bytes());
        pos += need;
    }
    // Last entry of the final block extends to the block end.
    let rec_len = BLOCK_SIZE as usize - last_entry_off;
    cur[last_entry_off + 4..last_entry_off + 6].copy_from_slice(&(rec_len as u16).to_le_bytes());
    blocks.extend_from_slice(&cur);
    blocks
}

// ── Pass 3: geometry sizing ────────────────────────────────────────────────

struct Geometry {
    total_blocks: u64,
    groups: u64,
    inodes_per_group: u64,
    inodes_count: u64,
    inode_table_blocks: u64, // per group
    gdt_blocks: u64,
}

impl Geometry {
    fn size(max_ino: u32, data_blocks: u64) -> Geometry {
        let inodes_needed = (max_ino as u64).max(FIRST_INO as u64 + 1);
        let mut groups = 1u64;
        loop {
            let ipg =
                round_up(div_ceil(inodes_needed, groups), INODES_PER_BLOCK).max(INODES_PER_BLOCK);
            let itb = ipg / INODES_PER_BLOCK;
            let gdt = div_ceil(groups * 32, BLOCK_SIZE);
            let mut meta = 0u64;
            for g in 0..groups {
                if has_super(g) {
                    meta += 1 + gdt;
                }
                meta += 2 + itb; // block bitmap + inode bitmap + inode table
            }
            let total = meta + data_blocks; // first_data_block = 0 for 4K blocks
            let need = div_ceil(total, BLOCKS_PER_GROUP);
            if need <= groups {
                let inodes_count = ipg * groups;
                // Round a MULTI-group image up to whole block groups so the final
                // group always spans its full metadata footprint (super/GDT/block+
                // inode bitmaps/inode table). A short trailing group — `total` just
                // past a group boundary — would otherwise place that metadata past
                // the image end (an out-of-bounds write for any large, multi-group
                // build-capable disk). The extra blocks are free, sparse data space.
                // A single-group image keeps its exact size: its one (partial) group
                // already contains its metadata.
                let total_blocks = if groups > 1 {
                    groups * BLOCKS_PER_GROUP
                } else {
                    total
                };
                return Geometry {
                    total_blocks,
                    groups,
                    inodes_per_group: ipg,
                    inodes_count,
                    inode_table_blocks: itb,
                    gdt_blocks: gdt,
                };
            }
            groups = need;
        }
    }

    /// The first block of group `g`'s data region (after its metadata).
    fn data_start(&self, g: u64) -> u64 {
        let base = g * BLOCKS_PER_GROUP;
        let mut off = 0;
        if has_super(g) {
            off += 1 + self.gdt_blocks;
        }
        off += 2 + self.inode_table_blocks;
        base + off
    }

    /// The number of blocks group `g` actually spans (last group may be short).
    fn group_blocks(&self, g: u64) -> u64 {
        (self.total_blocks - g * BLOCKS_PER_GROUP).min(BLOCKS_PER_GROUP)
    }

    fn block_bitmap_block(&self, g: u64) -> u64 {
        let base = g * BLOCKS_PER_GROUP;
        base + if has_super(g) { 1 + self.gdt_blocks } else { 0 }
    }
    fn inode_bitmap_block(&self, g: u64) -> u64 {
        self.block_bitmap_block(g) + 1
    }
    fn inode_table_block(&self, g: u64) -> u64 {
        self.block_bitmap_block(g) + 2
    }

    /// Byte offset of inode number `ino` within the image.
    fn inode_offset(&self, ino: u64) -> usize {
        let idx = ino - 1; // inodes are 1-based
        let g = idx / self.inodes_per_group;
        let within = idx % self.inodes_per_group;
        let table = self.inode_table_block(g);
        (table * BLOCK_SIZE + within * INODE_SIZE) as usize
    }

    /// Mark all metadata blocks (superblocks, GDTs, bitmaps, inode tables, and
    /// any padding past the filesystem end) as used.
    fn mark_metadata(&self, used: &mut [bool]) {
        for g in 0..self.groups {
            let base = g * BLOCKS_PER_GROUP;
            let mut off = 0u64;
            if has_super(g) {
                for k in 0..(1 + self.gdt_blocks) {
                    used[(base + k) as usize] = true;
                }
                off += 1 + self.gdt_blocks;
            }
            // block bitmap + inode bitmap + inode table
            for k in 0..(2 + self.inode_table_blocks) {
                used[(base + off + k) as usize] = true;
            }
        }
    }
}

// ── Pass 4: data-block allocator ───────────────────────────────────────────

struct Allocator {
    segments: Vec<(u64, u64)>, // (start_block, count) per group data region
    seg: usize,
    off: u64,
}

impl Allocator {
    fn new(geom: &Geometry) -> Allocator {
        let mut segments = Vec::new();
        for g in 0..geom.groups {
            let start = geom.data_start(g);
            let end = g * BLOCKS_PER_GROUP + geom.group_blocks(g);
            if end > start {
                segments.push((start, end - start));
            }
        }
        Allocator {
            segments,
            seg: 0,
            off: 0,
        }
    }

    /// Allocate `n` blocks as contiguous runs (extents), filling group data
    /// regions in order; runs never cross a group boundary, and are capped at
    /// 32768 blocks (the ext4 extent length limit).
    fn alloc(&mut self, mut n: u64, used: &mut [bool]) -> Vec<(u64, u64)> {
        let mut runs = Vec::new();
        while n > 0 {
            if self.seg >= self.segments.len() {
                break; // sized with slack; should not happen
            }
            let (start, len) = self.segments[self.seg];
            let avail = len - self.off;
            if avail == 0 {
                self.seg += 1;
                self.off = 0;
                continue;
            }
            let take = n.min(avail).min(32768);
            let phys = start + self.off;
            for k in 0..take {
                used[(phys + k) as usize] = true;
            }
            runs.push((phys, take));
            self.off += take;
            n -= take;
        }
        runs
    }

    fn alloc_one(&mut self, used: &mut [bool]) -> u64 {
        let runs = self.alloc(1, used);
        runs[0].0
    }
}

/// Build the extent-tree leaf blocks for a file with >4 extents (depth-1 index
/// stored in i_block; this returns the leaf-block byte payloads, in order).
fn build_extent_tree(ino: u32, extents: &[(u32, u64, u16)]) -> Result<Vec<Vec<u8>>, Ext4Error> {
    let per_leaf = ((BLOCK_SIZE - 12) / 12) as usize; // header + entries
    let leaves: Vec<&[(u32, u64, u16)]> = extents.chunks(per_leaf).collect();
    if leaves.len() > 4 {
        return Err(Ext4Error::TooManyExtents(ino));
    }
    let mut out = Vec::new();
    for chunk in leaves {
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        put16(&mut buf, 0, EH_MAGIC);
        put16(&mut buf, 2, chunk.len() as u16);
        put16(&mut buf, 4, per_leaf as u16);
        put16(&mut buf, 6, 0); // depth 0 (leaf)
        for (i, (logical, phys, len)) in chunk.iter().enumerate() {
            let o = 12 + i * 12;
            put32(&mut buf, o, *logical);
            put16(&mut buf, o + 4, *len);
            put16(&mut buf, o + 6, (phys >> 32) as u16);
            put32(&mut buf, o + 8, *phys as u32);
        }
        out.push(buf);
    }
    Ok(out)
}

/// Write an inode's data bytes to their allocated extents via the sparse [`Sink`]
/// — the streaming analogue of writing into the dense image. A file's trailing
/// hole (an extent past the data's end) is simply not written, so it stays sparse.
fn sink_extents_data(sink: &mut Sink, extents: &[(u32, u64, u16)], bytes: &[u8]) {
    for (logical, phys, len) in extents {
        let src_off = (*logical as u64 * BLOCK_SIZE) as usize;
        let dst_off = *phys * BLOCK_SIZE;
        let n = ((*len as u64 * BLOCK_SIZE) as usize).min(bytes.len().saturating_sub(src_off));
        if n > 0 {
            sink.write_at(dst_off, &bytes[src_off..src_off + n]);
        }
    }
}

// ── Pass 5: encoders ───────────────────────────────────────────────────────

fn encode_inode(inode: &Inode, tree_blocks: Option<&Vec<(u64, Vec<u8>)>>) -> Vec<u8> {
    let mut b = vec![0u8; INODE_SIZE as usize];
    put16(&mut b, 0, inode.mode);
    put16(&mut b, 2, inode.uid as u16);
    let size = body_size(inode);
    put32(&mut b, 4, size as u32);
    put32(&mut b, 8, inode.mtime); // atime
    put32(&mut b, 12, inode.mtime); // ctime
    put32(&mut b, 16, inode.mtime); // mtime
    put32(&mut b, 20, 0); // dtime
    put16(&mut b, 24, inode.gid as u16);
    put16(&mut b, 26, inode.links);
    put32(&mut b, 28, inode.blocks_512 as u32);

    let is_fast_symlink =
        matches!(&inode.body, IBody::Symlink(t) if t.len() <= 60) && inode.extents.is_empty();
    let is_special = matches!(inode.body, IBody::Special(_));

    let mut flags = 0u32;
    if !is_fast_symlink && !is_special {
        flags |= EXTENTS_FL;
    }
    put32(&mut b, 32, flags);

    // i_block (60 bytes at offset 40)
    match &inode.body {
        IBody::Symlink(t) if is_fast_symlink => {
            b[40..40 + t.len()].copy_from_slice(t);
        }
        IBody::Special(rdev) => {
            put32(&mut b, 40, *rdev); // legacy device encoding in i_block[0]
        }
        _ => {
            encode_extent_root(&mut b[40..100], &inode.extents, tree_blocks);
        }
    }

    put32(&mut b, 100, 0); // i_generation
    put32(&mut b, 108, (size >> 32) as u32); // i_size_high
                                             // High 16 bits of uid/gid (l_i_uid_high / l_i_gid_high, linux2 osd2). Without
                                             // these, an image whose files are owned by uid/gid ≥ 65536 (rootless/namespaced
                                             // builds, e.g. 100000:100000) gets silently wrong ownership.
    put16(&mut b, 120, (inode.uid >> 16) as u16);
    put16(&mut b, 122, (inode.gid >> 16) as u16);
    put16(&mut b, 128, EXTRA_ISIZE);
    put32(&mut b, 144, inode.mtime); // i_crtime
    b
}

fn body_size(inode: &Inode) -> u64 {
    match &inode.body {
        IBody::Dir(_) => {
            // directory size = number of blocks * block size
            inode.extents.iter().map(|(_, _, l)| *l as u64).sum::<u64>() * BLOCK_SIZE
        }
        IBody::File(d) => d.len() as u64,
        IBody::Symlink(t) => t.len() as u64,
        IBody::Special(_) => 0,
    }
}

/// Write the extent header + entries into the inode's 60-byte i_block region.
/// Inline when ≤4 extents; otherwise a depth-1 index pointing at the leaf blocks.
fn encode_extent_root(
    ib: &mut [u8],
    extents: &[(u32, u64, u16)],
    tree_blocks: Option<&Vec<(u64, Vec<u8>)>>,
) {
    put16(ib, 0, EH_MAGIC);
    if let Some(leaves) = tree_blocks {
        // depth-1 index node
        put16(ib, 2, leaves.len() as u16);
        put16(ib, 4, 4); // max index entries inline
        put16(ib, 6, 1); // depth 1
        for (i, (blk, payload)) in leaves.iter().enumerate() {
            // ei_block = the first logical block this leaf covers
            let first_logical =
                u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
            let o = 12 + i * 12;
            put32(ib, o, first_logical);
            put32(ib, o + 4, *blk as u32); // ei_leaf_lo
            put16(ib, o + 8, (blk >> 32) as u16); // ei_leaf_hi
            put16(ib, o + 10, 0);
        }
    } else {
        put16(ib, 2, extents.len() as u16);
        put16(ib, 4, 4); // max inline
        put16(ib, 6, 0); // depth 0
        for (i, (logical, phys, len)) in extents.iter().enumerate() {
            let o = 12 + i * 12;
            put32(ib, o, *logical);
            put16(ib, o + 4, *len);
            put16(ib, o + 6, (phys >> 32) as u16);
            put32(ib, o + 8, *phys as u32);
        }
    }
}

impl Geometry {
    /// Emit the per-group bitmaps, the gathered group descriptors, and the
    /// (primary + backup) superblocks into the sparse [`Sink`] — the streaming
    /// analogue of writing them into the dense image, byte-for-byte identical.
    fn sink_bitmaps_descriptors_superblocks(
        &self,
        sink: &mut Sink,
        block_use: &[bool],
        inodes: &[Inode],
    ) {
        // Which inodes are in use, by number.
        let mut inode_used = vec![false; (self.inodes_count + 1) as usize];
        for i in 1..=10u64 {
            inode_used[i as usize] = true; // reserved inodes 1..10
        }
        inode_used[LF_INO as usize] = true;
        for ino in inodes {
            inode_used[ino.ino as usize] = true;
        }
        let dir_inos: alloc::collections::BTreeSet<u32> = inodes
            .iter()
            .filter(|i| matches!(i.body, IBody::Dir(_)))
            .map(|i| i.ino)
            .collect();

        // Group descriptors are gathered then written to every super-bearing group.
        let mut descriptors = vec![0u8; (self.groups * 32) as usize];
        let mut total_free_blocks = 0u64;
        let mut total_free_inodes = 0u64;

        for g in 0..self.groups {
            let base = g * BLOCKS_PER_GROUP;
            let gblocks = self.group_blocks(g);

            // Block bitmap for this group (one block, built locally then sunk).
            let bb = self.block_bitmap_block(g);
            let mut free_blocks = 0u64;
            let mut bb_bitmap = vec![0u8; BLOCK_SIZE as usize];
            for i in 0..BLOCKS_PER_GROUP {
                let blk = base + i;
                let used = if i >= gblocks {
                    true // past the group/filesystem end → mark used
                } else {
                    block_use[blk as usize]
                };
                if used {
                    bb_bitmap[(i / 8) as usize] |= 1 << (i % 8);
                } else {
                    free_blocks += 1;
                }
            }
            sink.write_at(bb * BLOCK_SIZE, &bb_bitmap);
            total_free_blocks += free_blocks;

            // Inode bitmap for this group (one block, built locally then sunk).
            let ib = self.inode_bitmap_block(g);
            let mut free_inodes = 0u64;
            let mut used_dirs = 0u64;
            let mut ib_bitmap = vec![0u8; BLOCK_SIZE as usize];
            for i in 0..self.inodes_per_group {
                let ino = g * self.inodes_per_group + i + 1;
                let used = ino <= self.inodes_count && inode_used[ino as usize];
                if used {
                    ib_bitmap[(i / 8) as usize] |= 1 << (i % 8);
                    if dir_inos.contains(&(ino as u32)) || ino == ROOT_INO as u64 {
                        used_dirs += 1;
                    }
                } else {
                    free_inodes += 1;
                }
            }
            // bits past inodes_per_group within the bitmap byte range: mark used
            for i in self.inodes_per_group..(BLOCK_SIZE * 8) {
                ib_bitmap[(i / 8) as usize] |= 1 << (i % 8);
            }
            sink.write_at(ib * BLOCK_SIZE, &ib_bitmap);
            total_free_inodes += free_inodes;

            // Group descriptor (32 bytes).
            let d = (g * 32) as usize;
            put32(&mut descriptors, d, bb as u32);
            put32(&mut descriptors, d + 4, ib as u32);
            put32(&mut descriptors, d + 8, self.inode_table_block(g) as u32);
            put16(&mut descriptors, d + 12, free_blocks as u16);
            put16(&mut descriptors, d + 14, free_inodes as u16);
            put16(&mut descriptors, d + 16, used_dirs as u16);
            put16(&mut descriptors, d + 18, 0); // bg_flags
            put16(&mut descriptors, d + 28, 0); // bg_itable_unused
        }

        // Superblock + GDT copies in every super-bearing group.
        let sb = self.superblock(total_free_blocks, total_free_inodes);
        for g in 0..self.groups {
            if !has_super(g) {
                continue;
            }
            let base = g * BLOCKS_PER_GROUP;
            // superblock: primary at byte 1024 of block 0; backups at the group's
            // first block start, with s_block_group_nr set.
            let mut this_sb = sb.clone();
            put16(&mut this_sb, 90, g as u16); // s_block_group_nr
            if g == 0 {
                sink.write_at(1024, &this_sb);
            } else {
                sink.write_at(base * BLOCK_SIZE, &this_sb);
            }
            // GDT after the superblock block.
            sink.write_at((base + 1) * BLOCK_SIZE, &descriptors);
        }
    }

    fn superblock(&self, free_blocks: u64, free_inodes: u64) -> Vec<u8> {
        let mut s = vec![0u8; 1024];
        put32(&mut s, 0, self.inodes_count as u32);
        put32(&mut s, 4, self.total_blocks as u32);
        put32(&mut s, 8, 0); // r_blocks
        put32(&mut s, 12, free_blocks as u32);
        put32(&mut s, 16, free_inodes as u32);
        put32(&mut s, 20, 0); // first_data_block (0 for 4K blocks)
        put32(&mut s, 24, LOG_BLOCK_SIZE);
        put32(&mut s, 28, LOG_BLOCK_SIZE); // log_cluster_size
        put32(&mut s, 32, BLOCKS_PER_GROUP as u32);
        put32(&mut s, 36, BLOCKS_PER_GROUP as u32); // clusters_per_group
        put32(&mut s, 40, self.inodes_per_group as u32);
        put32(&mut s, 44, 0); // mtime
        put32(&mut s, 48, 0); // wtime
        put16(&mut s, 52, 0); // mnt_count
        put16(&mut s, 54, 0xFFFF); // max_mnt_count = -1
        put16(&mut s, 56, EXT4_MAGIC);
        put16(&mut s, 58, 1); // state = clean
        put16(&mut s, 60, 1); // errors = continue
        put16(&mut s, 62, 0); // minor_rev
        put32(&mut s, 64, 0); // lastcheck
        put32(&mut s, 68, 0); // checkinterval
        put32(&mut s, 72, 0); // creator_os = Linux
        put32(&mut s, 76, 1); // rev_level = dynamic
        put16(&mut s, 80, 0); // def_resuid
        put16(&mut s, 82, 0); // def_resgid
        put32(&mut s, 84, FIRST_INO);
        put16(&mut s, 88, INODE_SIZE as u16);
        put16(&mut s, 90, 0); // block_group_nr (overwritten per copy)
        put32(&mut s, 92, 0); // feature_compat
        put32(&mut s, 96, FEATURE_INCOMPAT);
        put32(&mut s, 100, FEATURE_RO_COMPAT);
        s[104..120].copy_from_slice(&UUID);
        put16(&mut s, 348, EXTRA_ISIZE); // s_min_extra_isize
        put16(&mut s, 350, EXTRA_ISIZE); // s_want_extra_isize
        s
    }
}

// ── little-endian helpers ──────────────────────────────────────────────────

fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn div_ceil(a: u64, b: u64) -> u64 {
    a.div_ceil(b)
}
fn round_up(a: u64, m: u64) -> u64 {
    div_ceil(a, m) * m
}

/// Sparse-super: groups 0, 1, and powers of 3, 5, 7 carry a superblock backup.
fn has_super(g: u64) -> bool {
    if g == 0 || g == 1 {
        return true;
    }
    for base in [3u64, 5, 7] {
        let mut p = base;
        while p < g {
            p *= base;
        }
        if p == g {
            return true;
        }
    }
    false
}
