// SPDX-License-Identifier: GPL-2.0-or-later

//! Raw btrfs ioctl bindings.
//!
//! This mirrors what compsize(8) does: for every regular file, ask the kernel
//! for the inode's EXTENT_DATA items (`BTRFS_IOC_TREE_SEARCH_V2` with
//! `tree_id = 0`, which searches the subvolume tree the inode belongs to).
//! No libbtrfs is needed - the on-disk item layout is stable and defined in
//! linux/btrfs_tree.h.

use std::io;

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

/// Bit 0 of `struct btrfs_root_item`'s flags, and of the flags
/// GET_SUBVOL_INFO reports (which are a copy of the root item's). The ioctl
/// flag of the same name - what BTRFS_IOC_SUBVOL_SETFLAGS takes - is a
/// different bit and never shows up in either.
pub const BTRFS_ROOT_SUBVOL_RDONLY: u64 = 1 << 0;

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
// TREE_SEARCH_V2: fs-tree items
// ---------------------------------------------------------------------------

pub const BTRFS_INODE_ITEM_KEY: u32 = 1;

/// What an INODE_ITEM says about times. It is the record the tree reader puts
/// in a node, so an inode whose statx cannot be reached - the directory a mount
/// point covers - can still be described the way the reader describes it.
#[derive(Debug, Clone, Copy)]
pub struct InodeTimes {
    pub mtime_sec: u64,
    pub otime_sec: u64,
    pub otime_nsec: u32,
}

