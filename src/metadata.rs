//! Reading the trees instead of the files.
//!
//! The file backend asks the kernel about one inode at a time - `statx` for the
//! metadata, `BTRFS_IOC_TREE_SEARCH_V2` for that inode's extents. Both facts
//! already live in the subvolume's *fs tree*, which can be enumerated in key
//! order instead:
//!
//! * `INODE_ITEM` carries mode, size, mtime and birth time (`otime`), so no
//!   `statx` is needed;
//! * `DIR_INDEX` carries (directory inode, name, child inode, child type),
//!   which is the whole directory structure - and, unlike `INODE_REF`, it sees
//!   nested subvolumes, whose root inodes live in another tree;
//! * `EXTENT_DATA` is the same extent list the per-inode search returns.
//!
//! Subvolume boundaries come from the root tree's backrefs, so a nested
//! subvolume or snapshot is found without walking anything above it.

use crate::btrfs::{
    self, BTRFS_DIR_INDEX_KEY, BTRFS_EXTENT_DATA_KEY, BTRFS_FILE_EXTENT_INLINE, BTRFS_INODE_ITEM_KEY,
};
use crate::model::*;
use crate::scan::{Progress, ScanOptions, ScanStats};
use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use std::io;

pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

/// One inode's metadata, plus the accumulated extent accounting for it.
#[derive(Clone, Copy, Default)]
struct InodeRec {
    inode: u64,
    mode: u16,
    mtime: u32,
    btime: u32,
    btime_ns: u16,
    referenced: u64,
    first_ref: u32,
    nrefs: u32,
}

/// A directory entry, sorted by (parent, name) once collection is done.
#[derive(Clone, Copy)]
struct Entry {
    parent: u64,
    child: u64,
    name_off: u32,
    name_len: u16,
    ft: u8,
    /// the location's key type: BTRFS_ROOT_ITEM_KEY means a subvolume entry
    loc_type: u8,
}

/// Which inodes are inside a directory of `root_id`, found by walking
/// DIR_INDEX entries only - far cheaper than a second full pass, and it is what
/// lets a scan rooted inside a subvolume ignore the rest of it.
fn scope_of(fd: i32, root_id: u64, root_inode: u64) -> io::Result<HashSet<u64>> {
    let mut entries: Vec<(u64, u64)> = Vec::new(); // (parent dir, child)
    btrfs::for_each_fs_item(fd, root_id, None, &mut |obj, ty, _off, payload| {
        if ty != BTRFS_DIR_INDEX_KEY {
            return;
        }
        if let Some(e) = btrfs::parse_dir_item(payload) {
            entries.push((obj, e.child));
        }
    })?;
    entries.sort_unstable();
    let mut inside: HashSet<u64> = HashSet::new();
    inside.insert(root_inode);
    let mut frontier = vec![root_inode];
    while let Some(dir) = frontier.pop() {
        let lo = entries.partition_point(|e| e.0 < dir);
        let hi = entries.partition_point(|e| e.0 <= dir);
        for (_, child) in &entries[lo..hi] {
            if inside.insert(*child) {
                frontier.push(*child);
            }
        }
    }
    Ok(inside)
}

/// One subvolume's collected tree.
struct Collected {
    subvol: u16,
    recs: Vec<InodeRec>,
    entries: Vec<Entry>,
    names: Vec<u8>,
    /// subvolumes nested directly inside this one: (child root id, dir inode, name)
    nested: Vec<(u64, u64, Vec<u8>)>,
}

/// What one fs-tree walk writes into: the shared extent map, the flat
/// reference array, and the warnings the walk raised.
///
/// A walk owns one of these - in parallel, each job gets its own, so no two
/// walks touch the same vec. The collector holds the shard the walks are merged
/// into; at one thread that is the only shard there is, so a serial scan
/// neither copies nor merges anything.
#[derive(Default)]
struct Shard {
    extents: ExtentMap,
    refs: Vec<u64>,
    warnings: Vec<String>,
}

/// Collects a whole scan: the shared extent map, the flat reference array and
/// per-subvolume trees.
struct Collector {
    shard: Shard,
    subs: Vec<Collected>,
    subvols: Vec<Subvol>,
    unreadable: u64,
    files: u64,
    dirs: u64,
    /// walk every fs tree, including the snapshot-class subvolumes (the M2
    /// behaviour: snapshot interiors stay browsable)
    full_scan: bool,
    /// snapshot-class subvolumes left un-walked: (root id, subvol index, path)
    collapsed: Vec<(u64, u16, Vec<u8>)>,
    /// (collapsed parent's root id, child subvol index, name) for subvolumes
    /// found through the root tree inside a collapsed one: their trees are
    /// read, and this is where their nodes hang.
    graph_kids: Vec<(u64, u16, Vec<u8>)>,
}

impl Collector {
    /// Should this nested subvolume be walked, or stood in for by a single
    /// node whose references come from the extent tree?
    ///
    /// Only *snapshots* collapse - subvolumes whose `parent_uuid` still
    /// resolves to another subvolume of this filesystem. A subvolume that
    /// merely shares reflinked extents with another has its own tree, its own
    /// inodes and its own names, and is walked like any other.
    fn collapse(&self, subvol: u16) -> bool {
        !self.full_scan && self.subvols[subvol as usize].is_snapshot()
    }
}

impl Collector {
    /// Intern a subvolume from its root item, for one found in the root tree
    /// rather than by opening a path: the root tree carries the whole identity
    /// (uuid, parent_uuid, flags, otime), so nothing has to be opened - which is
    /// what lets a subvolume nested inside a *collapsed* one, whose directories
    /// are never read, still be found and judged on its own merits.
    fn intern_root(
        &mut self,
        r: &btrfs::RootItem,
        inventory: &[btrfs::RootItem],
        path: &[u8],
        name: &[u8],
    ) -> u16 {
        if let Some(i) = self.subvols.iter().position(|s| s.id == r.treeid) {
            return i as u16;
        }
        let snapshot = r.parent_uuid != [0u8; 16] && inventory.iter().any(|x| x.uuid == r.parent_uuid);
        self.subvols.push(Subvol {
            id: r.treeid,
            uuid: r.uuid,
            parent_uuid: r.parent_uuid,
            flags: r.flags,
            otime: r.otime_sec,
            name: name.to_vec(),
            path: path.to_vec(),
            seen: true,
            snapshot,
        });
        (self.subvols.len() - 1) as u16
    }

    fn intern_subvol(&mut self, info: &btrfs::SubvolInfo, inventory: &[btrfs::RootItem], path: &[u8]) -> u16 {
        if let Some(i) = self.subvols.iter().position(|s| s.id == info.treeid) {
            return i as u16;
        }
        // a subvolume is a snapshot only when the subvolume it came from still
        // exists - the same rule the file backend uses
        let snapshot =
            info.parent_uuid != [0u8; 16] && inventory.iter().any(|r| r.uuid == info.parent_uuid);
        self.subvols.push(Subvol {
            id: info.treeid,
            uuid: info.uuid,
            parent_uuid: info.parent_uuid,
            flags: info.flags,
            otime: info.otime_sec,
            name: info.name.clone(),
            path: path.to_vec(),
            seen: true,
            snapshot,
        });
        (self.subvols.len() - 1) as u16
    }
}

fn open_dir(p: &[u8]) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(btrfs::O_DIRECTORY | btrfs::O_NOFOLLOW | btrfs::O_NOCTTY | btrfs::O_NONBLOCK)
        .open(std::ffi::OsStr::from_bytes(p))
}

/// Enumerate one subvolume's fs tree into a `Collected`.
///
/// `shard` is this walk's own accounting (in parallel, one per job);
/// `scope_inode`, when set, narrows the walk to that directory of the subvolume
/// and is what lets a scan rooted inside a subvolume ignore the rest of it.
/// `cancel` ends the walk at its next kernel search.
fn collect_subvolume(
    shard: &mut Shard,
    fd: i32,
    root_id: u64,
    subvol: u16,
    snap_class: bool,
    scope_inode: Option<u64>,
    live: Option<&Progress>,
    cancel: Option<&AtomicBool>,
) -> io::Result<Collected> {
    let scope = match scope_inode {
        Some(ino) => Some(scope_of(fd, root_id, ino)?),
        None => None,
    };
    let in_scope = |inode: u64| scope.as_ref().map(|s| s.contains(&inode)).unwrap_or(true);
    let mut col = Collected {
        subvol,
        recs: Vec::new(),
        entries: Vec::new(),
        names: Vec::new(),
        nested: Vec::new(),
    };
    let mut cur: Option<InodeRec> = None;
    let mut overflow = false;

    // INODE_ITEM then DIR_INDEX then EXTENT_DATA arrive for one inode before the
    // next inode starts, so a record can be finished and pushed as we go.
    let flush = |cur: &mut Option<InodeRec>, col: &mut Collected| {
        if let Some(rec) = cur.take() {
            col.recs.push(rec);
        }
    };

    btrfs::for_each_fs_item(fd, root_id, cancel, &mut |obj, ty, key_off, payload| {
        // records are in inode order, so a new objectid ends the previous one
        let same = cur.as_ref().map(|r| r.inode == obj).unwrap_or(false);
        if !same && ty == BTRFS_INODE_ITEM_KEY {
            flush(&mut cur, &mut col);
        }
        if !in_scope(obj) {
            return;
        }
        match ty {
            BTRFS_INODE_ITEM_KEY => {
                let Some(item) = btrfs::parse_inode_item(payload) else {
                    return;
                };
                if same {
                    return;
                }
                // Live counters for a GUI: one per inode, so the counts move as
                // the tree is read rather than only when a subvolume ends.
                if let Some(l) = live {
                    match item.mode & btrfs::S_IFMT {
                        btrfs::S_IFDIR => l.dirs.fetch_add(1, Ordering::Relaxed),
                        _ => l.files.fetch_add(1, Ordering::Relaxed),
                    };
                }
                cur = Some(InodeRec {
                    inode: obj,
                    mode: item.mode,
                    mtime: item.mtime_sec.min(u32::MAX as u64) as u32,
                    btime: if snap_class {
                        u32::MAX
                    } else {
                        item.otime_sec.min(u32::MAX as u64) as u32
                    },
                    btime_ns: if snap_class {
                        u16::MAX
                    } else {
                        (item.otime_nsec >> 14) as u16
                    },
                    ..Default::default()
                });
            }
            BTRFS_DIR_INDEX_KEY => {
                let Some(e) = btrfs::parse_dir_item(payload) else {
                    return;
                };
                let name_off = col.names.len() as u32;
                col.names.extend_from_slice(e.name);
                col.entries.push(Entry {
                    parent: obj,
                    child: e.child,
                    name_off,
                    name_len: e.name.len() as u16,
                    ft: e.file_type,
                    loc_type: e.location_type,
                });
            }
            BTRFS_EXTENT_DATA_KEY => {
                let Some(rec) = cur.as_mut() else { return };
                let Some(item) = btrfs::parse_file_extent(payload, key_off) else {
                    return;
                };
                if item.kind == BTRFS_FILE_EXTENT_INLINE {
                    // The payload lives in metadata, not in an extent: it is
                    // what the file claims, and nothing a subtree owns.
                    rec.referenced += item.ram_bytes;
                    return;
                }
                if item.disk_bytenr == 0 {
                    return; // hole
                }
                if let Some(l) = live {
                    l.refs.fetch_add(1, Ordering::Relaxed);
                }
                let page = (item.disk_bytenr >> 12) as u32;
                if (item.disk_bytenr >> 12) > u32::MAX as u64 {
                    overflow = true;
                    return;
                }
                if rec.first_ref == 0 && rec.nrefs == 0 {
                    rec.first_ref = shard.refs.len() as u32;
                }
                // The window is set later, from the references that reach the
                // shared block (`set_windows`): the inodes collected here are
                // candidates, not holders - only what build() places can hold.
                shard.extents.entry(page).or_insert_with(|| ExtentInfo {
                    disk: item.disk_num_bytes,
                    ram: item.ram_bytes,
                    win_btime: u32::MAX,
                    win_btime_ns: u16::MAX,
                    win_otime: u64::MAX,
                    win_count: 0,
                    assigned: 0,
                });
                shard.refs.push((page as u64) << 32);
                rec.nrefs += 1;
                rec.referenced += item.num_bytes;
            }
            _ => {}
        }
    })?;
    flush(&mut cur, &mut col);
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    if overflow {
        shard.warnings.push(format!(
            "subvolume {root_id}: extent above the 16 TiB addressing limit, not counted"
        ));
    }
    col.nested = btrfs::subvolumes_inside(fd, root_id)?;
    Ok(col)
}

