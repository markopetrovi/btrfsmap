//! Raw btrfs ioctl bindings.
//!
//! This mirrors what compsize(8) does: for every regular file, ask the kernel
//! for the inode's EXTENT_DATA items (`BTRFS_IOC_TREE_SEARCH_V2` with
//! `tree_id = 0`, which searches the subvolume tree the inode belongs to).
//! No libbtrfs is needed - the on-disk item layout is stable and defined in
//! linux/btrfs_tree.h.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// ioctl(2) glue
// ---------------------------------------------------------------------------

extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

// _IOC direction bits, x86_64/aarch64 Linux (asm-generic).
const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

const fn ioc(dir: u64, ty: u64, nr: u64, size: u64) -> u64 {
    (dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT)
}

// open(2) flags, x86_64 Linux.
pub const O_NOCTTY: i32 = 0o400;
pub const O_NONBLOCK: i32 = 0o4000;
pub const O_DIRECTORY: i32 = 0o200_000;
pub const O_NOFOLLOW: i32 = 0o400_000;

pub const BTRFS_IOCTL_MAGIC: u64 = 0x94;

// Item types (linux/btrfs_tree.h).
pub const BTRFS_EXTENT_DATA_KEY: u32 = 108;
pub const BTRFS_ROOT_ITEM_KEY: u32 = 132;

pub const BTRFS_FILE_EXTENT_INLINE: u8 = 0;
#[allow(dead_code)] // documented for completeness
pub const BTRFS_FILE_EXTENT_REG: u8 = 1;
pub const BTRFS_FILE_EXTENT_PREALLOC: u8 = 2;

/// Size of `struct btrfs_file_extent_item` (it is __packed__).
pub const FILE_EXTENT_ITEM_SIZE: usize = 53;
/// Fixed header of an inline extent item: everything before the inline payload.
const INLINE_HEADER_SIZE: usize = FILE_EXTENT_ITEM_SIZE - 4 * 8;

const SEARCH_KEY_SIZE: usize = 104; // struct btrfs_ioctl_search_key, padded
const SEARCH_ARGS_V2_SIZE: usize = SEARCH_KEY_SIZE + 8; // + buf_size
const SEARCH_HEADER_SIZE: usize = 32; // struct btrfs_ioctl_search_header, padded

const BTRFS_IOC_TREE_SEARCH_V2: u64 = ioc(IOC_WRITE | IOC_READ, BTRFS_IOCTL_MAGIC, 17, SEARCH_ARGS_V2_SIZE as u64);
const BTRFS_IOC_GET_SUBVOL_INFO: u64 = ioc(IOC_READ, BTRFS_IOCTL_MAGIC, 60, SUBVOL_INFO_SIZE as u64);
// _IOWR(0x94, 18, struct btrfs_ioctl_ino_lookup_args) = 0xd0009412: the size
// field is sizeof(struct ...) = 4096, the number is 18 and not 1.
const BTRFS_IOC_INO_LOOKUP: u64 = ioc(IOC_WRITE | IOC_READ, BTRFS_IOCTL_MAGIC, 18, INO_LOOKUP_SIZE as u64);

// ---------------------------------------------------------------------------
// GET_SUBVOL_INFO
// ---------------------------------------------------------------------------

const BTRFS_VOL_NAME_MAX: usize = 255;
const BTRFS_UUID_SIZE: usize = 16;
/// struct btrfs_ioctl_get_subvol_info_args, laid out by hand because we only
/// need a few fields and the struct is large (5 sectors of subvolume metadata).
const SUBVOL_INFO_SIZE: usize = 504;

const OFF_SUBVOL_TREEID: usize = 0;
const OFF_SUBVOL_NAME: usize = 8;
const OFF_SUBVOL_FLAGS: usize = 8 + (BTRFS_VOL_NAME_MAX + 1) + 8 + 8 + 8;
const OFF_SUBVOL_UUID: usize = OFF_SUBVOL_FLAGS + 8;
const OFF_SUBVOL_PARENT_UUID: usize = OFF_SUBVOL_UUID + BTRFS_UUID_SIZE;
const OFF_SUBVOL_RECEIVED_UUID: usize = OFF_SUBVOL_PARENT_UUID + BTRFS_UUID_SIZE;
// ctime, otime, stime, rtime each occupy 16 bytes (u64 sec + u32 nsec + pad).
const OFF_SUBVOL_CTIME: usize = OFF_SUBVOL_RECEIVED_UUID + BTRFS_UUID_SIZE + 4 * 8;
const OFF_SUBVOL_OTIME: usize = OFF_SUBVOL_CTIME + 16;

#[allow(dead_code)]
pub const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;