/// `struct btrfs_inode_item` is packed; the three times sit after the
/// generation/transid/size counters, and otime is the last of them.
pub fn parse_inode_times(p: &[u8]) -> Option<InodeTimes> {
    if p.len() < 160 {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(p[o..o + 8].try_into().unwrap());
    let u32_at = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
    Some(InodeTimes {
        mtime_sec: u64_at(136),
        otime_sec: u64_at(148),
        otime_nsec: u32_at(156),
    })
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
    pub item_type: u32,
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

    fn set_key(&mut self, spec: &SearchSpec, min_objectid: u64, min_offset: u64) {
        const MAX: u64 = u64::MAX;
        let b = &mut self.buf;
        b[0..8].copy_from_slice(&spec.tree_id.to_ne_bytes());
        b[8..16].copy_from_slice(&min_objectid.to_ne_bytes()); // min_objectid
        b[16..24].copy_from_slice(&spec.max_objectid.to_ne_bytes()); // max_objectid
        b[24..32].copy_from_slice(&min_offset.to_ne_bytes()); // min_offset
        b[32..40].copy_from_slice(&MAX.to_ne_bytes()); // max_offset
        b[40..48].copy_from_slice(&0u64.to_ne_bytes()); // min_transid
        b[48..56].copy_from_slice(&MAX.to_ne_bytes()); // max_transid
        b[56..60].copy_from_slice(&spec.item_type.to_ne_bytes()); // min_type
        b[60..64].copy_from_slice(&spec.item_type.to_ne_bytes()); // max_type
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
            item_type: BTRFS_EXTENT_DATA_KEY,
            min_objectid: ino,
            max_objectid: ino,
        };
        self.for_each_item(fd, &spec, &mut |_obj, key_offset, payload| {
            if let Some(item) = parse_extent_item(payload, key_offset) {
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
    pub fn for_each_item<F>(&mut self, fd: i32, spec: &SearchSpec, f: &mut F) -> io::Result<u64>
    where
        F: FnMut(u64, u64, &[u8]),
    {
        let cap = self.capacity();
        let mut n_seen = 0u64;
        let mut min_objectid = spec.min_objectid;
        let mut min_offset = 0u64;
        loop {
            self.set_key(spec, min_objectid, min_offset);
            let rc = unsafe { ioctl(fd, BTRFS_IOC_TREE_SEARCH_V2, self.buf.as_mut_ptr()) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            let nr_items = u32::from_ne_bytes(self.buf[64..68].try_into().unwrap()) as usize;

            let items = &self.buf[SEARCH_ARGS_V2_SIZE..];
            let mut pos = 0usize;
            // (objectid, offset) of the last item we fully delivered
            let mut last_complete: Option<(u64, u64)> = None;
            let mut truncated_at: Option<(u64, u64, usize)> = None;
            let mut count = 0usize;
            while count < nr_items {
                if pos + SEARCH_HEADER_SIZE > cap {
                    break; // no room even for a header
                }
                let h = &items[pos..pos + SEARCH_HEADER_SIZE];
                let obj = u64::from_ne_bytes(h[8..16].try_into().unwrap());
                let key_off = u64::from_ne_bytes(h[16..24].try_into().unwrap());
                let len = u32::from_ne_bytes(h[28..32].try_into().unwrap()) as usize;
                if pos + SEARCH_HEADER_SIZE + len > cap {
                    // Header landed but the payload was cut off: refetch this item whole.
                    truncated_at = Some((obj, key_off, len));
                    break;
                }
                let payload = &items[pos + SEARCH_HEADER_SIZE..pos + SEARCH_HEADER_SIZE + len];
                f(obj, key_off, payload);
                last_complete = Some((obj, key_off));
                n_seen += 1;
                pos += SEARCH_HEADER_SIZE + len;
                count += 1;
            }

            // Keep asking until the kernel returns nothing. "Is there room for
            // another header" cannot tell a full buffer from the end of the
            // range: an item needs its header *and* its payload, so a buffer
            // ending 1-31 bytes short looks not-full and the remaining items
            // would be dropped silently. One extra, empty call is the price.
            if nr_items == 0 || last_complete.is_none() && truncated_at.is_none() {
                return Ok(n_seen);
            }
            let resume = match truncated_at {
                // Refetch the cut-off item; if it cannot fit in the whole buffer,
                // there is no way to make progress, so step over it instead.
                Some((obj, off, len)) if SEARCH_HEADER_SIZE + len <= cap => (obj, off),
                Some((obj, off, _)) => (obj, off + 1),
                None => match last_complete {
                    Some((obj, off)) => (obj, off + 1),
                    None => return Ok(n_seen),
                },
            };
            min_objectid = resume.0;
            min_offset = match resume.1 {
                u64::MAX => return Ok(n_seen),
                v => v,
            };
        }
    }
}

/// Parse a `struct btrfs_file_extent_item`. Returns None for items that carry
/// no data accounting (holes are reported so the caller can skip them).
fn parse_extent_item(p: &[u8], key_offset: u64) -> Option<ExtentItem> {
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
        item_type: BTRFS_ROOT_ITEM_KEY,
        min_objectid: 1,
        max_objectid: u64::MAX,
    };
    let mut out = Vec::new();
    searcher.for_each_item(fd, &spec, &mut |obj, _off, payload| {
        if let Some(r) = parse_root_item(payload, obj) {
            out.push(r);
        }
    })?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// statx: one call per file that yields inode, mode, mtime and birth time
// ---------------------------------------------------------------------------

extern "C" {
    fn statx(dirfd: i32, pathname: *const i8, flags: i32, mask: u32, buf: *mut Statx) -> i32;
}

pub const AT_FDCWD: i32 = -100;
pub const AT_SYMLINK_NOFOLLOW: i32 = 0x100;
const STATX_BASIC_STATS: u32 = 0x7ff;
pub const STATX_BTIME: u32 = 0x800;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    _reserved: i32,
}

/// struct statx, 256 bytes; offsets checked against the C compiler.
#[repr(C)]
#[derive(Clone, Copy)]
struct Statx {
    mask: u32,
    blksize: u32,
    attributes: u64,
    nlink: u32,
    uid: u32,
    gid: u32,
    mode: u16,
    _spare0: u16,
    ino: u64,
    size: u64,
    blocks: u64,
    attributes_mask: u64,
    atime: StatxTimestamp,
    btime: StatxTimestamp,
    ctime: StatxTimestamp,
    mtime: StatxTimestamp,
    rdev_major: u32,
    rdev_minor: u32,
    dev_major: u32,
    dev_minor: u32,
    mnt_id: u64,
    dio_mem_align: u32,
    dio_offset_align: u32,
    _spare3: [u64; 12],
}

#[derive(Clone, Copy, Debug)]
pub struct StatInfo {
    pub ino: u64,
    /// S_IFMT bits
    pub mode: u16,
    pub mtime: u64,
    /// inode creation time; the ownership rule's primary key
    pub btime: u64,
    /// sub-second part, in nanoseconds
    pub btime_ns: u32,
    pub has_btime: bool,
    pub dev: u64,
}

pub const S_IFMT: u16 = 0o170000;
pub const S_IFREG: u16 = 0o100000;

/// The inode number of every subvolume's root directory. It is what tells a
/// mounted subvolume apart from a bind mount of an ordinary directory, whose
/// inode is the directory's own.
pub const BTRFS_FIRST_FREE_OBJECTID: u64 = 256;

/// `path` must be NUL-terminated.
pub fn stat_path(path: &[u8], nofollow: bool) -> io::Result<StatInfo> {
    let mut st: Statx = unsafe { std::mem::zeroed() };
    let flags = if nofollow { AT_SYMLINK_NOFOLLOW } else { 0 };
    let rc = unsafe {
        statx(
            AT_FDCWD,
            path.as_ptr() as *const i8,
            flags,
            STATX_BASIC_STATS | STATX_BTIME,
            &mut st,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(StatInfo {
        ino: st.ino,
        mode: st.mode,
        mtime: st.mtime.tv_sec.max(0) as u64,
        btime: st.btime.tv_sec.max(0) as u64,
        btime_ns: st.btime.tv_nsec.min(999_999_999),
        has_btime: st.mask & STATX_BTIME != 0,
        // st_dev as the rest of the system encodes it (new_encode_dev)
        dev: ((st.dev_major as u64) << 20) | (st.dev_minor as u64 & 0x000f_ffff),
    })
}

// ---------------------------------------------------------------------------
// FS_INFO: which filesystem is this, really
// ---------------------------------------------------------------------------

const FS_INFO_SIZE: usize = 1024;
const OFF_FS_INFO_FSID: usize = 16;
const OFF_FS_INFO_METADATA_UUID: usize = 64;
const BTRFS_IOC_FS_INFO: u64 = ioc(IOC_READ, BTRFS_IOCTL_MAGIC, 31, FS_INFO_SIZE as u64);

/// Identity of the filesystem an fd belongs to: the fsid stored in the btrfs
/// superblock, plus its metadata_uuid when that feature is in use. This is the
/// same UUID `btrfs filesystem show` prints; it is per *filesystem*, identical
/// for every subvolume of it, and persisted on disk.
///
/// Deliberately not statfs(2)'s f_fsid: that is a derived value which differs
/// between subvolumes of one filesystem and whose computation has changed
/// between kernels, so it identifies neither the volume nor anything stable.
pub fn fs_identity(fd: i32) -> io::Result<([u8; 16], [u8; 16])> {
    let mut buf = [0u8; FS_INFO_SIZE];
    let rc = unsafe { ioctl(fd, BTRFS_IOC_FS_INFO, buf.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fsid = [0u8; 16];
    fsid.copy_from_slice(&buf[OFF_FS_INFO_FSID..OFF_FS_INFO_FSID + 16]);
    let mut meta = [0u8; 16];
    meta.copy_from_slice(&buf[OFF_FS_INFO_METADATA_UUID..OFF_FS_INFO_METADATA_UUID + 16]);
    Ok((fsid, meta))
}

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

/// The filesystem's name for humans: its metadata UUID when set, else the fsid.
pub fn fmt_fsid(id: &([u8; 16], [u8; 16])) -> String {
    if id.1 != [0u8; 16] {
        fmt_uuid(&id.1)
    } else {
        fmt_uuid(&id.0)
    }
}