// ---------------------------------------------------------------------------
// The extent tree's block-reference graph
// ---------------------------------------------------------------------------

/// Which subvolumes reference each metadata block, as far as the extent tree
/// records it.
///
/// A *data* extent's shared reference does not name a subvolume: it names the
/// tree block that holds the file extent. The subvolumes that reference that
/// extent are the subvolumes whose trees can reach that block, which is the
/// transitive closure over two kinds of reference the extent tree does record:
///
/// * `tree block backref root R` - a block in root `R`'s tree points here;
/// * `shared block backref parent P` - the block `P` points here, and `P`'s own
///   references say who can reach `P`.
///
/// The closure is a good approximation rather than a certainty, and
/// `--full-scan` is the exact answer - see the limits section of the README.
pub struct BlockRefs {
    /// block bytenr -> (start, len) into `refs`
    index: HashMap<u64, (u32, u32)>,
    /// kind 176 = "a block in root R's tree points here", 182 = "block P does"
    refs: Vec<(u8, u64)>,
    memo: HashMap<u64, Vec<u64>>,
}

impl BlockRefs {
    fn insert(&mut self, bytenr: u64, kind: u8, value: u64) {
        let e = self.index.entry(bytenr).or_insert((self.refs.len() as u32, 0));
        self.refs.push((kind, value));
        e.1 += 1;
    }