/// What the kernel tells us about a subvolume - enough to identify snapshots
/// structurally (via `parent_uuid`) instead of guessing from path names.
#[derive(Debug, Clone)]
pub struct SubvolInfo {
    pub treeid: u64,
    pub name: Vec<u8>,
    pub flags: u64,
    pub uuid: [u8; BTRFS_UUID_SIZE],
    pub parent_uuid: [u8; BTRFS_UUID_SIZE],
    #[allow(dead_code)]
    pub received_uuid: [u8; BTRFS_UUID_SIZE],
    /// when the subvolume was created - never changes, unlike ctime which any
    /// write bumps. This is the age a snapshot/parent comparison needs.
    pub otime_sec: u64,
}

pub fn get_subvol_info(fd: i32) -> io::Result<SubvolInfo> {
    let mut buf = [0u8; SUBVOL_INFO_SIZE];
    let rc = unsafe { ioctl(fd, BTRFS_IOC_GET_SUBVOL_INFO, buf.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let u64_at = |off: usize| u64::from_ne_bytes(buf[off..off + 8].try_into().unwrap());
    let name_end = buf[OFF_SUBVOL_NAME..OFF_SUBVOL_NAME + BTRFS_VOL_NAME_MAX + 1]
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(BTRFS_VOL_NAME_MAX);
    let mut uuid = [0u8; BTRFS_UUID_SIZE];
    uuid.copy_from_slice(&buf[OFF_SUBVOL_UUID..OFF_SUBVOL_UUID + BTRFS_UUID_SIZE]);
    let mut parent_uuid = [0u8; BTRFS_UUID_SIZE];
    parent_uuid.copy_from_slice(&buf[OFF_SUBVOL_PARENT_UUID..OFF_SUBVOL_PARENT_UUID + BTRFS_UUID_SIZE]);
    let mut received_uuid = [0u8; BTRFS_UUID_SIZE];
    received_uuid
        .copy_from_slice(&buf[OFF_SUBVOL_RECEIVED_UUID..OFF_SUBVOL_RECEIVED_UUID + BTRFS_UUID_SIZE]);
    Ok(SubvolInfo {
        treeid: u64_at(OFF_SUBVOL_TREEID),
        name: buf[OFF_SUBVOL_NAME..OFF_SUBVOL_NAME + name_end].to_vec(),
        flags: u64_at(OFF_SUBVOL_FLAGS),
        uuid,
        parent_uuid,
        received_uuid,
        otime_sec: u64_at(OFF_SUBVOL_OTIME),
    })
}

// ---------------------------------------------------------------------------
// FS_INFO: which filesystem is this, really
// ---------------------------------------------------------------------------

const FS_INFO_SIZE: usize = 1024;
const OFF_FS_INFO_FSID: usize = 16;
const OFF_FS_INFO_METADATA_UUID: usize = 64;
const BTRFS_IOC_FS_INFO: u64 = ioc(IOC_READ, BTRFS_IOCTL_MAGIC, 31, FS_INFO_SIZE as u64);

/// Identity of the filesystem an fd belongs to: the fsid stored in the
/// superblock, plus its metadata_uuid when that feature is in use. It is per
/// *filesystem* - every subvolume of one reports the same pair, and it is
/// persisted on disk - which is what makes it the test for "is this mount the
/// same filesystem, so are its extents comparable with ours".
///
/// Deliberately not statfs(2)'s f_fsid or stat's st_dev: btrfs gives every
/// mounted subvolume its own anonymous device (measured here: `/` is 37,
/// `/home` is 51, both subvolumes of one filesystem), so neither identifies the
/// volume.
pub fn fs_identity(fd: i32) -> io::Result<([u8; 16], [u8; 16])> {
    let mut buf = [0u8; FS_INFO_SIZE];
    let rc = unsafe { ioctl(fd, BTRFS_IOC_FS_INFO, buf.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fsid = [0u8; 16];
    fsid.copy_from_slice(&buf[OFF_FS_INFO_FSID..OFF_FS_INFO_FSID + 16]);
    let mut metadata = [0u8; 16];
    metadata
        .copy_from_slice(&buf[OFF_FS_INFO_METADATA_UUID..OFF_FS_INFO_METADATA_UUID + 16]);
    Ok((fsid, metadata))
}

// ---------------------------------------------------------------------------
// TREE_SEARCH_V2: per-inode extent list
// ---------------------------------------------------------------------------

/// One EXTENT_DATA item of a file, as compsize reads it.
#[derive(Debug, Clone, Copy)]
pub struct ExtentItem {
    /// BTRFS_FILE_EXTENT_{INLINE,REG,PREALLOC}
    pub kind: u8,
    /// Compression type, with 256 meaning prealloc (matches compsize's table).
    pub comp_type: u16,
    /// Physical address of the extent (0 = hole).
    pub disk_bytenr: u64,
    /// Physical size of the whole extent (shared extents included).
    pub disk_num_bytes: u64,
    /// Uncompressed size of the whole extent.
    pub ram_bytes: u64,
    /// How many bytes of the extent this file actually references.
    pub num_bytes: u64,
    /// Key offset: the file offset this item describes.
    #[allow(dead_code)]
    pub key_offset: u64,
}

pub const PREALLOC_TYPE: u16 = 256;

/// Which items to ask the kernel for: a tree, an item type, and the first
/// objectid (keys are ordered objectid, type, offset).
pub struct SearchSpec {
    pub tree_id: u64,
    /// inclusive range of item types; the same value for both restricts the
    /// search to one type, 0..u32::MAX enumerates a whole tree
    pub min_type: u32,
    pub max_type: u32,
    pub min_objectid: u64,
    pub max_objectid: u64,
}

/// Reusable per-thread search state: one kernel-side buffer plus the key we
/// mutate while walking a long extent list.
pub struct Searcher {
    buf: Vec<u8>,
}

impl Searcher {
    pub fn new(buf_size: usize) -> Self {
        let buf_size = buf_size.max(4096);
        Searcher {
            buf: vec![0u8; SEARCH_ARGS_V2_SIZE + buf_size],
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len() - SEARCH_ARGS_V2_SIZE
    }

    fn set_key(&mut self, spec: &SearchSpec, min_objectid: u64, min_type: u32, min_offset: u64) {
        const MAX: u64 = u64::MAX;
        let b = &mut self.buf;
        b[0..8].copy_from_slice(&spec.tree_id.to_ne_bytes());
        b[8..16].copy_from_slice(&min_objectid.to_ne_bytes()); // min_objectid
        b[16..24].copy_from_slice(&spec.max_objectid.to_ne_bytes()); // max_objectid
        b[24..32].copy_from_slice(&min_offset.to_ne_bytes()); // min_offset
        b[32..40].copy_from_slice(&MAX.to_ne_bytes()); // max_offset
        b[40..48].copy_from_slice(&0u64.to_ne_bytes()); // min_transid
        b[48..56].copy_from_slice(&MAX.to_ne_bytes()); // max_transid
        b[56..60].copy_from_slice(&min_type.to_ne_bytes()); // min_type
        b[60..64].copy_from_slice(&spec.max_type.to_ne_bytes()); // max_type
        b[64..68].copy_from_slice(&u32::MAX.to_ne_bytes()); // nr_items: as many as fit
        b[68..72].copy_from_slice(&0u32.to_ne_bytes()); // unused
        for off in [72, 80, 88, 96] {
            b[off..off + 8].copy_from_slice(&0u64.to_ne_bytes()); // unused1..4
        }
        let cap = (self.buf.len() - SEARCH_ARGS_V2_SIZE) as u64;
        self.buf[104..112].copy_from_slice(&cap.to_ne_bytes()); // buf_size
    }

    /// Call `f` for every EXTENT_DATA item of `ino`.
    pub fn for_each_extent<F>(&mut self, fd: i32, ino: u64, f: &mut F) -> io::Result<u64>
    where
        F: FnMut(&ExtentItem),
    {
        let spec = SearchSpec {
            tree_id: 0, // the subvolume the inode belongs to
            min_type: BTRFS_EXTENT_DATA_KEY,
            max_type: BTRFS_EXTENT_DATA_KEY,
            min_objectid: ino,
            max_objectid: ino,
        };
        self.for_each_item(fd, &spec, None, &mut |_obj, _ty, key_offset, payload| {
            if let Some(item) = parse_file_extent(payload, key_offset) {
                f(&item);
            }
        })
    }

    /// Call `f(objectid, offset, payload)` for every item of `spec.item_type`
    /// at or above `spec.min_objectid`.
    ///
    /// The kernel fills the buffer and reports how many items fit; we resume
    /// from the last *completely* parsed item. A trailing item whose payload
    /// did not fit is re-fetched whole rather than skipped, so no item can be
    /// lost or double counted.
    /// `cancel`, when set, is checked before each kernel search, so a caller
    /// that wants to stop does so within one in-flight `TREE_SEARCH_V2` - a few
    /// milliseconds - rather than at the end of the tree it is enumerating.
    /// The caller still decides what stopping means: this simply returns, and
    /// the caller sees the flag for itself.
    pub fn for_each_item<F>(
        &mut self,
        fd: i32,
        spec: &SearchSpec,
        cancel: Option<&AtomicBool>,
        f: &mut F,
    ) -> io::Result<u64>
    where
        F: FnMut(u64, u32, u64, &[u8]),
    {
        let cap = self.capacity();
        let mut n_seen = 0u64;
        let mut min_objectid = spec.min_objectid;
        let mut min_type = spec.min_type;
        let mut min_offset = 0u64;
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Ok(n_seen);
            }
            self.set_key(spec, min_objectid, min_type, min_offset);
            let rc = unsafe { ioctl(fd, BTRFS_IOC_TREE_SEARCH_V2, self.buf.as_mut_ptr()) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            let nr_items = u32::from_ne_bytes(self.buf[64..68].try_into().unwrap()) as usize;

            let items = &self.buf[SEARCH_ARGS_V2_SIZE..];
            let mut pos = 0usize;
            // (objectid, type, offset) of the last item we fully delivered
            let mut last_complete: Option<(u64, u32, u64)> = None;
            let mut truncated_at: Option<(u64, u32, u64, usize)> = None;
            let mut count = 0usize;
            while count < nr_items {
                if pos + SEARCH_HEADER_SIZE > cap {
                    break; // no room even for a header
                }
                let h = &items[pos..pos + SEARCH_HEADER_SIZE];
                let obj = u64::from_ne_bytes(h[8..16].try_into().unwrap());
                let key_off = u64::from_ne_bytes(h[16..24].try_into().unwrap());
                let key_type = u32::from_ne_bytes(h[24..28].try_into().unwrap());
                let len = u32::from_ne_bytes(h[28..32].try_into().unwrap()) as usize;
                if pos + SEARCH_HEADER_SIZE + len > cap {
                    // Header landed but the payload was cut off: refetch this item whole.
                    truncated_at = Some((obj, key_type, key_off, len));
                    break;
                }
                let payload = &items[pos + SEARCH_HEADER_SIZE..pos + SEARCH_HEADER_SIZE + len];
                f(obj, key_type, key_off, payload);
                last_complete = Some((obj, key_type, key_off));
                n_seen += 1;
                pos += SEARCH_HEADER_SIZE + len;
                count += 1;
            }

            // Keep asking until the kernel returns nothing: a buffer that fills
            // up stops mid-item, and "how much room is left" cannot tell the
            // difference between that and the end of the range (an item needs
            // its header *and* payload, and the payload can be any size). One
            // extra, empty call per enumeration is the price of not losing items.
            if nr_items == 0 || last_complete.is_none() && truncated_at.is_none() {
                return Ok(n_seen);
            }
            let resume = match truncated_at {
                // Refetch the cut-off item; if it cannot fit in the whole buffer,
                // there is no way to make progress, so step over it instead.
                Some((obj, ty, off, len)) if SEARCH_HEADER_SIZE + len <= cap => (obj, ty, off),
                Some((obj, ty, off, _)) => (obj, ty, off + 1),
                // Otherwise continue after the last item we delivered whole.
                None => match last_complete {
                    Some((obj, ty, off)) => (obj, ty, off + 1),
                    None => return Ok(n_seen),
                },
            };
            min_objectid = resume.0;
            // advancing the type too is what lets a whole-tree walk move past the
            // last item of a type instead of asking for it again forever
            min_type = resume.1;
            min_offset = match resume.2 {
                u64::MAX => return Ok(n_seen),
                v => v,
            };
        }
    }
}

/// Parse a `struct btrfs_file_extent_item`. Returns None for items that carry
/// no data accounting (holes are reported so the caller can skip them).
pub fn parse_file_extent(p: &[u8], key_offset: u64) -> Option<ExtentItem> {
    if p.len() < INLINE_HEADER_SIZE {
        return None;
    }
    let ram_bytes = u64::from_le_bytes(p[8..16].try_into().unwrap());
    let compression = p[16];
    let kind = p[20];
    if kind == BTRFS_FILE_EXTENT_INLINE {
        // The payload is whatever follows the fixed header.
        let payload = p.len() - INLINE_HEADER_SIZE;
        return Some(ExtentItem {
            kind,
            comp_type: compression as u16,
            disk_bytenr: 0,
            disk_num_bytes: payload as u64,
            ram_bytes,
            num_bytes: ram_bytes,
            key_offset,
        });
    }
    let disk_bytenr = u64::from_le_bytes(p[21..29].try_into().unwrap());
    let disk_num_bytes = u64::from_le_bytes(p[29..37].try_into().unwrap());
    let num_bytes = u64::from_le_bytes(p[45..53].try_into().unwrap());
    let comp_type = if kind == BTRFS_FILE_EXTENT_PREALLOC {
        PREALLOC_TYPE
    } else {
        compression as u16
    };
    Some(ExtentItem {
        kind,
        comp_type,
        disk_bytenr,
        disk_num_bytes,
        ram_bytes,
        num_bytes,
        key_offset,
    })
}

// ---------------------------------------------------------------------------
// Root tree: the inventory of subvolumes (what makes a snapshot a snapshot)
// ---------------------------------------------------------------------------

pub const BTRFS_ROOT_TREE_OBJECTID: u64 = 1;

/// One `struct btrfs_root_item`, reduced to what identifies a subvolume.
#[derive(Debug, Clone, Copy, Default)]
pub struct RootItem {
    pub treeid: u64,
    pub flags: u64,
    pub uuid: [u8; 16],
    pub parent_uuid: [u8; 16],
    pub received_uuid: [u8; 16],
    pub otime_sec: u64,
}

// Offsets inside the packed struct, checked against the C compiler.
const OFF_ROOT_GENERATION: usize = 160;
const OFF_ROOT_FLAGS: usize = 208;
const OFF_ROOT_GENERATION_V2: usize = 239;
const OFF_ROOT_UUID: usize = 247;
const OFF_ROOT_PARENT_UUID: usize = 263;
const OFF_ROOT_RECEIVED_UUID: usize = 279;
const OFF_ROOT_OTIME: usize = 339;
const ROOT_ITEM_FULL_LEN: usize = 439;

fn parse_root_item(p: &[u8], treeid: u64) -> Option<RootItem> {
    if p.len() < OFF_ROOT_FLAGS + 8 {
        return None;
    }
    let u64_at = |off: usize| u64::from_le_bytes(p[off..off + 8].try_into().unwrap());
    let mut item = RootItem {
        treeid,
        flags: u64_at(OFF_ROOT_FLAGS),
        otime_sec: if p.len() >= OFF_ROOT_OTIME + 12 {
            u64_at(OFF_ROOT_OTIME)
        } else {
            0
        },
        ..Default::default()
    };
    // The uuid fields only exist (and are only valid) in the extended layout.
    if p.len() >= ROOT_ITEM_FULL_LEN && u64_at(OFF_ROOT_GENERATION) == u64_at(OFF_ROOT_GENERATION_V2)
    {
        item.uuid.copy_from_slice(&p[OFF_ROOT_UUID..OFF_ROOT_UUID + 16]);
        item.parent_uuid
            .copy_from_slice(&p[OFF_ROOT_PARENT_UUID..OFF_ROOT_PARENT_UUID + 16]);
        item.received_uuid
            .copy_from_slice(&p[OFF_ROOT_RECEIVED_UUID..OFF_ROOT_RECEIVED_UUID + 16]);
    }
    Some(item)
}

/// Every subvolume on the filesystem, with the parent link that distinguishes
/// a snapshot from an original. `fd` may be any file inside the filesystem.
pub fn list_subvolumes(fd: i32) -> io::Result<Vec<RootItem>> {
    let mut searcher = Searcher::new(1 << 20);
    let spec = SearchSpec {
        tree_id: BTRFS_ROOT_TREE_OBJECTID,
        min_type: BTRFS_ROOT_ITEM_KEY,
        max_type: BTRFS_ROOT_ITEM_KEY,
        min_objectid: 1,
        max_objectid: u64::MAX,
    };
    let mut out = Vec::new();
    searcher.for_each_item(fd, &spec, None, &mut |obj, _ty, _off, payload| {
        if let Some(r) = parse_root_item(payload, obj) {
            out.push(r);
        }
    })?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// inode modes: what an INODE_ITEM's `mode` word is classified by
// ---------------------------------------------------------------------------

pub const S_IFMT: u16 = 0o170000;
pub const S_IFREG: u16 = 0o100000;

/// 8-4-4-4-12, the way btrfs-progs prints a UUID.
pub fn fmt_uuid(uuid: &[u8; 16]) -> String {
    let mut s = String::with_capacity(36);
    for (i, b) in uuid.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// fs-tree items: what a metadata scan reads instead of statx + per-inode ioctl
// ---------------------------------------------------------------------------

pub const BTRFS_INODE_ITEM_KEY: u32 = 1;
pub const BTRFS_ROOT_BACKREF_KEY: u32 = 144;
pub const BTRFS_DIR_INDEX_KEY: u32 = 96;

/// A directory entry: `btrfs_dir_item` is packed, location@0 (a disk key:
/// objectid@0, type@8, offset@9), name_len@27, file type@29, name@30.
#[derive(Debug, Clone, Copy)]
pub struct DirEntry<'a> {
    pub child: u64,
    /// key type of the location: BTRFS_ROOT_ITEM_KEY means the child is a
    /// subvolume root rather than a file inside this tree
    pub location_type: u8,
    pub name: &'a [u8],
}

pub fn parse_dir_item(p: &[u8]) -> Option<DirEntry<'_>> {
    if p.len() < 30 {
        return None;
    }
    let child = u64::from_le_bytes(p[0..8].try_into().unwrap());
    let location_type = p[8];
    let name_len = u16::from_le_bytes(p[27..29].try_into().unwrap()) as usize;
    if p.len() < 30 + name_len {
        return None;
    }
    Some(DirEntry {
        child,
        location_type,
        name: &p[30..30 + name_len],
    })
}
pub const BTRFS_FIRST_FREE_OBJECTID: u64 = 256;

pub const S_IFDIR: u16 = 0o040000;

/// Everything the scan needs about one inode, read from its INODE_ITEM.
#[derive(Debug, Clone, Copy, Default)]
pub struct InodeItem {
    pub mode: u16,
    pub mtime_sec: u64,
    /// inode creation time - the ownership rule's primary key
    pub otime_sec: u64,
    pub otime_nsec: u32,
}

/// `struct btrfs_inode_item` is packed: size@16, nlink@40, mode@52, mtime@136,
/// otime@148 (each timespec is 8 bytes of seconds + 4 of nanoseconds).
pub fn parse_inode_item(p: &[u8]) -> Option<InodeItem> {
    if p.len() < 160 {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(p[o..o + 8].try_into().unwrap());
    let u32_at = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
    let u16_at = |o: usize| u16::from_le_bytes(p[o..o + 2].try_into().unwrap());
    Some(InodeItem {
        mode: u16_at(52),
        mtime_sec: u64_at(136),
        otime_sec: u64_at(148),
        otime_nsec: u32_at(156),
    })
}

/// Enumerate every item of one subvolume's fs tree, in key order.
pub fn for_each_fs_item<F>(
    fd: i32,
    subvol_root_id: u64,
    cancel: Option<&AtomicBool>,
    f: &mut F,
) -> io::Result<u64>
where
    F: FnMut(u64, u32, u64, &[u8]),
{
    let mut searcher = Searcher::new(4 << 20);
    let spec = SearchSpec {
        tree_id: subvol_root_id,
        min_type: 0,
        max_type: u32::MAX,
        min_objectid: BTRFS_FIRST_FREE_OBJECTID,
        max_objectid: u64::MAX,
    };
    searcher.for_each_item(fd, &spec, cancel, f)
}

/// `(child_root, dirid, name)` for every subvolume that lives inside
/// `parent_root` - found in the root tree, so no directory walk is needed to
/// know where subvolumes are nested.
///
/// A backref is keyed `(child_root, ROOT_BACKREF, parent_root)`: the objectid is
/// the subvolume and the offset is the tree that *holds* its entry - the
/// subvolume it lives in, not the one it was snapshotted from. Its payload is
/// `(dirid, sequence, name)`, the directory in that tree and the entry's name
/// there. Measured on this filesystem: a subvolume has exactly one of these, a
/// snapshot of the tree holding it does not get one of its own, and moving the
/// subvolume updates this one in place (dirid and sequence change).
///
/// Every entry is recorded twice in the root tree, as `(child, ROOT_BACKREF,
/// parent)` and as the mirror `(parent, ROOT_REF, child)`, so a search over one
/// offset sees both and the mirror reads as the child's *parent* being a child
/// of `parent_root`. The type is therefore checked here: only a ROOT_BACKREF
/// says "this subvolume is inside that one", and the item's payload - the dirid
/// of the containing directory - belongs to the backref that named it.
pub fn subvolumes_inside(fd: i32, parent_root: u64) -> io::Result<Vec<(u64, u64, Vec<u8>)>> {
    let mut searcher = Searcher::new(1 << 20);
    let spec = SearchSpec {
        tree_id: BTRFS_ROOT_TREE_OBJECTID,
        min_type: BTRFS_ROOT_BACKREF_KEY,
        max_type: BTRFS_ROOT_BACKREF_KEY,
        min_objectid: 1,
        max_objectid: u64::MAX,
    };
    let mut out = Vec::new();
    searcher.for_each_item(fd, &spec, None, &mut |child, ty, parent, payload| {
        if ty != BTRFS_ROOT_BACKREF_KEY || parent != parent_root || payload.len() < 18 {
            return;
        }
        let dirid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        let name_len = u16::from_le_bytes(payload[16..18].try_into().unwrap()) as usize;
        if payload.len() >= 18 + name_len {
            out.push((child, dirid, payload[18..18 + name_len].to_vec()));
        }
    })?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// INO_LOOKUP
// ---------------------------------------------------------------------------

/// struct btrfs_ioctl_ino_lookup_args: `{ u64 treeid; u64 objectid; char
/// name[4080]; }` - the name buffer is BTRFS_INO_LOOKUP_PATH_MAX and the struct
/// is exactly one page.
const INO_LOOKUP_SIZE: usize = 4096;
const INO_LOOKUP_NAME_OFF: usize = 16;
const INO_LOOKUP_NAME_MAX: usize = 4080;

/// The path from `treeid`'s root down to one inode, as the kernel knows it:
/// `/`-separated names, NUL-terminated. The subvolume's root itself comes back
/// as the empty string.
///
/// This is how a subvolume found in the root tree gets a real path: its tree is
/// never walked, so the dirid in its backref names a directory in that tree
/// that nothing here has read. It is one small ioctl, and it is only needed
/// where a walk was skipped for exactly that reason.
pub fn ino_lookup(fd: i32, treeid: u64, objectid: u64) -> io::Result<Vec<u8>> {
    let mut buf = [0u8; INO_LOOKUP_SIZE];
    buf[0..8].copy_from_slice(&treeid.to_ne_bytes());
    buf[8..16].copy_from_slice(&objectid.to_ne_bytes());
    let rc = unsafe { ioctl(fd, BTRFS_IOC_INO_LOOKUP, buf.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let name = &buf[INO_LOOKUP_NAME_OFF..INO_LOOKUP_NAME_OFF + INO_LOOKUP_NAME_MAX];
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Ok(name[..end].to_vec())
}

// ---------------------------------------------------------------------------
// extent tree: who references each physical extent
// ---------------------------------------------------------------------------

pub const BTRFS_EXTENT_TREE_OBJECTID: u64 = 2;
pub const BTRFS_EXTENT_ITEM_KEY: u32 = 168;
pub const BTRFS_METADATA_ITEM_KEY: u32 = 169;
pub const BTRFS_EXTENT_DATA_REF_KEY: u32 = 178;
pub const BTRFS_SHARED_DATA_REF_KEY: u32 = 184;

/// A data extent's header: `btrfs_extent_item` is 24 bytes packed, with the
/// reference count first, the generation, and the extent flags last. Only the
/// flags are read here: the other two are not part of the accounting.
#[derive(Debug, Clone, Copy)]
pub struct ExtentItemHdr {
    pub flags: u64,
    /// a metadata (tree block) extent: what the scan counts against the tree
    /// rather than against file data
    pub is_metadata: bool,
}

/// `BTRFS_EXTENT_FLAG_TREE_BLOCK`: the extent holds a tree block. This - not
/// the block-group flags - is what marks an extent item as metadata, and it is
/// also what decides whether a `btrfs_tree_block_info` follows the header.
pub const BTRFS_EXTENT_FLAG_TREE_BLOCK: u64 = 1 << 1;

/// True for a `METADATA_ITEM` key, or an `EXTENT_ITEM` whose flags say tree block.
pub fn is_metadata_extent(key_type: u32, hdr: &ExtentItemHdr) -> bool {
    key_type == BTRFS_METADATA_ITEM_KEY || hdr.flags & BTRFS_EXTENT_FLAG_TREE_BLOCK != 0
}

pub fn parse_extent_item_hdr(p: &[u8]) -> Option<ExtentItemHdr> {
    if p.len() < 24 {
        return None;
    }
    let flags = u64::from_le_bytes(p[16..24].try_into().unwrap());
    Some(ExtentItemHdr {
        flags,
        is_metadata: flags & BTRFS_EXTENT_FLAG_TREE_BLOCK != 0,
    })
}

/// Inline references live inside the EXTENT_ITEM itself, after its 24-byte
/// header: a 9-byte `btrfs_extent_inline_ref` (type + offset) and, for a data
/// reference, a 28-byte `btrfs_extent_data_ref` body. An extent with a single
/// reference - the common case - stores it only here, so a reader that looks
/// for EXTENT_DATA_REF items alone sees almost nothing.
pub const BTRFS_TREE_BLOCK_REF_KEY: u32 = 176;
/// simple-quota's "which subvolume wrote this extent" ref; in the tree it is
/// carried in the header's offset field, so it is 9 bytes with no body
pub const BTRFS_EXTENT_OWNER_REF_KEY: u32 = 172;
pub const BTRFS_SHARED_BLOCK_REF_KEY: u32 = 182;

/// Size of one inline reference in bytes, from its own type byte.
///
/// This is btrfs-progs' `btrfs_extent_inline_ref_size()`: a data reference is
/// **29**, not 37, because its `btrfs_extent_data_ref` starts *at the header's
/// offset field* - those 8 bytes are shared with the reference header, not added
/// to it. Reading it as 9 + 28 steps eight bytes too far on every data
/// reference and lands the walk inside the next one, which is what made 113 of
/// 200 sampled items "desynchronise on a zero byte".
pub fn inline_ref_size(ty: u32) -> Option<usize> {
    match ty {
        BTRFS_TREE_BLOCK_REF_KEY | BTRFS_SHARED_BLOCK_REF_KEY | BTRFS_EXTENT_OWNER_REF_KEY => Some(9),
        BTRFS_SHARED_DATA_REF_KEY => Some(13),
        BTRFS_EXTENT_DATA_REF_KEY => Some(29),
        _ => None,
    }
}

/// Where an extent item's inline references begin: after the 24-byte
/// `btrfs_extent_item` and, for a tree-block `EXTENT_ITEM` (the fat, pre-skinny
/// layout), after the `btrfs_tree_block_info` that follows it. A
/// `METADATA_ITEM` is skinny - the block's level rides in the key offset
/// instead - so its references start right after the header.
pub fn inline_ref_start(key_type: u32, p: &[u8]) -> Option<usize> {
    if p.len() < 24 {
        return None;
    }
    if key_type == BTRFS_METADATA_ITEM_KEY {
        return Some(24);
    }
    let flags = u64::from_le_bytes(p[16..24].try_into().unwrap());
    Some(if flags & BTRFS_EXTENT_FLAG_TREE_BLOCK != 0 {
        24 + 18 // struct btrfs_tree_block_info: a disk key (17) plus level
    } else {
        24
    })
}

/// Visit every inline reference of an extent item, in order, as
/// `f(type, payload)`: the 8-byte offset for a block ref or owner ref, and for
/// a data ref the 28-byte `btrfs_extent_data_ref` that the offset field starts.
/// Returns false if the refs do not tile the item exactly - an unknown type,
/// an overrun, or a trailing remainder - so callers can tell a complete parse
/// from a guess.
pub fn for_each_inline_ref<F: FnMut(u32, &[u8])>(key_type: u32, p: &[u8], f: &mut F) -> bool {
    let Some(mut off) = inline_ref_start(key_type, p) else {
        return false;
    };
    while off < p.len() {
        let ty = p[off] as u32;
        let Some(sz) = inline_ref_size(ty) else {
            return false;
        };
        if off + sz > p.len() {
            return false;
        }
        f(ty, &p[off + 1..off + sz]);
        off += sz;
    }
    true
}

/// A shared reference to a data extent: the block that holds the reference.
/// `btrfs_shared_data_ref` is parent then a 4-byte count; only the parent is
/// needed, since it is what the block-reference graph resolves to subvolumes.
#[derive(Debug, Clone, Copy)]
pub struct SharedDataRef {
    pub parent: u64,
}

pub fn parse_shared_data_ref(p: &[u8]) -> Option<SharedDataRef> {
    if p.len() < 12 {
        return None;
    }
    Some(SharedDataRef {
        parent: u64::from_le_bytes(p[0..8].try_into().unwrap()),
    })
}

/// One reference to a data extent: which subvolume, and how many times over
/// (`btrfs_extent_data_ref`, 28 bytes packed; the inode and file offset that
/// follow the subvolume are not part of the accounting).
#[derive(Debug, Clone, Copy)]
pub struct DataRef {
    pub root: u64,
    pub count: u32,
}

pub fn parse_data_ref(p: &[u8]) -> Option<DataRef> {
    if p.len() < 28 {
        return None;
    }
    Some(DataRef {
        root: u64::from_le_bytes(p[0..8].try_into().unwrap()),
        count: u32::from_le_bytes(p[24..28].try_into().unwrap()),
    })
}

/// Enumerate the whole extent tree in key order: each data extent's header is
/// followed by its references, which is what makes a streaming pass possible.
pub fn for_each_extent_tree_item<F>(
    fd: i32,
    cancel: Option<&AtomicBool>,
    f: &mut F,
) -> io::Result<u64>
where
    F: FnMut(u64, u32, u64, &[u8]),
{
    let mut searcher = Searcher::new(4 << 20);
    let spec = SearchSpec {
        tree_id: BTRFS_EXTENT_TREE_OBJECTID,
        min_type: 0,
        max_type: u32::MAX,
        min_objectid: 0,
        max_objectid: u64::MAX,
    };
    searcher.for_each_item(fd, &spec, cancel, f)
}

// ---------------------------------------------------------------------------
// LOGICAL_INO_V2: the kernel resolves an extent's references for us
// ---------------------------------------------------------------------------

/// struct btrfs_ioctl_logical_ino_args is 56 bytes (checked against C)
const BTRFS_IOC_LOGICAL_INO_V2: u64 = ioc(IOC_WRITE | IOC_READ, BTRFS_IOCTL_MAGIC, 59, 56);
/// "return every ref to the extent, not just those containing logical block"
pub const BTRFS_LOGICAL_INO_ARGS_IGNORE_OFFSET: u64 = 1;

#[repr(C)]
#[derive(Default)]
struct LogicalInoArgs {
    logical: u64,
    size: u64,
    reserved: [u64; 3],
    flags: u64,
    inodes: u64,
}

/// Every reference to the extent starting at `bytenr`: (root, inode, offset).
///
/// One ioctl per extent, but the kernel does the reference-tree walk and the
/// item parsing, which is both exact and far less code than reading the extent
/// tree's inline reference encoding by hand.
pub fn logical_ino(fd: i32, bytenr: u64, buf: &mut Vec<u8>) -> io::Result<Vec<(u64, u64, u64)>> {
    if buf.len() < 4096 {
        buf.resize(64 * 1024, 0);
    }
    let mut args = LogicalInoArgs {
        logical: bytenr,
        size: buf.len() as u64,
        flags: BTRFS_LOGICAL_INO_ARGS_IGNORE_OFFSET,
        inodes: buf.as_mut_ptr() as u64,
        ..Default::default()
    };
    let rc = unsafe { ioctl(fd, BTRFS_IOC_LOGICAL_INO_V2, &mut args as *mut LogicalInoArgs) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // btrfs_data_container: bytes_left, bytes_missing, elem_cnt, elem_missed, then u64 values
    let elem_cnt = u32::from_ne_bytes(buf[8..12].try_into().unwrap()) as usize;
    let mut out = Vec::with_capacity(elem_cnt / 3);
    let mut i = 16;
    while i + 24 <= 16 + elem_cnt * 8 && i + 24 <= buf.len() {
        let inum = u64::from_ne_bytes(buf[i..i + 8].try_into().unwrap());
        let offset = u64::from_ne_bytes(buf[i + 8..i + 16].try_into().unwrap());
        let root = u64::from_ne_bytes(buf[i + 16..i + 24].try_into().unwrap());
        out.push((root, inum, offset));
        i += 24;
    }
    Ok(out)
}
