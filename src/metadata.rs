//! Reading the trees instead of the files.
//!
//! The file backend asks the kernel about one inode at a time - `statx` for the
//! metadata, `BTRFS_IOC_TREE_SEARCH_V2` for that inode's extents. Both facts
//! already live in the subvolume's *fs tree*, which can be enumerated in key
//! order instead: `INODE_ITEM` carries size, mode, mtime and birth time,
//! `INODE_REF`/`INODE_EXTREF` carry the parent inode and name, and
//! `EXTENT_DATA` carries the same extent list the per-inode search returns.
//!
//! One sequential pass per subvolume replaces one lookup per file, and the
//! directory structure comes out of the same pass - no `readdir` walk needed.
//! Subvolume boundaries are known from the root tree's backrefs, so a nested
//! subvolume or snapshot is found without walking the directory tree above it.

use crate::btrfs::{self, BTRFS_EXTENT_DATA_KEY, BTRFS_INODE_EXTREF_KEY, BTRFS_INODE_ITEM_KEY, BTRFS_INODE_REF_KEY};
use crate::model::{KIND_DIR, KIND_FILE};
use std::io;

/// What one pass over a subvolume's fs tree found. Used to validate the reader
/// against what `find` and the file backend see before anything is built on it.
#[derive(Default, Debug)]
pub struct SubvolProbe {
    pub inodes: u64,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub other: u64,
    pub refs: u64,
    pub extrefs: u64,
    pub extents: u64,
    pub items: u64,
    /// subvolumes nested inside this one, from the root tree's backrefs
    pub nested: Vec<(u64, u64, Vec<u8>)>,
}

fn is_dir(mode: u16) -> bool {
    mode & btrfs::S_IFMT == btrfs::S_IFDIR
}

/// One streaming pass, counting what is there.
pub fn probe(fd: i32, subvol_root_id: u64) -> io::Result<SubvolProbe> {
    let mut p = SubvolProbe::default();
    let mut seen_inode = u64::MAX;
    let mut first_key: Option<(u64, u32, u64)> = None;
    let mut last_key: Option<(u64, u32, u64)> = None;
    let mut dbg = std::env::var_os("CST_META_DEBUG").is_some();
    btrfs::for_each_fs_item(fd, subvol_root_id, &mut |obj, ty, off, payload| {
        if first_key.is_none() {
            first_key = Some((obj, ty, off));
        }
        last_key = Some((obj, ty, off));
        if dbg && p.items < 5 {
            eprintln!("  [item] obj={obj} type={ty} off={off} len={}", payload.len());
        }
        p.items += 1;
        match ty {
            BTRFS_INODE_ITEM_KEY => {
                let Some(item) = btrfs::parse_inode_item(payload) else {
                    p.other += 1;
                    return;
                };
                // one INODE_ITEM per inode, and they arrive in inode order
                if obj != seen_inode {
                    seen_inode = obj;
                    p.inodes += 1;
                    if is_dir(item.mode) {
                        p.dirs += 1;
                    } else if item.mode & btrfs::S_IFMT == btrfs::S_IFREG {
                        p.files += 1;
                    } else if item.mode & btrfs::S_IFMT == btrfs::S_IFLNK {
                        p.symlinks += 1;
                    } else {
                        p.other += 1;
                    }
                }
            }
            BTRFS_INODE_REF_KEY => p.refs += 1,
            BTRFS_INODE_EXTREF_KEY => {
                p.refs += 1;
                p.extrefs += 1;
            }
            BTRFS_EXTENT_DATA_KEY => p.extents += 1,
            _ => {}
        }
    })?;
    p.nested = btrfs::subvolumes_inside(fd, subvol_root_id)?;
    if dbg || std::env::var_os("CST_META_DEBUG").is_some() {
        eprintln!("  [range] first={first_key:?} last={last_key:?}");
    }
    let _ = dbg;
    dbg = false;
    let _ = dbg;
    Ok(p)
}

/// Which node kind an inode mode maps to, or None for things we do not scan
/// (devices, fifos, sockets - the file backend skips them too).
pub fn kind_of(mode: u16) -> Option<u8> {
    match mode & btrfs::S_IFMT {
        btrfs::S_IFDIR => Some(KIND_DIR),
        btrfs::S_IFREG => Some(KIND_FILE),
        _ => None,
    }
}