    /// Every root that can reach `bytenr`, memoised: the block graph is a DAG
    /// whose interior is small, so this runs once per distinct block.
    fn roots_of(&mut self, bytenr: u64) -> Vec<u64> {
        if let Some(v) = self.memo.get(&bytenr) {
            return v.clone();
        }
        let mut out: Vec<u64> = Vec::new();
        // an in-progress marker: a cycle would otherwise recurse forever
        self.memo.insert(bytenr, Vec::new());
        if let Some(&(start, len)) = self.index.get(&bytenr) {
            for i in start..start + len {
                let (kind, value) = self.refs[i as usize];
                match kind {
                    176 => out.push(value),
                    182 => {
                        if value != bytenr {
                            out.extend(self.roots_of(value));
                        }
                    }
                    _ => {}
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        self.memo.insert(bytenr, out.clone());
        out
    }
}

/// Read the block-reference graph: one pass over the extent tree, keeping the
/// references of every metadata extent.
fn collect_block_refs(
    fd: i32,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    g: &mut BlockRefs,
) -> io::Result<()> {
    btrfs::for_each_extent_tree_item(fd, cancel, &mut |bytenr, ty, key_off, payload| {
        match ty {
            btrfs::BTRFS_EXTENT_ITEM_KEY | btrfs::BTRFS_METADATA_ITEM_KEY => {
                let Some(h) = btrfs::parse_extent_item_hdr(payload) else {
                    return;
                };
                if !btrfs::is_metadata_extent(ty, &h) {
                    return;
                }
                btrfs::for_each_inline_ref(ty, payload, &mut |ty, body| {
                    let value = u64::from_le_bytes(body[..8].try_into().unwrap());
                    match ty {
                        btrfs::BTRFS_TREE_BLOCK_REF_KEY => g.insert(bytenr, 176, value),
                        btrfs::BTRFS_SHARED_BLOCK_REF_KEY => g.insert(bytenr, 182, value),
                        _ => {}
                    }
                });
            }
            // the same two, for blocks with more references than an item holds
            btrfs::BTRFS_TREE_BLOCK_REF_KEY => g.insert(bytenr, 176, key_off),
            btrfs::BTRFS_SHARED_BLOCK_REF_KEY => g.insert(bytenr, 182, key_off),
            _ => {}
        }
    })?;
    Ok(())
}

/// Record one extent's collapsed holders: the extent, and every snapshot-class
/// subvolume that references it.
fn flush_collapsed(
    bytenr: u64,
    len: u64,
    is_data: bool,
    holders: &mut Vec<u32>,
    extents: &mut ExtentMap,
    out: &mut Vec<(u32, u32)>,
    added: &mut u64,
    stats: &mut CollapseStats,
    named_in_scan: bool,
    named_gone: bool,
) {
    if holders.is_empty() {
        if is_data && !extents.contains_key(&((bytenr >> 12) as u32)) {
            // In the extent tree, referenced by something, and nothing resolved
            // to a subvolume this scan knows. When one of the named roots *is*
            // a subvolume of the scan, the walk would have counted those bytes
            // and this pass did not: a real loss. Otherwise the reference is
            // from outside the scanned tree, which the walk does not count
            // either.
            let bucket = if named_in_scan {
                &mut stats.lost
            } else if named_gone {
                &mut stats.gone
            } else {
                &mut stats.outside
            };
            bucket.0 += 1;
            bucket.1 += len;
        }
        return;
    }
    holders.sort_unstable();
    holders.dedup();
    let page = (bytenr >> 12) as u32;
    // An extent no walked tree references is new here, and the extent tree is
    // then the only place its size is known. `disk` is exact (the item's key
    // offset is the length); `ram` is not recorded there at all, so a
    // snapshot-only extent reports its compressed size as its uncompressed one.
    match extents.entry(page) {
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(ExtentInfo {
                disk: len,
                ram: len,
                win_btime: u32::MAX,
                win_btime_ns: u16::MAX,
                win_otime: u64::MAX,
                win_count: 0,
                assigned: 0,
            });
            *added += 1;
            stats.added.1 += len;
        }
        std::collections::hash_map::Entry::Occupied(_) => {
            // a walked subtree holds this too: the one case where a collapsed
            // subvolume's age has to be weighed against a file's
            stats.contested.0 += 1;
            stats.contested.1 += len;
        }
    }
    // The window is set later, from the references that reach the shared block
    // (`set_windows`), so nothing here has to weigh a collapsed subvolume's age
    // against a file's: the ownership pass does that from the keys themselves.
    for &n in holders.iter() {
        out.push((n, page));
    }
    holders.clear();
}

/// What one collapsed pass found, for the notes the scan prints and for the
/// limits section of the README.
#[derive(Default)]
pub struct CollapseStats {
    /// extents only snapshots hold, and their bytes
    pub added: (u64, u64),
    /// extents held by a walked subtree *and* by a collapsed subvolume
    pub contested: (u64, u64),
    /// extents that resolved to no subvolume of this scan although one of their
    /// named roots is in it: bytes the walk would have counted and this did not
    pub lost: (u64, u64),
    /// the same, for references from outside the scanned tree
    pub outside: (u64, u64),
    /// unresolved extents whose references name a subvolume that is not in the
    /// root tree any more - a snapshot that was pruned, whose leaves survive
    /// only in the snapshots that share them. The walk finds those extents in
    /// the snapshots; the extent tree cannot link them to any surviving one.
    pub gone: (u64, u64),
}

/// Attribute collapsed subvolumes' extents straight from the extent tree.
///
/// Extent-tree keys order by (bytenr, type, offset), so an extent's own item
/// and any reference that did not fit inline arrive together and in order: the
/// whole attribution streams in one pass, with no reference array to build and
/// nothing to sort by key afterwards.
fn collect_collapsed_refs(
    fd: i32,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    nodes: &HashMap<u64, u32>,
    in_scan: &HashSet<u64>,
    inventory: &HashSet<u64>,
    g: &mut BlockRefs,
    extents: &mut ExtentMap,
) -> io::Result<(Vec<(u32, u32)>, CollapseStats)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    let mut added = 0u64;
    let mut stats = CollapseStats::default();
    let mut holders: Vec<u32> = Vec::new();
    let mut cur_bytenr = u64::MAX;
    let mut cur_len = 0u64;
    let mut cur_data = false;
    let mut named_in_scan = false;
    let mut named_gone = false;
    // Unresolved extents whose references run through a block the kernel does
    // not trust, kept as (len, bytenr) for the budgeted repair below. That flag
    // is the general signal: it is exactly the case where the extent tree's ref
    // list is known to be incomplete, so a subvolume that holds the extent can
    // be missing from it.

    btrfs::for_each_extent_tree_item(fd, cancel, &mut |bytenr, ty, key_off, payload| {
        if bytenr != cur_bytenr {
            flush_collapsed(
                cur_bytenr, cur_len, cur_data, &mut holders, extents, &mut out,
                &mut added, &mut stats, named_in_scan, named_gone,
            );
            cur_bytenr = bytenr;
            cur_data = false;
            named_in_scan = false;
            named_gone = false;
            cur_len = key_off; // an EXTENT_ITEM's key offset is its length
        }
        match ty {
            btrfs::BTRFS_EXTENT_ITEM_KEY | btrfs::BTRFS_METADATA_ITEM_KEY => {
                let Some(h) = btrfs::parse_extent_item_hdr(payload) else {
                    return;
                };
                if btrfs::is_metadata_extent(ty, &h) {
                    return;
                }
                cur_data = true;
                btrfs::for_each_inline_ref(ty, payload, &mut |ty, body| match ty {
                    btrfs::BTRFS_EXTENT_DATA_REF_KEY => {
                        // the very first field is the referencing subvolume
                        let root = u64::from_le_bytes(body[..8].try_into().unwrap());
                        if in_scan.contains(&root) {
                            named_in_scan = true;
                        } else if !inventory.contains(&root) {
                            named_gone = true;
                        }
                        if let Some(&n) = nodes.get(&root) {
                            holders.push(n);
                        }
                    }
                    btrfs::BTRFS_SHARED_DATA_REF_KEY => {
                        // a shared reference names the tree block that holds the
                        // file extent; the subvolumes are the roots that reach it
                        if let Some(s) = btrfs::parse_shared_data_ref(body) {
                            for r in g.roots_of(s.parent) {
                                if in_scan.contains(&r) {
                                    named_in_scan = true;
                                } else if !inventory.contains(&r) {
                                    named_gone = true;
                                }
                                if let Some(&n) = nodes.get(&r) {
                                    holders.push(n);
                                }
                            }
                        }
                    }
                    _ => {}
                });
            }
            // references that did not fit in the extent item live as their own
            // items at the same bytenr
            btrfs::BTRFS_EXTENT_DATA_REF_KEY => {
                if payload.len() >= 28 {
                    let root = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                    if in_scan.contains(&root) {
                        named_in_scan = true;
                    } else if !inventory.contains(&root) {
                        named_gone = true;
                    }
                    if let Some(&n) = nodes.get(&root) {
                        holders.push(n);
                    }
                }
            }
            btrfs::BTRFS_SHARED_DATA_REF_KEY => {
                // keyed (bytenr, type, parent): the parent block is the offset
                for r in g.roots_of(key_off) {
                    if in_scan.contains(&r) {
                        named_in_scan = true;
                    } else if !inventory.contains(&r) {
                        named_gone = true;
                    }
                    if let Some(&n) = nodes.get(&r) {
                        holders.push(n);
                    }
                }
            }
            _ => {}
        }
    })?;
    flush_collapsed(
        cur_bytenr, cur_len, cur_data, &mut holders, extents, &mut out, &mut added,
        &mut stats, named_in_scan, named_gone,
    );
    stats.added.0 = added;

    Ok((out, stats))
}

/// Where a scan starts: one entry per path given on the command line.
pub struct Root {
    pub path: Vec<u8>,
    pub subvol: u16,
    pub inode: u64,
    pub info: btrfs::SubvolInfo,
}

/// Assemble the tree the rest of the program expects from the collected pieces.
///
/// Node indices come out in the same order the file backend produces: breadth
/// first, children sorted by name. That is what makes the two comparable at all.
/// Append the single node that stands in for a collapsed subvolume.
///
/// It is a directory node with no children: the scan knows the subvolume is
/// there and how much it holds, but not what is inside it. Its `btime` is the
/// maximum, which is the truthful statement that the ages of the files inside
/// are unknown - the effect is that it loses every tie against a walked holder
/// and only wins extents no walked subvolume references, which is its share of
/// the data its snapshots keep alive.
fn push_collapsed_node(
    tree: &mut Tree,
    c: &mut Collector,
    subvol: u16,
    parent_node: u32,
    name: &[u8],
    depth: u16,
) -> u32 {
    let idx = tree.nodes.len() as u32;
    let s = &tree.subvols[subvol as usize];
    let mut flags = FLAG_SUBVOL | FLAG_COLLAPSED | FLAG_SNAP_CLASS;
    if s.is_snapshot() {
        flags |= FLAG_SNAPSHOT;
    }
    if s.is_readonly() {
        flags |= FLAG_READONLY;
    }
    let name_off = tree.names.len() as u32;
    tree.names.extend_from_slice(name);
    let extra = tree.dirs.len() as u32;
    tree.dirs.push(DirExtra {
        subvol,
        depth,
        ..Default::default()
    });
    c.dirs += 1;
    // The age this node carries into the ownership rule: none. Its birth time
    // is "unknown", which loses against every walked file, and the subvolume's
    // creation time rides in the third field so that snapshots tie with each
    // other and the oldest one is the custodian. A snapshot owns what the live
    // trees let go of, not what they still hold - its files' own birth times
    // are not a fact about the data at all (a reflink copy and a rename are
    // enough to hand a live file a younger inode), and it is the one thing the
    // extent tree cannot supply for a collapsed subvolume.
    tree.nodes.push(Node {
        parent: parent_node,
        name_off,
        name_len: name.len() as u16,
        kind: KIND_DIR,
        flags,
        extra,
        btime: u32::MAX,
        btime_ns: u16::MAX,
        ..Default::default()
    });
    idx
}

fn build(
    c: &mut Collector,
    roots: &[Root],
    excludes: &[Vec<u8>],
) -> (Tree, Vec<u64>, HashMap<u64, u32>) {
    // `--exclude` is a path prefix, as in the file walker: an excluded node is
    // not placed and its subtree is not descended, so its references never
    // enter the map and extents it shares look exclusive to what remains. The
    // paths are only built when there is something to compare against.
    let filter = !excludes.is_empty();
    let mut tree = Tree {
        nodes: Vec::new(),
        names: Vec::new(),
        dirs: Vec::new(),
        roots: Vec::new(),
        root: 0,
        subvols: c.subvols.clone(),
        inventory: Vec::new(),
        warnings: c.shard.warnings.clone(),
        stats: ScanStats::default(),
        scanned_at: 0,
        scanned_roots: Vec::new(),
    };
    // synthetic root
    tree.nodes.push(Node {
        parent: NO_NODE,
        kind: KIND_DIR,
        flags: FLAG_ROOT,
        ..Default::default()
    });
    tree.dirs.push(DirExtra::default());

    let mut final_refs: Vec<u64> = Vec::with_capacity(c.shard.refs.len());
    // (collected index, inode, parent node, name, flags) worklist for the BFS
    let mut queue: VecDeque<(usize, u64, u32, Vec<u8>, u8)> = VecDeque::new();

    for root in roots {
        // the root node itself: a real directory inside the collected subvolume
        let Some(ci) = c.subs.iter().position(|s| s.subvol == root.subvol) else {
            continue;
        };
        // name and mtime of the root path come from its own inode record
        let name = root
            .path
            .rsplit(|b| *b == b'/')
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or(&root.path)
            .to_vec();
        let mut flags = FLAG_SUBVOL;
        let s = &tree.subvols[root.subvol as usize];
        if s.is_snapshot() {
            flags |= FLAG_SNAPSHOT;
        }
        if s.is_readonly() {
            flags |= FLAG_READONLY;
        }
        queue.push_back((ci, root.inode, 0, name, flags));
    }

    // children of a directory are a contiguous run of (parent, name)-sorted
    // entries, so no parent->children map is needed
    for col in c.subs.iter_mut() {
        let names = &col.names;
        col.entries.sort_by(|a, b| {
            (a.parent, &names[a.name_off as usize..a.name_off as usize + a.name_len as usize])
                .cmp(&(b.parent, &names[b.name_off as usize..b.name_off as usize + b.name_len as usize]))
        });
    }
    let mut root_nodes: Vec<u32> = Vec::new();
    // root id -> the node that stands for it
    let mut collapsed_nodes: HashMap<u64, u32> = HashMap::new();

    // A directory must own a *contiguous* run of nodes, because everything
    // downstream (children slices, the rollup) assumes it. So each directory is
    // visited once and its children appended as one batch, then the child
    // directories are visited in turn - which is the order the file backend's
    // BFS produces, children sorted by name.
    // (collected subvolume, inode, node, path - the last empty unless filtering)
    let mut dirs: VecDeque<(usize, u64, u32, Vec<u8>)> = VecDeque::new();
    // synthetic root's children: the paths given on the command line
    let mut root_queue: Vec<(usize, u64, Vec<u8>, u8, Vec<u8>)> = Vec::new();
    for root in roots {
        let Some(ci) = c.subs.iter().position(|s| s.subvol == root.subvol) else {
            continue;
        };
        let name = root
            .path
            .rsplit(|b| *b == b'/')
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or(&root.path)
            .to_vec();
        let mut flags = FLAG_SUBVOL;
        let s = &tree.subvols[root.subvol as usize];
        if s.is_snapshot() {
            flags |= FLAG_SNAPSHOT;
        }
        if s.is_readonly() {
            flags |= FLAG_READONLY;
        }
        let path = if filter { root.path.clone() } else { Vec::new() };
        root_queue.push((ci, root.inode, name, flags, path));
    }

    // helper closure: append one node
    fn push_node(
        tree: &mut Tree,
        c: &mut Collector,
        ci: usize,
        parent_node: u32,
        name: &[u8],
        mut flags: u8,
        rec: InodeRec,
        kind: u8,
        final_refs: &mut Vec<u64>,
    ) -> u32 {
        let idx = tree.nodes.len() as u32;
        if c.subs[ci].subvol_snapshot(&tree.subvols) {
            flags |= FLAG_SNAP_CLASS;
        }
        let a = rec.first_ref as usize;
        let b = a + rec.nrefs as usize;
        let first_ref = final_refs.len() as u32;
        for r in &c.shard.refs[a..b] {
            final_refs.push(*r | idx as u64); // low half: owner node index
        }
        if kind == KIND_DIR {
            c.dirs += 1;
        } else {
            c.files += 1;
        }
        let name_off = tree.names.len() as u32;
        tree.names.extend_from_slice(name);
        tree.nodes.push(Node {
            parent: parent_node,
            name_off,
            name_len: name.len() as u16,
            kind,
            flags,
            mtime: rec.mtime,
            btime: rec.btime,
            btime_ns: rec.btime_ns,
            referenced: rec.referenced,
            first_ref,
            nrefs: rec.nrefs,
            ..Default::default()
        });
        idx
    }

    // append the roots as the synthetic root's batch
    {
        let first = tree.nodes.len() as u32;
        for (ci, inode, name, flags, root_path) in root_queue {
            let Ok(pos) = c.subs[ci].recs.binary_search_by_key(&inode, |r| r.inode) else {
                continue;
            };
            let rec = c.subs[ci].recs[pos];
            let kind = match rec.mode & btrfs::S_IFMT {
                btrfs::S_IFDIR => KIND_DIR,
                btrfs::S_IFREG => KIND_FILE,
                _ => continue,
            };
            let idx = push_node(&mut tree, c, ci, 0, &name, flags, rec, kind, &mut final_refs);
            root_nodes.push(idx);
            if kind == KIND_DIR {
                let extra = tree.dirs.len() as u32;
                tree.dirs.push(DirExtra {
                    subvol: c.subs[ci].subvol,
                    depth: 1,
                    ..Default::default()
                });
                tree.nodes[idx as usize].extra = extra;
                let p = if filter { root_path.clone() } else { Vec::new() };
                dirs.push_back((ci, inode, idx, p));
            }
        }
        let n = tree.nodes.len() as u32 - first;
        let node = &mut tree.nodes[0];
        node.first_child = first;
        node.nchildren = n;
        tree.roots = root_nodes.clone();
    }

    while let Some((ci, inode, node_idx, dir_path)) = dirs.pop_front() {
        let first_child = tree.nodes.len() as u32;
        // gather this directory's children first: pushing nodes borrows the
        // collector mutably, so nothing may hold a borrow of its entries
        let mut kids: Vec<(usize, u64, Vec<u8>, u8, Option<u16>, Vec<u8>)> = Vec::new();
        {
            let entries = &c.subs[ci].entries;
            let names = &c.subs[ci].names;
            let lo = entries.partition_point(|e| e.parent < inode);
            let hi = entries.partition_point(|e| e.parent <= inode);
            for e in &entries[lo..hi] {
            let child_name = names[e.name_off as usize..e.name_off as usize + e.name_len as usize].to_vec();
            let (child_ci, child_inode, child_flags, collapsed) = if e.loc_type == 132 {
                // a directory entry pointing at another subvolume's root
                let nested = c.subs[ci]
                    .nested
                    .iter()
                    .find(|(_, dirid, name)| *dirid == inode && name.as_slice() == child_name.as_slice());
                match nested {
                    Some((root_id, _, _)) => {
                        // a collapsed subvolume has no collected tree: its node
                        // is a single directory standing for the whole subtree
                        if let Some(&(_, subvol, _)) =
                            c.collapsed.iter().find(|(r, _, _)| r == root_id)
                        {
                            let child_path = if filter {
                                let mut p = dir_path.clone();
                                if !p.ends_with(b"/") {
                                    p.push(b'/');
                                }
                                p.extend_from_slice(&child_name);
                                if excluded(&p, excludes) {
                                    continue;
                                }
                                p
                            } else {
                                Vec::new()
                            };
                            kids.push((usize::MAX, 0, child_name, 0, Some(subvol), child_path));
                            continue;
                        }
                        match c
                            .subs
                            .iter()
                            .position(|s| tree.subvols[s.subvol as usize].id == *root_id)
                        {
                            Some(child_ci) => {
                                let s = &tree.subvols[c.subs[child_ci].subvol as usize];
                                let mut f = FLAG_SUBVOL;
                                if s.is_snapshot() {
                                    f |= FLAG_SNAPSHOT;
                                }
                                if s.is_readonly() {
                                    f |= FLAG_READONLY;
                                }
                                (child_ci, btrfs::BTRFS_FIRST_FREE_OBJECTID, f, None)
                            }
                            None => continue,
                        }
                    }
                    None => continue,
                }
            } else {
                (ci, e.child, 0, None)
            };
            let child_path = if filter {
                let mut p = dir_path.clone();
                if !p.ends_with(b"/") {
                    p.push(b'/');
                }
                p.extend_from_slice(&child_name);
                if excluded(&p, excludes) {
                    continue;
                }
                p
            } else {
                Vec::new()
            };
            kids.push((child_ci, child_inode, child_name, child_flags, collapsed, child_path));
        }
        }
        for (child_ci, child_inode, child_name, child_flags, collapsed, child_path) in kids {
            // depth comes from the parent, which is already placed
            let depth = if node_idx == 0 {
                1
            } else {
                tree.dirs[tree.nodes[node_idx as usize].extra as usize].depth + 1
            };
            if let Some(subvol) = collapsed {
                let idx = push_collapsed_node(&mut tree, c, subvol, node_idx, &child_name, depth);
                let root_id = tree.subvols[subvol as usize].id;
                collapsed_nodes.insert(root_id, idx);
                // A subvolume the root tree says lives inside this collapsed one
                // had its own tree read, so its node belongs here. The chain of
                // directories between the two is not read - that is the point
                // of collapsing - so it hangs directly off the collapsed node.
                let kids = c.graph_kids.clone();
                let first = tree.nodes.len() as u32;
                for (p_root, child_subvol, gname) in kids {
                    if p_root != root_id {
                        continue;
                    }
                    let Some(child_ci) =
                        c.subs.iter().position(|s| s.subvol == child_subvol)
                    else {
                        continue;
                    };
                    let sv = &tree.subvols[child_subvol as usize];
                    let mut f = FLAG_SUBVOL;
                    if sv.is_snapshot() {
                        f |= FLAG_SNAPSHOT;
                    }
                    if sv.is_readonly() {
                        f |= FLAG_READONLY;
                    }
                    let inode = btrfs::BTRFS_FIRST_FREE_OBJECTID;
                    let Ok(pos) = c.subs[child_ci].recs.binary_search_by_key(&inode, |r| r.inode)
                    else {
                        continue;
                    };
                    let rec = c.subs[child_ci].recs[pos];
                    let gidx = push_node(
                        &mut tree, c, child_ci, idx, &gname, f, rec, KIND_DIR,
                        &mut final_refs,
                    );
                    let gd = tree.dirs.len() as u32;
                    tree.dirs.push(DirExtra {
                        subvol: c.subs[child_ci].subvol,
                        depth: depth + 1,
                        ..Default::default()
                    });
                    tree.nodes[gidx as usize].extra = gd;
                    let gp = if filter {
                        let mut p = child_path.clone();
                        if !p.ends_with(b"/") {
                            p.push(b'/');
                        }
                        p.extend_from_slice(&gname);
                        p
                    } else {
                        Vec::new()
                    };
                    dirs.push_back((child_ci, inode, gidx, gp));
                }
                let n = tree.nodes.len() as u32 - first;
                tree.nodes[idx as usize].first_child = first;
                tree.nodes[idx as usize].nchildren = n;
                continue;
            }
            let Ok(pos) = c.subs[child_ci].recs.binary_search_by_key(&child_inode, |r| r.inode) else {
                continue;
            };
            let rec = c.subs[child_ci].recs[pos];
            let kind = match rec.mode & btrfs::S_IFMT {
                btrfs::S_IFDIR => KIND_DIR,
                btrfs::S_IFREG => KIND_FILE,
                _ => continue, // symlinks and specials, as in the file backend
            };
            let idx = push_node(&mut tree, c, child_ci, node_idx, &child_name, child_flags, rec, kind, &mut final_refs);
            if kind == KIND_DIR {
                let extra = tree.dirs.len() as u32;
                tree.dirs.push(DirExtra {
                    subvol: c.subs[child_ci].subvol,
                    depth,
                    ..Default::default()
                });
                tree.nodes[idx as usize].extra = extra;
                dirs.push_back((child_ci, child_inode, idx, child_path));
            }
        }
        let n = tree.nodes.len() as u32 - first_child;
        let node = &mut tree.nodes[node_idx as usize];
        node.first_child = first_child;
        node.nchildren = n;
    }

    (tree, final_refs, collapsed_nodes)
}

impl Collected {
    fn subvol_snapshot(&self, table: &[Subvol]) -> bool {
        table[self.subvol as usize].is_snapshot()
    }
}

/// Build a path to a directory *inside* a collected subvolume: the path of a
/// nested subvolume is its containing subvolume's path, plus the chain of
/// names from that subvolume's root down to the directory holding it, plus the
/// entry's own name. Without this, a snapshot like
/// /home/.snapshots/457/snapshot - whose entry lives in a subdirectory of the
/// .snapshots subvolume - cannot be found.
fn path_to_dir(col: &Collected, dirid: u64, name: &[u8], root_path: &[u8]) -> Vec<u8> {
    let mut chain: Vec<&[u8]> = Vec::new();
    let mut cur = dirid;
    while cur != btrfs::BTRFS_FIRST_FREE_OBJECTID {
        // the entry whose child is `cur` gives its name and its parent
        let mut found = None;
        for e in &col.entries {
            if e.child == cur {
                found = Some((
                    e.parent,
                    &col.names[e.name_off as usize..e.name_off as usize + e.name_len as usize],
                ));
                break;
            }
        }
        let Some((parent, n)) = found else { break };
        chain.push(n);
        cur = parent;
    }
    chain.reverse();
    let mut out = root_path.to_vec();
    for c in chain {
        if !out.ends_with(b"/") {
            out.push(b'/');
        }
        out.extend_from_slice(c);
    }
    if !out.ends_with(b"/") {
        out.push(b'/');
    }
    out.extend_from_slice(name);
    out
}

// ---------------------------------------------------------------------------
// The walks, on one thread or several
// ---------------------------------------------------------------------------

/// One fs-tree walk, as handed to a worker thread. The walk needs the root id
/// (its fs tree), whether the subvolume is snapshot-class (its reference keys)
/// and, for a scan rooted at a directory rather than at a subvolume root, that
/// directory - everything else it records comes back with the result.
struct Job {
    root_id: u64,
    subvol: u16,
    snap_class: bool,
    scope_inode: Option<u64>,
}

/// A finished walk: the tree it collected, the shard it wrote, and how long
/// the walk itself took (which is what CST_TIME_SUBVOL prints).
type JobResult = io::Result<(Collected, Shard, std::time::Duration)>;

/// Walks in flight, and the results workers park for the coordinator.
///
/// Only the *walks* are parallel. Which subvolume is walked next, how it is
/// interned, under which path and where a nested one hangs are decided on the
/// coordinator thread, in the same order a serial scan decides them, because
/// those decisions are order-dependent: a subvolume reachable through two
/// parents keeps the path of the discovery that came first, and a subvolume
/// found through the root tree hangs under whichever collapsed node reached it
/// first. Peers doing the discovery too would make both depend on which thread
/// arrived first - the scheduling-order leak that must not reach the cache.
struct Pool {
    state: Mutex<PoolState>,
    /// a job is waiting
    work: Condvar,
    /// a result is waiting
    done: Condvar,
    /// set when the scan gives up: a walk in flight ends at its next kernel
    /// search, since the `Searcher` polls this like a cancel
    stop: AtomicBool,
}

#[derive(Default)]
struct PoolState {
    /// jobs no worker has taken. Workers take the newest, which is the one the
    /// coordinator - whose work list is a stack - will ask for next.
    queue: VecDeque<Job>,
    /// finished walks by root id, waiting to be taken back in traversal order
    results: HashMap<u64, JobResult>,
    /// submitted and not finished: a `take` with neither a result nor anything
    /// in flight is a bug, not something to wait for
    inflight: usize,
    shutdown: bool,
}

impl Pool {
    fn new() -> Pool {
        Pool {
            state: Mutex::new(PoolState::default()),
            work: Condvar::new(),
            done: Condvar::new(),
            stop: AtomicBool::new(false),
        }
    }

    fn submit(&self, job: Job) {
        {
            let mut st = self.state.lock().unwrap();
            st.queue.push_back(job);
            st.inflight += 1;
        }
        self.work.notify_one();
    }

    /// The result of one walk, waiting for a worker to finish it.
    fn take(&self, root_id: u64, cancel: Option<&AtomicBool>) -> JobResult {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(r) = st.results.remove(&root_id) {
                return r;
            }
            if st.inflight == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "internal: no walk was in flight for this subvolume",
                ));
            }
            // Bounded, so a cancel set by a GUI is seen even while a long walk
            // is running: the walkers then stop at their next kernel search.
            let (next, _) = self
                .done
                .wait_timeout(st, Duration::from_millis(20))
                .unwrap();
            st = next;
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                self.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut st = self.state.lock().unwrap();
        st.shutdown = true;
        drop(st);
        self.work.notify_all();
        self.done.notify_all();
    }
}

/// One worker: take a job, walk its tree, park the result.
fn pool_worker(pool: &Pool, fd: i32, live: Option<&Progress>) {
    loop {
        let job = {
            let mut st = pool.state.lock().unwrap();
            loop {
                if st.shutdown {
                    return;
                }
                if let Some(j) = st.queue.pop_back() {
                    break j;
                }
                st = pool.work.wait(st).unwrap();
            }
        };
        let started = std::time::Instant::now();
        let mut shard = Shard::default();
        let r = collect_subvolume(
            &mut shard,
            fd,
            job.root_id,
            job.subvol,
            job.snap_class,
            job.scope_inode,
            live,
            Some(&pool.stop),
        );
        let walk = started.elapsed();
        let mut st = pool.state.lock().unwrap();
        st.inflight -= 1;
        st.results.insert(job.root_id, r.map(|col| (col, shard, walk)));
        drop(st);
        pool.done.notify_all();
    }
}

/// Merge one walk's shard into the collector's.
///
/// The walk recorded `first_ref`/`nrefs` against its own reference array, so
/// every index moves by that array's new base. The extent values are
/// properties of the extent itself (`disk`/`ram` are the same whichever file
/// reached it), so the entry already in the map is the right one. The merge
/// happens in traversal order, so the merged arrays come out exactly as a
/// serial scan built them, whatever the thread count.
fn merge_shard(dst: &mut Shard, col: &mut Collected, src: Shard) {
    let base = dst.refs.len() as u32;
    if base != 0 {
        for rec in col.recs.iter_mut() {
            if rec.nrefs > 0 {
                rec.first_ref += base;
            }
        }
    }
    dst.refs.extend_from_slice(&src.refs);
    dst.warnings.extend(src.warnings);
    for (page, info) in src.extents {
        dst.extents.entry(page).or_insert(info);
    }
}

/// Collect one subvolume, then every subvolume nested inside it.
///
/// The walks run on `threads` workers; the traversal is one thread's, so the
/// scan it produces does not depend on how many.
fn collect_all(
    c: &mut Collector,
    fd: i32,
    inventory: &[btrfs::RootItem],
    start: &Root,
    excludes: &[Vec<u8>],
    live: Option<&Progress>,
    threads: usize,
) -> io::Result<()> {
    if threads <= 1 {
        // the serial path, unchanged: one thread, no copies, no merges
        let pool = Pool::new();
        return collect_walk(c, fd, inventory, start, excludes, live, &pool, false);
    }
    // Declared outside the scope so the workers' borrow of it outlives them.
    let pool = Pool::new();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| pool_worker(&pool, fd, live));
        }
        let r = collect_walk(c, fd, inventory, start, excludes, live, &pool, true);
        pool.shutdown();
        r
    })
}

/// The traversal: pop a subvolume, take its walk (or run it), discover what is
/// nested inside it, and push that back on the list.
fn collect_walk(
    c: &mut Collector,
    fd: i32,
    inventory: &[btrfs::RootItem],
    start: &Root,
    excludes: &[Vec<u8>],
    live: Option<&Progress>,
    pool: &Pool,
    parallel: bool,
) -> io::Result<()> {
    // (root id, subvol index, path, whether to read its fs tree). A collapsed
    // subvolume is still *visited*, to find what lives inside it - just not
    // read.
    let mut work: Vec<(u64, u16, Vec<u8>, bool)> =
        vec![(start.info.treeid, start.subvol, start.path.clone(), true)];
    let mut done: Vec<u64> = Vec::new();
    // root ids handed to the pool: a subvolume reached twice is walked once
    let mut submitted: HashSet<u64> = HashSet::new();
    let cancel = live.map(|l| &l.cancel);
    while let Some((root_id, subvol, path, read_tree)) = work.pop() {
        // A GUI can cancel between subvolumes; the reads themselves cannot be
        // interrupted, since the kernel answers each fs-tree search whole.
        if let Some(l) = live {
            if l.cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let mut cur = l.current.lock().unwrap();
            cur.clear();
            cur.extend_from_slice(&path[..path.len().min(160)]);
        }
        if done.contains(&root_id) {
            continue;
        }
        done.push(root_id);
        if !read_tree {
            // A collapsed subvolume's directories are never read, so its
            // children come from the root tree instead: every subvolume
            // reference is a ROOT_BACKREF keyed (child, ROOT_BACKREF, parent)
            // carrying the entry's dirid and name, so one search per collapsed
            // root yields them all with no directory reading at all - the way
            // `btrfs subvolume list -o` answers the same question.
            //
            // They matter: a container layer can hold a mock root made with
            // `dnf --installroot`, which is not a snapshot of anything and has
            // a tree and files of its own. Skipping it loses its extents, since
            // nothing else can attribute them - the extent tree names a
            // subvolume the scan would never have interned.
            for (child_root, _dirid, name) in btrfs::subvolumes_inside(fd, root_id)? {
                // Only what this scan has not already accounted for. A snapshot
                // of a subvolume holds copies of that subvolume's directory
                // entries, so the root tree lists everything nested in the
                // original as a child of every snapshot of it too - and those
                // are the same subvolumes, already walked or collapsed from
                // where they really live. Taking them again would hang each
                // subtree under every snapshot.
                if c.subvols.iter().any(|s| s.id == child_root) {
                    continue;
                }
                let Some(r) = inventory.iter().find(|r| r.treeid == child_root) else {
                    continue;
                };
                let mut child_path = path.clone();
                if !child_path.ends_with(b"/") {
                    child_path.push(b'/');
                }
                child_path.extend_from_slice(&name);
                if excluded(&child_path, excludes) {
                    continue;
                }
                let child = c.intern_root(r, inventory, &child_path, &name);
                let collapsed = c.collapse(child);
                if collapsed {
                    c.collapsed.push((child_root, child, child_path.clone()));
                } else {
                    c.graph_kids.push((root_id, child, name.clone()));
                    if parallel && submitted.insert(child_root) {
                        pool.submit(Job {
                            root_id: child_root,
                            subvol: child,
                            snap_class: c.subvols[child as usize].is_snapshot(),
                            // a child of a collapsed node is not the scan root
                            scope_inode: None,
                        });
                    }
                }
                work.push((child_root, child, child_path, !collapsed));
            }
            continue;
        }
        // Which subvolume a scan started in is a question about the subvolume,
        // not about the spelling of the path it was given.
        let scope_inode =
            if root_id == start.info.treeid && start.inode != btrfs::BTRFS_FIRST_FREE_OBJECTID {
                Some(start.inode)
            } else {
                None
            };
        let snap_class = c.subvols[subvol as usize].is_snapshot();
        let (col, walk) = if parallel {
            if submitted.insert(root_id) {
                pool.submit(Job { root_id, subvol, snap_class, scope_inode });
            }
            let (mut col, shard, walk) = pool.take(root_id, cancel)?;
            merge_shard(&mut c.shard, &mut col, shard);
            (col, walk)
        } else {
            let t_sub = std::time::Instant::now();
            let col = collect_subvolume(
                &mut c.shard, fd, root_id, subvol, snap_class, scope_inode, live, cancel,
            )?;
            (col, t_sub.elapsed())
        };
        if std::env::var_os("CST_TIME_SUBVOL").is_some() {
            let s = &c.subvols[subvol as usize];
            eprintln!(
                "[time]   subvol {} ({}) walked in {:.2}s: {} inodes, {} entries",
                s.id,
                String::from_utf8_lossy(&path),
                walk.as_secs_f64(),
                col.recs.len(),
                col.entries.len()
            );
        }
        for (child_root, dirid, name) in &col.nested {
            // the nested subvolume is reachable as a directory of this one, so
            // its identity is one open() away - no need to decode root items
            let child_path = path_to_dir(&col, *dirid, name, &path);
            if excluded(&child_path, excludes) {
                continue;
            }
            let Ok(child) = open_dir(&child_path) else { continue };
            let Ok(info) = btrfs::get_subvol_info(std::os::fd::AsRawFd::as_raw_fd(&child)) else {
                continue;
            };
            if info.treeid != *child_root {
                continue;
            }
            let child_subvol = c.intern_subvol(&info, inventory, &child_path);
            let collapsed = c.collapse(child_subvol);
            if collapsed {
                c.collapsed.push((*child_root, child_subvol, child_path.clone()));
            } else if parallel && submitted.insert(*child_root) {
                pool.submit(Job {
                    root_id: *child_root,
                    subvol: child_subvol,
                    snap_class: c.subvols[child_subvol as usize].is_snapshot(),
                    scope_inode: None,
                });
            }
            work.push((*child_root, child_subvol, child_path, !collapsed));
        }
        c.subs.push(col);
    }
    Ok(())
}

/// Would this path be skipped? `--exclude` is a prefix match, as in the file
/// backend: the excluded subtree is not walked, so extents shared with it look
/// exclusive against what remains.
fn excluded(path: &[u8], excludes: &[Vec<u8>]) -> bool {
    excludes.iter().any(|ex| path.starts_with(ex))
}

/// The key `attribute_owned` compares an extent's window against, for one node.
/// Mirrors model.rs exactly - the window is now built from it (see
/// `set_windows`), so a drift here would misattribute bytes silently.
/// `(node btime, node btime_ns, the subvolume's creation time)`.
fn node_key(tree: &Tree, i: u32, subvol_otime: &[u64]) -> (u32, u16, u64, u16) {
    let n = &tree.nodes[i as usize];
    let collapsed = n.flags & FLAG_COLLAPSED != 0;
    let subvol = if collapsed {
        tree.dirs[n.extra as usize].subvol
    } else {
        let p = &tree.nodes[n.parent as usize];
        if p.kind == KIND_DIR {
            tree.dirs[p.extra as usize].subvol
        } else {
            0
        }
    };
    (
        n.btime,
        n.btime_ns,
        *subvol_otime.get(subvol as usize).unwrap_or(&0),
        subvol,
    )
}

/// Give every extent the window its holders set: the smallest key among the
/// references that reached the shared block, and how many of them carry it.
///
/// It has to be derived from these references and not from the inodes the walk
/// collected. An extent is split between the references the ownership pass can
/// see, so a holder that never became a node - an unlinked inode, a file whose
/// directory entry is gone, anything the walk saw but build() did not place -
/// must not set a window either. Otherwise its key is the window, no reference
/// in the block matches it, and the placed holders' shares are credited to
/// nobody.
fn set_windows(tree: &Tree, refs: &[u64], extents: &mut ExtentMap, subvol_otime: &[u64]) {
    for r in refs {
        let Some(e) = extents.get_mut(&((*r >> 32) as u32)) else {
            continue;
        };
        let (btime, btime_ns, ctime, _) = node_key(tree, *r as u32, subvol_otime);
        let key = (btime, btime_ns, ctime);
        let best = (e.win_btime, e.win_btime_ns, e.win_otime);
        if key < best || e.win_count == 0 {
            e.win_btime = btime;
            e.win_btime_ns = btime_ns;
            e.win_otime = ctime;
            e.win_count = 1;
        } else if key == best {
            e.win_count += 1;
        }
    }
}

/// Development aid for the ownership pass: after `attribute_owned` has run,
/// `e.assigned` is how many (node, reference) pairs matched the extent's
/// window - zero means the extent is in the map, is referenced, and is owned by
/// nobody. Counts those, and prints the window and the referencing nodes'
/// keys for the bytenrs named in CST_DEBUG_BYTENR / CST_DEBUG_BYTENR_FILE.
fn debug_ownership(
    tree: &Tree,
    extents: &ExtentMap,
    ref_blocks: &[Vec<u64>],
    subvol_otime: &[u64],
) {
    let mut wanted: HashSet<u32> = HashSet::new();
    if let Ok(list) = std::env::var("CST_DEBUG_BYTENR") {
        for t in list.split(',') {
            if let Ok(b) = t.trim().parse::<u64>() {
                wanted.insert((b >> 12) as u32);
            }
        }
    }
    if let Ok(path) = std::env::var("CST_DEBUG_BYTENR_FILE") {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if let Ok(b) = line.trim().parse::<u64>() {
                    wanted.insert((b >> 12) as u32);
                }
            }
        }
    }

    // node index per page, for the pages asked about
    let mut refs_of: HashMap<u32, Vec<u32>> = HashMap::new();
    // every page any node references at all
    let mut referenced: HashSet<u32> = HashSet::new();
    for r in ref_blocks[0].iter() {
        let page = (*r >> 32) as u32;
        referenced.insert(page);
        if wanted.contains(&page) {
            refs_of.entry(page).or_default().push(*r as u32);
        }
    }

    let mut unclaimed: Vec<&u32> = Vec::new();
    let mut unclaimed_bytes = 0u64;
    let mut no_refs: Vec<&u32> = Vec::new();
    let mut no_refs_bytes = 0u64;
    for (p, e) in extents.iter() {
        if e.assigned != 0 {
            continue;
        }
        if referenced.contains(p) {
            unclaimed.push(p);
            unclaimed_bytes += e.disk;
        } else {
            no_refs.push(p);
            no_refs_bytes += e.disk;
        }
    }
    eprintln!(
        "[own] extents {} ({} referenced):\n\
         [own]   owned by nobody, referenced by a node: {} extents, {:.2} GiB\n\
         [own]   owned by nobody, referenced by no node: {} extents, {:.2} GiB",
        extents.len(),
        referenced.len(),
        unclaimed.len(),
        unclaimed_bytes as f64 / 1073741824.0,
        no_refs.len(),
        no_refs_bytes as f64 / 1073741824.0
    );
    let mut unclaimed_sorted: Vec<u32> = unclaimed.iter().map(|p| **p).collect();
    unclaimed_sorted.sort_unstable();
    if !unclaimed_sorted.is_empty() {
        let show: Vec<u64> = unclaimed_sorted
            .iter()
            .take(8)
            .map(|p| (*p as u64) << 12)
            .collect();
        eprintln!("[own]   smallest unowned bytenrs: {show:?}");
    }
    if let Ok(path) = std::env::var("CST_DEBUG_OWN_DUMP") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::File::create(&path) {
            let mut buf = String::new();
            for p in unclaimed_sorted.iter() {
                buf.push_str(&format!("{}\n", (*p as u64) << 12));
            }
            let _ = f.write_all(buf.as_bytes());
            eprintln!("[own]   wrote {} unowned bytenrs to {path}", unclaimed_sorted.len());
        }
    }

    // Detail a few unowned extents, preferring ones also named in the wanted
    // list (e.g. the bytenrs a full-scan-minus-collapsed diff produced).
    let mut detail: Vec<u32> = Vec::new();
    for p in unclaimed_sorted.iter() {
        if wanted.contains(p) && detail.len() < 5 {
            detail.push(*p);
        }
    }
    for p in unclaimed_sorted.iter() {
        if detail.len() < 8 && !detail.contains(p) {
            detail.push(*p);
        }
    }
    if detail.is_empty() {
        // no referenced-but-unowned extents here: show the unreferenced ones
        let mut nr: Vec<u32> = no_refs.iter().map(|p| **p).collect();
        nr.sort_unstable();
        detail.extend(nr.iter().take(8).copied());
    }
    for page in detail.iter() {
        let bytenr = (*page as u64) << 12;
        let nrefs = refs_of.get(page).map(|v| v.len()).unwrap_or(0);
        match extents.get(page) {
            Some(e) => eprintln!(
                "[own] bytenr {bytenr}: disk {} ram {} window (btime {} btime_ns {} otime {}) \
                 win_count {} assigned {} refs {nrefs}",
                e.disk, e.ram, e.win_btime, e.win_btime_ns, e.win_otime, e.win_count, e.assigned
            ),
            None => eprintln!("[own] bytenr {bytenr}: not in the extent map at all"),
        }
        for node in refs_of.get(page).into_iter().flatten() {
            let n = &tree.nodes[*node as usize];
            let (btime, btime_ns, ctime, subvol) = node_key(tree, *node, subvol_otime);
            let matched = extents
                .get(page)
                .map(|e| {
                    btime == e.win_btime
                        && btime_ns == e.win_btime_ns
                        && ctime == e.win_otime
                        && e.win_count > 0
                })
                .unwrap_or(false);
            let s = &tree.subvols[subvol as usize];
            eprintln!(
                "[own]   node {node} kind {} flags 0x{:02x} btime {btime} btime_ns {btime_ns} \
                 ctime {ctime} (subvol {} id {}) owned {} matched {matched} path {}",
                n.kind,
                n.flags,
                subvol,
                s.id,
                n.owned,
                String::from_utf8_lossy(&tree.path_of(*node))
            );
        }
    }
}

/// Development aid, gated by `CST_DEBUG_OWN_STATS`: the ownership
/// arithmetic, checked rather than trusted.
///
/// * `split` recomputes, per extent, how many references in the block match
///   the window and what that credits them, and compares it with
///   `win_count`: the two have to agree, since `set_windows` derives the
///   window from exactly these references - so a deficit is bytes credited
///   to nobody.
/// * `inline` tallies the payloads that live in the fs tree rather than the
///   extent tree, invisible to any mode that does not walk the subvolume.
/// * `reach` walks the tree from the scanned roots and sums the owned bytes
///   they reach, to catch bytes that never roll into a root's total.
fn debug_ownership_stats(
    tree: &Tree,
    extents: &ExtentMap,
    ref_blocks: &[Vec<u64>],
    subvol_otime: &[u64],
) {
    // The split arithmetic: an extent is divided by win_count, and win_count
    // is the number of references in the block that carry the window key.
    // Count that independently - references whose node's key equals the window
    // - and sum what each matched reference is credited. A deficit is bytes
    // credited to nobody and an excess the reverse; both have to be zero, since
    // `set_windows` derives the window from exactly these references.
    {
        let mut matched: HashMap<u32, u32> = HashMap::new();
        let mut match_subvol: HashMap<u32, u16> = HashMap::new();
        for r in ref_blocks[0].iter() {
            let page = (*r >> 32) as u32;
            let Some(e) = extents.get(&page) else { continue };
            if e.win_count == 0 {
                continue;
            }
            let (btime, btime_ns, ctime, subvol) = node_key(tree, *r as u32, subvol_otime);
            if btime == e.win_btime && btime_ns == e.win_btime_ns && ctime == e.win_otime {
                *matched.entry(page).or_insert(0) += 1;
                match_subvol.entry(page).or_insert(subvol);
            }
        }
        let mut deficit = 0u64;
        let mut deficit_n = 0u64;
        let mut over = 0u64;
        let mut over_n = 0u64;
        let mut mismatched_assigned = 0u64;
        let mut by_subvol: HashMap<u16, (u64, u64)> = HashMap::new();
        let mut by_otime: HashMap<u64, (u64, u64)> = HashMap::new();
        for (p, e) in extents.iter() {
            if e.win_count == 0 {
                continue;
            }
            let m = *matched.get(p).unwrap_or(&0) as u64;
            let n = e.win_count as u64;
            if m != e.assigned as u64 {
                mismatched_assigned += 1;
            }
            let credited = if m < n {
                m * (e.disk / n)
            } else {
                e.disk + (m - n) * (e.disk / n)
            };
            if m < n {
                let lost = e.disk - credited;
                deficit += lost;
                deficit_n += 1;
                let sv = match_subvol.get(p).copied().unwrap_or(u16::MAX);
                let b = by_subvol.entry(sv).or_insert((0, 0));
                b.0 += 1;
                b.1 += lost;
                let b = by_otime.entry(e.win_otime).or_insert((0, 0));
                b.0 += 1;
                b.1 += lost;
            } else if m > n {
                over += credited - e.disk;
                over_n += 1;
            }
        }
        eprintln!(
            "[own] split: {deficit_n} extents lose {:.3} GiB (matched < win_count); \
             {over_n} extents over-credit {:.3} GiB (matched > win_count); \
             {mismatched_assigned} extents where matched != assigned",
            deficit as f64 / 1073741824.0,
            over as f64 / 1073741824.0
        );
        let mut v: Vec<(u16, (u64, u64))> = by_subvol.into_iter().collect();
        v.sort_by_key(|(_, c)| std::cmp::Reverse(c.1));
        for (sv, (n, bytes)) in v.iter().take(8) {
            let label = if *sv == u16::MAX {
                "no matching ref".to_string()
            } else {
                let s = &tree.subvols[*sv as usize];
                format!("id {} ({})", s.id, String::from_utf8_lossy(&s.path))
            };
            eprintln!(
                "[own]   lost share: {n} extents, {:.3} GiB, matching refs in subvol {label}",
                *bytes as f64 / 1073741824.0
            );
        }
        let mut w: Vec<(u64, (u64, u64))> = by_otime.into_iter().collect();
        w.sort_by_key(|(_, c)| std::cmp::Reverse(c.1));
        for (ot, (n, bytes)) in w.iter().take(5) {
            let ids: Vec<u64> = tree
                .subvols
                .iter()
                .filter(|s| s.otime == *ot)
                .map(|s| s.id)
                .collect();
            eprintln!(
                "[own]   lost share: window otime {ot} -> {n} extents, {:.3} GiB; subvol ids {ids:?}",
                *bytes as f64 / 1073741824.0
            );
        }
    }

    // Are the credited bytes reachable from the roots the report walks? A node
    // that no root's traversal reaches holds owned bytes that never roll into
    // the scanned root's total.
    {
        let total = tree.nodes.len();
        let mut seen = vec![false; total];
        let mut stack: Vec<u32> = Vec::new();
        for r in tree.roots.iter() {
            seen[*r as usize] = true;
            stack.push(*r);
        }
        while let Some(i) = stack.pop() {
            let nd = tree.nodes[i as usize];
            for c in nd.first_child..nd.first_child + nd.nchildren {
                if !seen[c as usize] {
                    seen[c as usize] = true;
                    stack.push(c);
                }
            }
        }
        // and again from the synthetic root's children, to tell "outside the
        // scanned tree" from "not linked in at all"
        let mut seen0 = vec![false; total];
        let mut stack0: Vec<u32> = Vec::new();
        {
            let root0 = &tree.nodes[0];
            for c in root0.first_child..root0.first_child + root0.nchildren {
                seen0[c as usize] = true;
                stack0.push(c);
            }
        }
        while let Some(i) = stack0.pop() {
            let nd = tree.nodes[i as usize];
            for c in nd.first_child..nd.first_child + nd.nchildren {
                if !seen0[c as usize] {
                    seen0[c as usize] = true;
                    stack0.push(c);
                }
            }
        }
        let mut all_owned = 0u64;
        let mut seen_owned = 0u64;
        let mut seen0_owned = 0u64;
        let mut hidden: Vec<(u32, u64)> = Vec::new();
        for i in 0..total {
            let o = tree.nodes[i].owned;
            all_owned += o;
            if seen[i] {
                seen_owned += o;
            } else if o > 0 {
                hidden.push((i as u32, o));
            }
            if seen0[i] {
                seen0_owned += o;
            }
        }
        hidden.sort_by_key(|(_, o)| std::cmp::Reverse(*o));
        eprintln!(
            "[own] reach: {} of {total} nodes reachable from the roots, owned {:.3} GiB; \
             from the synthetic root, owned {:.3} GiB; all nodes {:.3} GiB; \
             {} nodes ({:.3} GiB) outside the roots",
            seen.iter().filter(|x| **x).count(),
            seen_owned as f64 / 1073741824.0,
            seen0_owned as f64 / 1073741824.0,
            all_owned as f64 / 1073741824.0,
            hidden.len(),
            (all_owned - seen_owned) as f64 / 1073741824.0
        );
        for (i, o) in hidden.iter().take(8) {
            let nd = &tree.nodes[*i as usize];
            let d = if nd.kind == KIND_DIR {
                &tree.dirs[nd.extra as usize]
            } else {
                &tree.dirs[tree.nodes[nd.parent as usize].extra as usize]
            };
            let s = &tree.subvols[d.subvol as usize];
            eprintln!(
                "[own]   outside-roots node {i} kind {} flags 0x{:02x} owned {:.3} GiB \
                 subvol id {} ({}) parent {} path {}",
                nd.kind,
                nd.flags,
                *o as f64 / 1073741824.0,
                s.id,
                String::from_utf8_lossy(&s.path),
                nd.parent,
                String::from_utf8_lossy(&tree.path_of(*i))
            );
        }
    }
}

/// Scan by reading the trees: byte-identical to the retired file walker,
/// without visiting a single file.
pub fn scan_tree(opts: &ScanOptions) -> io::Result<(Tree, ScanStats)> {
    let started = std::time::Instant::now();
    // Per-phase timing on stderr: which part of a scan the seconds go to.
    let mut t_phase = std::time::Instant::now();
    // Development aid, silent unless CST_TIME is set: where a scan's time goes,
    // phase by phase, on stderr.
    let show_phases = std::env::var_os("CST_TIME").is_some();
    macro_rules! phase {
        ($name:expr) => {{
            if show_phases {
                eprintln!(
                    "[time] {:<24} {:8.2}s (total {:.2}s)",
                    $name,
                    t_phase.elapsed().as_secs_f64(),
                    started.elapsed().as_secs_f64()
                );
            }
            t_phase = std::time::Instant::now();
        }};
    }
    let mut stats = ScanStats::default();
    let live = opts.live.clone();
    if let Some(l) = &live {
        l.running.store(true, Ordering::SeqCst);
    }
    let mut c = Collector {
        shard: Shard::default(),
        subs: Vec::new(),
        subvols: Vec::new(),
        unreadable: 0,
        files: 0,
        dirs: 0,
        full_scan: opts.full_scan,
        collapsed: Vec::new(),
        graph_kids: Vec::new(),
    };

    // inventory of every subvolume, for snapshot classification
    let first = opts.roots.first().cloned().unwrap_or_else(|| b"/".to_vec());
    let f = open_dir(&first)?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&f);
    let inv = btrfs::list_subvolumes(fd).unwrap_or_default();


    let mut roots = Vec::new();
    for path in &opts.roots {
        let file = open_dir(path)?;
        let info = btrfs::get_subvol_info(std::os::fd::AsRawFd::as_raw_fd(&file))?;
        let subvol = c.intern_subvol(&info, &inv, path);
        let inode = file
            .metadata()
            .map(|m| std::os::unix::fs::MetadataExt::ino(&m))
            .unwrap_or(btrfs::BTRFS_FIRST_FREE_OBJECTID);
        roots.push(Root {
            path: path.clone(),
            subvol,
            inode,
            info,
        });
    }
    phase!("inventory + roots");
    let t_collect = std::time::Instant::now();
    // 0 means "as many as the machine has", the old -j default; 1 means the
    // serial path, exactly as it was before threads existed.
    let threads = if opts.threads > 0 {
        opts.threads
    } else {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    };
    for root in &roots {
        collect_all(&mut c, fd, &inv, root, &opts.excludes, live.as_deref(), threads)?;
    }
    if std::env::var_os("CST_TIME_SUBVOL").is_some() {
        eprintln!(
            "[time]   collect_all wall {:.2}s for {} subvolumes",
            t_collect.elapsed().as_secs_f64(),
            c.subs.len()
        );
    }
    phase!("collect_all (fs-tree walks)");

    let (mut tree, final_refs, collapsed_nodes) = build(&mut c, &roots, &opts.excludes);
    tree.inventory = inv.clone();
    // the assembled array, whose references name their owner node
    let mut final_refs = final_refs;
    phase!("build");

    // Where a cancel is honoured: the walks check it per kernel search, these
    // passes per pass, and the ownership passes per batch of extents. Nothing
    // here can interrupt a syscall already in flight, so the floor is one
    // TREE_SEARCH_V2 - a few milliseconds - rather than one tree.
    let cancel = opts.live.as_ref().map(|p| &p.cancel);
    let cancelled = || cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed));

    // Snapshot-class subvolumes were not walked, so their references - and the
    // extents only they still hold - come from the extent tree instead.
    if !collapsed_nodes.is_empty() {
        let started = std::time::Instant::now();
        let mut g = BlockRefs {
            index: HashMap::new(),
            refs: Vec::new(),
            memo: HashMap::new(),
        };
        collect_block_refs(fd, cancel, &mut g)?;
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        phase!("collect_block_refs");
        let blocks = g.index.len();
        // every subvolume this scan covers: a reference that names one of them
        // is in scope, and if it resolves to nothing the bytes are lost
        let in_scan: HashSet<u64> = tree.subvols.iter().map(|s| s.id).collect();
        let inventory: HashSet<u64> = inv.iter().map(|r| r.treeid).collect();
        let (mut pairs, cstats) = collect_collapsed_refs(
            fd, cancel, &collapsed_nodes, &in_scan, &inventory, &mut g, &mut c.shard.extents,
        )?;
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        phase!("collect_collapsed_refs");
        drop(g);
        let nrefs = pairs.len();
        // A node's references have to be one contiguous run of the array, so
        // group by node before appending: extens are visited in bytenr order,
        // which is not node order.
        pairs.sort_unstable();
        let mut i = 0usize;
        while i < pairs.len() {
            let node = pairs[i].0;
            let start = final_refs.len() as u32;
            while i < pairs.len() && pairs[i].0 == node {
                final_refs.push(((pairs[i].1 as u64) << 32) | node as u64);
                i += 1;
            }
            let n = &mut tree.nodes[node as usize];
            n.first_ref = start;
            n.nrefs = final_refs.len() as u32 - start;
        }
        phase!("collapse ref grouping");
        eprintln!(
            "collapsed {} subvolumes: {nrefs} references resolved through {blocks} tree blocks in {:.1}s",
            collapsed_nodes.len(),
            started.elapsed().as_secs_f64()
        );
        eprintln!(
            "  snapshots alone hold {} extents ({:.2} GiB); {} extents ({:.2} GiB) are held by both \
             a walked tree and a snapshot",
            cstats.added.0,
            cstats.added.1 as f64 / 1073741824.0,
            cstats.contested.0,
            cstats.contested.1 as f64 / 1073741824.0
        );
        eprintln!(
            "  unresolved: {} extents ({:.2} GiB) inside this scan, {} extents ({:.2} GiB) outside it",
            cstats.lost.0,
            cstats.lost.1 as f64 / 1073741824.0,
            cstats.outside.0,
            cstats.outside.1 as f64 / 1073741824.0
        );
        if cstats.gone.0 > 0 {
            eprintln!(
                "  {} extents ({:.2} GiB) reference a subvolume that no longer exists (a pruned \
                 snapshot's leaves, still referenced by the ones that share them)",
                cstats.gone.0,
                cstats.gone.1 as f64 / 1073741824.0
            );
        }
        if std::env::var_os("CST_DEBUG_EXT").is_some() {
            let mut per: HashMap<u32, u64> = HashMap::new();
            for (n, _) in pairs.iter() {
                *per.entry(*n).or_insert(0) += 1;
            }
            let mut v: Vec<(u32, u64)> = per.into_iter().collect();
            v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
            for (n, c) in v.iter().take(12) {
                let d = &tree.dirs[tree.nodes[*n as usize].extra as usize];
                let id = tree.subvols[d.subvol as usize].id;
                eprintln!("  node {n} (subvol {id}): {c} refs");
            }
            eprintln!("  collapsed nodes receiving nothing: {}", collapsed_nodes.len() - v.len());
        }
    }

    let mut ref_blocks = vec![final_refs];
    // deterministic subvolume numbering, same as the file backend
    {
        let mut order: Vec<usize> = (0..tree.subvols.len()).collect();
        order.sort_by_key(|&i| tree.subvols[i].uuid);
        let mut remap = vec![0u16; tree.subvols.len()];
        let sorted: Vec<Subvol> = order
            .iter()
            .enumerate()
            .map(|(new, &old)| {
                remap[old] = new as u16;
                tree.subvols[old].clone()
            })
            .collect();
        tree.subvols = sorted;
        for d in tree.dirs.iter_mut() {
            d.subvol = remap[d.subvol as usize];
        }
    }

    stats.subvolumes = c.subvols.len() as u64;
    stats.files = c.files;
    stats.dirs = c.dirs;
    stats.extents = c.shard.extents.len() as u64;
    stats.refs = ref_blocks[0].len() as u64;
    stats.elapsed = started.elapsed();
    tree.stats = stats.clone();
    tree.scanned_at = crate::cache::now();
    tree.scanned_roots = opts.roots.clone();

    let mut extents = std::mem::take(&mut c.shard.extents);
    // Development aid: the extent map this scan ended up with, as logical
    // bytenrs, so two scans can be diffed against each other.
    if let Ok(path) = std::env::var("CST_DUMP_MAP") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::File::create(&path) {
            let mut pages: Vec<u32> = extents.keys().copied().collect();
            pages.sort_unstable();
            let mut buf = String::with_capacity(1 << 20);
            for p in pages {
                buf.push_str(&format!("{}\n", (p as u64) << 12));
                if buf.len() >= 1 << 20 {
                    let _ = f.write_all(buf.as_bytes());
                    buf.clear();
                }
            }
            let _ = f.write_all(buf.as_bytes());
            eprintln!("dumped {} extents to {path}", extents.len());
        }
    }
    phase!("subvol remap + map dump");
    let subvol_otime: Vec<u64> = tree.subvols.iter().map(|s| s.otime).collect();
    // The one place windows are set: from the references that reach the block,
    // so a holder build() never placed cannot own - or deny - an extent.
    set_windows(&tree, &ref_blocks[0], &mut extents, &subvol_otime);
    tree.attribute_owned(&ref_blocks, &mut extents, &subvol_otime, cancel);
    phase!("attribute_owned");
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    if std::env::var_os("CST_DEBUG_OWN").is_some() {
        debug_ownership(&tree, &extents, &ref_blocks, &subvol_otime);
        phase!("debug_ownership");
    }
    if std::env::var_os("CST_DEBUG_OWN_STATS").is_some() {
        debug_ownership_stats(&tree, &extents, &ref_blocks, &subvol_otime);
        phase!("debug_ownership_stats");
    }
    tree.attribute_reach(&mut ref_blocks, &extents, cancel);
    phase!("attribute_reach");
    if cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    tree.rollup();
    phase!("rollup");
    if let Some(l) = &live {
        l.running.store(false, Ordering::SeqCst);
    }
    Ok((tree, stats))
}



/// Time the kernel's reference lookup on a sample of extents.
pub fn probe_logical_ino(fd: i32, sample: usize) -> io::Result<(usize, usize, f64)> {
    let mut extents: Vec<u64> = Vec::new();
    btrfs::for_each_extent_tree_item(fd, None, &mut |bytenr, ty, _len, payload| {
        if extents.len() >= sample {
            return;
        }
        if ty == btrfs::BTRFS_EXTENT_ITEM_KEY {
            if let Some(h) = btrfs::parse_extent_item_hdr(payload) {
                if !h.is_metadata {
                    extents.push(bytenr);
                }
            }
        }
    })?;
    let mut buf = Vec::new();
    let started = std::time::Instant::now();
    let mut refs = 0usize;
    let mut failed = 0usize;
    for e in &extents {
        match btrfs::logical_ino(fd, *e, &mut buf) {
            Ok(r) => refs += r.len(),
            Err(_) => failed += 1,
        }
    }
    Ok((extents.len(), refs + failed * 0, started.elapsed().as_secs_f64()))
}

/// Development aid: what one pass over the filesystem's extent tree sees.
pub fn probe_extent_tree(fd: i32) -> io::Result<(u64, u64, u64, u64, u64, Vec<(u64, u64)>)> {
    use std::collections::HashMap;
    let mut extents = 0u64;
    let mut bytes = 0u64;
    let mut refs = 0u64;
    let mut shared = 0u64;
    let mut metadata_extents = 0u64;
    let mut bad = 0u64;
    let mut owner = 0u64;
    let mut per_root: HashMap<u64, u64> = HashMap::new();
    let mut cur_data = false;
    if std::env::var_os("CST_DUMP_EXT").is_some() {
        extents = 1; // dump mode: the first items, raw, for external checking
    }
    btrfs::for_each_extent_tree_item(fd, None, &mut |bytenr, ty, len, payload| match ty {
        btrfs::BTRFS_EXTENT_ITEM_KEY | btrfs::BTRFS_METADATA_ITEM_KEY => {
            let Some(h) = btrfs::parse_extent_item_hdr(payload) else {
                return;
            };
            if btrfs::is_metadata_extent(ty, &h) {
                cur_data = false;
                metadata_extents += 1;
                return;
            }
            cur_data = true;
            extents += 1;
            bytes += len;
            if std::env::var_os("CST_DUMP_EXT").is_some() && extents < 201 {
                println!(
                    "{} {} {} {}",
                    bytenr,
                    len,
                    payload.len(),
                    payload.iter().map(|b| format!("{b:02x}")).collect::<String>()
                );
            }
            // every reference of a single-ref extent lives in this item
            let complete = btrfs::for_each_inline_ref(ty, payload, &mut |ty, body| match ty {
                btrfs::BTRFS_EXTENT_DATA_REF_KEY => {
                    if let Some(r) = btrfs::parse_data_ref(body) {
                        refs += r.count as u64;
                        *per_root.entry(r.root).or_insert(0) += r.count as u64;
                    }
                }
                btrfs::BTRFS_SHARED_DATA_REF_KEY => {
                    if let Some(s) = btrfs::parse_shared_data_ref(body) {
                        shared += 1;
                        let _ = s;
                    }
                }
                btrfs::BTRFS_EXTENT_OWNER_REF_KEY => owner += 1,
                _ => {}
            });
            if !complete {
                bad += 1;
                if std::env::var_os("CST_DEBUG_EXT").is_some() {
                    eprintln!(
                        "  [desync] bytenr {bytenr} len {len} item_len {} hex {}",
                        payload.len(),
                        payload.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    );
                }
            }
        }
        btrfs::BTRFS_EXTENT_DATA_REF_KEY => {
            if cur_data {
                if let Some(r) = btrfs::parse_data_ref(payload) {
                    refs += r.count as u64;
                    *per_root.entry(r.root).or_insert(0) += r.count as u64;
                }
            }
        }
        btrfs::BTRFS_SHARED_DATA_REF_KEY => {
            if cur_data {
                shared += 1;
            }
        }
        _ => {}
    })?;
    if extents > 0 && std::env::var_os("CST_DUMP_EXT").is_some() {
        extents -= 1;
    }
    if bad > 0 {
        eprintln!("extent items that did not parse cleanly: {bad}");
    }
    eprintln!("owner refs: {owner}");
    let mut roots: Vec<(u64, u64)> = per_root.into_iter().collect();
    roots.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    Ok((extents, bytes, refs, shared, metadata_extents, roots))
}
