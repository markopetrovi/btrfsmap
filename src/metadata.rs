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
use crate::walk::{ScanOptions, ScanStats};
use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::ffi::OsStrExt;
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
    inline_disk: u16,
    inline_ram: u16,
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
    btrfs::for_each_fs_item(fd, root_id, &mut |obj, ty, _off, payload| {
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
    dirs: u64,
    files: u64,
}

/// Collects a whole scan: the shared extent map, the flat reference array and
/// per-subvolume trees.
struct Collector {
    extents: ExtentMap,
    refs: Vec<u64>,
    subs: Vec<Collected>,
    subvols: Vec<Subvol>,
    warnings: Vec<String>,
    unreadable: u64,
    files: u64,
    dirs: u64,
    /// walk every fs tree, including the snapshot-class subvolumes (the M2
    /// behaviour: snapshot interiors stay browsable)
    full_scan: bool,
    /// snapshot-class subvolumes left un-walked: (root id, subvol index, path)
    collapsed: Vec<(u64, u16, Vec<u8>)>,
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
fn collect_subvolume(
    c: &mut Collector,
    fd: i32,
    root_id: u64,
    subvol: u16,
    subvol_otime: u64,
    scope: Option<&HashSet<u64>>,
) -> io::Result<Collected> {
    let in_scope = |inode: u64| scope.map(|s| s.contains(&inode)).unwrap_or(true);
    // References from inside a snapshot-class subvolume carry no birth time -
    // the same key the collapsed scan builds for it, so the two modes agree.
    // See the ownership rule: a snapshot owns what the live trees let go of.
    let snap_class = c.subvols[subvol as usize].is_snapshot();
    let mut col = Collected {
        subvol,
        recs: Vec::new(),
        entries: Vec::new(),
        names: Vec::new(),
        nested: Vec::new(),
        dirs: 0,
        files: 0,
    };
    let mut cur: Option<InodeRec> = None;
    let mut overflow = false;

    // INODE_ITEM then DIR_INDEX then EXTENT_DATA arrive for one inode before the
    // next inode starts, so a record can be finished and pushed as we go.
    let mut flush = |cur: &mut Option<InodeRec>, col: &mut Collected, c: &mut Collector| {
        if let Some(rec) = cur.take() {
            let _ = c;
            col.recs.push(rec);
        }
    };

    btrfs::for_each_fs_item(fd, root_id, &mut |obj, ty, key_off, payload| {
        // records are in inode order, so a new objectid ends the previous one
        let same = cur.as_ref().map(|r| r.inode == obj).unwrap_or(false);
        if !same && ty == BTRFS_INODE_ITEM_KEY {
            flush(&mut cur, &mut col, c);
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
                    rec.inline_disk = rec
                        .inline_disk
                        .saturating_add(item.disk_num_bytes.min(u16::MAX as u64) as u16);
                    rec.inline_ram = rec
                        .inline_ram
                        .saturating_add(item.ram_bytes.min(u16::MAX as u64) as u16);
                    rec.referenced += item.ram_bytes;
                    return;
                }
                if item.disk_bytenr == 0 {
                    return; // hole
                }
                let page = (item.disk_bytenr >> 12) as u32;
                if (item.disk_bytenr >> 12) > u32::MAX as u64 {
                    overflow = true;
                    return;
                }
                if rec.first_ref == 0 && rec.nrefs == 0 {
                    rec.first_ref = c.refs.len() as u32;
                }
                let key = (rec.btime, rec.btime_ns, subvol_otime);
                let e = c.extents.entry(page).or_insert_with(|| ExtentInfo {
                    disk: item.disk_num_bytes,
                    ram: item.ram_bytes,
                    win_btime: u32::MAX,
                    win_btime_ns: u16::MAX,
                    win_otime: u64::MAX,
                    win_count: 0,
                    assigned: 0,
                });
                let best = (e.win_btime, e.win_btime_ns, e.win_otime);
                if key < best {
                    e.win_btime = key.0;
                    e.win_btime_ns = key.1;
                    e.win_otime = key.2;
                    e.win_count = 1;
                } else if key == best {
                    e.win_count += 1;
                }
                c.refs.push((page as u64) << 32);
                rec.nrefs += 1;
                rec.referenced += item.num_bytes;
            }
            _ => {}
        }
    })?;
    flush(&mut cur, &mut col, c);
    if overflow {
        c.warnings.push(format!(
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
    /// blocks whose reference list the kernel itself does not trust
    /// (`BTRFS_BLOCK_FLAG_FULL_BACKREF`): an extent referenced through one of
    /// these cannot be resolved from the extent tree alone
    full: HashSet<u64>,
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
fn collect_block_refs(fd: i32, g: &mut BlockRefs) -> io::Result<()> {
    btrfs::for_each_extent_tree_item(fd, &mut |bytenr, ty, key_off, payload| {
        match ty {
            btrfs::BTRFS_EXTENT_ITEM_KEY | btrfs::BTRFS_METADATA_ITEM_KEY => {
                let Some(h) = btrfs::parse_extent_item_hdr(payload) else {
                    return;
                };
                if !btrfs::is_metadata_extent(ty, &h) {
                    return;
                }
                if h.flags & btrfs::BTRFS_BLOCK_FLAG_FULL_BACKREF != 0 {
                    g.full.insert(bytenr);
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
    keys: &HashMap<u32, (u32, u16, u64)>,
    extents: &mut ExtentMap,
    out: &mut Vec<(u32, u32)>,
    added: &mut u64,
    stats: &mut CollapseStats,
    named_in_scan: bool,
    named_gone: bool,
    candidates: &mut Vec<(u64, u64)>,
    suspect: bool,
    want_candidates: bool,
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
            if suspect && want_candidates {
                candidates.push((len, bytenr));
            }
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
    // The window decides ownership, and a walker sets it as it goes; these
    // references need the same treatment or the extent ends up with a window no
    // node's key matches and nobody owns it.
    let e = extents.get_mut(&page).unwrap();
    let dbg = std::env::var("CST_DEBUG_BYTENR")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v == bytenr)
        .unwrap_or(false);
    if dbg {
        eprintln!(
            "  [dbg] bytenr {bytenr} len {len} window before ({}, {}, {}) count {} holders {}",
            e.win_btime, e.win_btime_ns, e.win_otime, e.win_count, holders.len()
        );
        for &n in holders.iter() {
            eprintln!("    holder node {n}: key {:?}", keys.get(&n));
        }
    }
    for &n in holders.iter() {
        let key = *keys.get(&n).unwrap_or(&(u32::MAX, u16::MAX, 0));
        let best = (e.win_btime, e.win_btime_ns, e.win_otime);
        if key < best || e.win_count == 0 {
            e.win_btime = key.0;
            e.win_btime_ns = key.1;
            e.win_otime = key.2;
            e.win_count = 1;
        } else if key == best {
            e.win_count += 1;
        }
    }
    if dbg {
        let e = extents.get(&page).unwrap();
        eprintln!(
            "  [dbg] window after ({}, {}, {}) count {}",
            e.win_btime, e.win_btime_ns, e.win_otime, e.win_count
        );
    }
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
    nodes: &HashMap<u64, u32>,
    keys: &HashMap<u32, (u32, u16, u64)>,
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
    // Off by default: asking the kernel is the only exact route, at ~80-400us
    // per extent, so it has to be aimed. Measured on /home with a 20s budget
    // over the 245k largest unresolved extents: not one belonged to a collapsed
    // subvolume, so the extents the collapsed scan cannot attribute are not
    // simply the unresolved ones. Left as a knob for the filesystem where it
    // does line up. See the README's limits.
    let resolve_budget: f64 = std::env::var("CST_RESOLVE_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let mut candidates: Vec<(u64, u64)> = Vec::new();
    let mut suspect = false;

    btrfs::for_each_extent_tree_item(fd, &mut |bytenr, ty, key_off, payload| {
        if bytenr != cur_bytenr {
            flush_collapsed(
                cur_bytenr, cur_len, cur_data, &mut holders, keys, extents, &mut out,
                &mut added, &mut stats, named_in_scan, named_gone, &mut candidates, suspect,
                resolve_budget > 0.0,
            );
            cur_bytenr = bytenr;
            cur_data = false;
            named_in_scan = false;
            named_gone = false;
            suspect = false;
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
                            if g.full.contains(&s.parent) {
                                suspect = true;
                            }
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
                if g.full.contains(&key_off) {
                    suspect = true;
                }
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
        cur_bytenr, cur_len, cur_data, &mut holders, keys, extents, &mut out, &mut added,
        &mut stats, named_in_scan, named_gone, &mut candidates, suspect,
        resolve_budget > 0.0,
    );
    stats.added.0 = added;

    // The budgeted repair. The extent tree can name a root whose block
    // references are incomplete (`FULL_BACKREF`), and then an extent held only
    // by collapsed snapshots looks exactly like one held by a subvolume outside
    // the scan. The kernel can tell them apart - `LOGICAL_INO` does the full
    // backref walk, reading tree blocks one at a time - but at ~150us per
    // extent it is minutes for the whole filesystem, so it is spent here on the
    // largest unresolved extents only, under a time cap.
    if !candidates.is_empty() {
        let budget = resolve_budget;
        candidates.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let queued: Vec<(u64, u64)> = std::mem::take(&mut candidates);
        let started = std::time::Instant::now();
        let mut buf: Vec<u8> = Vec::new();
        let (mut tried, mut fixed, mut bytes) = (0u64, 0u64, 0u64);
        for (len, bytenr) in queued.iter() {
            if started.elapsed().as_secs_f64() > budget {
                break;
            }
            tried += 1;
            let Ok(refs) = btrfs::logical_ino(fd, *bytenr, &mut buf) else {
                continue;
            };
            holders.clear();
            for (root, _ino, _off) in refs {
                if let Some(&n) = nodes.get(&root) {
                    holders.push(n);
                }
            }
            if holders.is_empty() {
                continue; // genuinely outside the scan
            }
            flush_collapsed(
                *bytenr, *len, true, &mut holders, keys, extents, &mut out, &mut added,
                &mut stats, false, false, &mut candidates, false, false,
            );
            fixed += 1;
            bytes += len;
        }
        if tried > 0 {
            eprintln!(
                "  asked the kernel about the {tried} largest unresolved extents ({:.1}s): \
                 {fixed} belong to a collapsed subvolume ({:.2} GiB); {} left",
                started.elapsed().as_secs_f64(),
                bytes as f64 / 1073741824.0,
                queued.len() as u64 - tried
            );
        }
    }
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

fn build(c: &mut Collector, roots: &[Root]) -> (Tree, Vec<u64>, HashMap<u64, u32>) {
    let mut tree = Tree {
        nodes: Vec::new(),
        names: Vec::new(),
        dirs: Vec::new(),
        roots: Vec::new(),
        root: 0,
        subvols: c.subvols.clone(),
        inventory: Vec::new(),
        warnings: c.warnings.clone(),
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

    let mut final_refs: Vec<u64> = Vec::with_capacity(c.refs.len());
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
    let mut inode_copies: HashMap<(usize, u64), u32> = HashMap::new();
    let mut root_nodes: Vec<u32> = Vec::new();
    // root id -> the node that stands for it
    let mut collapsed_nodes: HashMap<u64, u32> = HashMap::new();

    // A directory must own a *contiguous* run of nodes, because everything
    // downstream (children slices, the rollup) assumes it. So each directory is
    // visited once and its children appended as one batch, then the child
    // directories are visited in turn - which is the order the file backend's
    // BFS produces, children sorted by name.
    let mut dirs: VecDeque<(usize, u64, u32)> = VecDeque::new();
    // synthetic root's children: the paths given on the command line
    let mut root_queue: Vec<(usize, u64, Vec<u8>, u8)> = Vec::new();
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
        root_queue.push((ci, root.inode, name, flags));
    }

    // helper closure: append one node
    fn push_node(
        tree: &mut Tree,
        c: &mut Collector,
        ci: usize,
        inode: u64,
        parent_node: u32,
        name: &[u8],
        mut flags: u8,
        rec: InodeRec,
        kind: u8,
        final_refs: &mut Vec<u64>,
        inode_copies: &mut HashMap<(usize, u64), u32>,
    ) -> u32 {
        let idx = tree.nodes.len() as u32;
        if c.subs[ci].subvol_snapshot(&tree.subvols) {
            flags |= FLAG_SNAP_CLASS;
        }
        let copies = inode_copies.entry((ci, inode)).or_insert(0);
        *copies += 1;
        let a = rec.first_ref as usize;
        let b = a + rec.nrefs as usize;
        let first_ref = final_refs.len() as u32;
        for r in &c.refs[a..b] {
            final_refs.push(*r | idx as u64); // low half: owner node index
        }
        if *copies > 1 {
            for r in &c.refs[a..b] {
                if let Some(e) = c.extents.get_mut(&((*r >> 32) as u32)) {
                    e.win_count += 1;
                }
            }
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
            inline_disk: rec.inline_disk,
            inline_ram: rec.inline_ram,
            first_ref,
            nrefs: rec.nrefs,
            ..Default::default()
        });
        idx
    }

    // append the roots as the synthetic root's batch
    {
        let first = tree.nodes.len() as u32;
        for (ci, inode, name, flags) in root_queue {
            let Ok(pos) = c.subs[ci].recs.binary_search_by_key(&inode, |r| r.inode) else {
                continue;
            };
            let rec = c.subs[ci].recs[pos];
            let kind = match rec.mode & btrfs::S_IFMT {
                btrfs::S_IFDIR => KIND_DIR,
                btrfs::S_IFREG => KIND_FILE,
                _ => continue,
            };
            let idx = push_node(&mut tree, c, ci, inode, 0, &name, flags, rec, kind, &mut final_refs, &mut inode_copies);
            root_nodes.push(idx);
            if kind == KIND_DIR {
                let extra = tree.dirs.len() as u32;
                tree.dirs.push(DirExtra {
                    subvol: c.subs[ci].subvol,
                    depth: 1,
                    ..Default::default()
                });
                tree.nodes[idx as usize].extra = extra;
                dirs.push_back((ci, inode, idx));
            }
        }
        let n = tree.nodes.len() as u32 - first;
        let node = &mut tree.nodes[0];
        node.first_child = first;
        node.nchildren = n;
        tree.roots = root_nodes.clone();
    }

    while let Some((ci, inode, node_idx)) = dirs.pop_front() {
        let first_child = tree.nodes.len() as u32;
        // gather this directory's children first: pushing nodes borrows the
        // collector mutably, so nothing may hold a borrow of its entries
        let mut kids: Vec<(usize, u64, Vec<u8>, u8, Option<u16>)> = Vec::new();
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
                            kids.push((usize::MAX, 0, child_name, 0, Some(subvol)));
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
            kids.push((child_ci, child_inode, child_name, child_flags, collapsed));
        }
        }
        for (child_ci, child_inode, child_name, child_flags, collapsed) in kids {
            // depth comes from the parent, which is already placed
            let depth = if node_idx == 0 {
                1
            } else {
                tree.dirs[tree.nodes[node_idx as usize].extra as usize].depth + 1
            };
            if let Some(subvol) = collapsed {
                let idx = push_collapsed_node(&mut tree, c, subvol, node_idx, &child_name, depth);
                collapsed_nodes.insert(tree.subvols[subvol as usize].id, idx);
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
            let idx = push_node(&mut tree, c, child_ci, child_inode, node_idx, &child_name, child_flags, rec, kind, &mut final_refs, &mut inode_copies);
            if kind == KIND_DIR {
                let extra = tree.dirs.len() as u32;
                tree.dirs.push(DirExtra {
                    subvol: c.subs[child_ci].subvol,
                    depth,
                    ..Default::default()
                });
                tree.nodes[idx as usize].extra = extra;
                dirs.push_back((child_ci, child_inode, idx));
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

/// Collect one subvolume, then every subvolume nested inside it.
fn collect_all(
    c: &mut Collector,
    fd: i32,
    inventory: &[btrfs::RootItem],
    start: &Root,
) -> io::Result<()> {
    let mut work: Vec<(u64, u16, Vec<u8>)> = vec![(start.info.treeid, start.subvol, start.path.clone())];
    let mut done: Vec<u64> = Vec::new();
    while let Some((root_id, subvol, path)) = work.pop() {
        if done.contains(&root_id) {
            continue;
        }
        done.push(root_id);
        let otime = c.subvols[subvol as usize].otime;
        // Scanning a whole subvolume (its root inode) is the common case and
        // needs no filtering; only a scan started deeper has to work out which
        // inodes are inside the requested directory.
        let scope = if path == start.path && start.inode != btrfs::BTRFS_FIRST_FREE_OBJECTID {
            Some(scope_of(fd, root_id, start.inode)?)
        } else {
            None
        };
        let col = collect_subvolume(c, fd, root_id, subvol, otime, scope.as_ref())?;
        for (child_root, dirid, name) in &col.nested {
            // the nested subvolume is reachable as a directory of this one, so
            // its identity is one open() away - no need to decode root items
            let child_path = path_to_dir(&col, *dirid, name, &path);
            let Ok(child) = open_dir(&child_path) else { continue };
            let Ok(info) = btrfs::get_subvol_info(std::os::fd::AsRawFd::as_raw_fd(&child)) else {
                continue;
            };
            if info.treeid != *child_root {
                continue;
            }
            let child_subvol = c.intern_subvol(&info, inventory, &child_path);
            if c.collapse(child_subvol) {
                // No fs tree walk: its references come from the extent tree, so
                // its subtree is not browsable - the trade the collapsed mode
                // is built on.
                c.collapsed.push((*child_root, child_subvol, child_path));
                continue;
            }
            work.push((*child_root, child_subvol, child_path));
        }
        c.subs.push(col);
    }
    Ok(())
}

/// Scan by reading the trees: identical results to `walk::scan_tree`, without
/// visiting a single file.
pub fn scan_tree(opts: &ScanOptions) -> io::Result<(Tree, ScanStats)> {
    let started = std::time::Instant::now();
    let mut stats = ScanStats::default();
    let mut c = Collector {
        extents: ExtentMap::new(),
        refs: Vec::new(),
        subs: Vec::new(),
        subvols: Vec::new(),
        warnings: Vec::new(),
        unreadable: 0,
        files: 0,
        dirs: 0,
        full_scan: opts.full_scan,
        collapsed: Vec::new(),
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
    for root in &roots {
        collect_all(&mut c, fd, &inv, root)?;
    }

    let (mut tree, final_refs, collapsed_nodes) = build(&mut c, &roots);
    tree.inventory = inv.clone();
    // the assembled array, whose references name their owner node
    let mut final_refs = final_refs;

    // Snapshot-class subvolumes were not walked, so their references - and the
    // extents only they still hold - come from the extent tree instead.
    if !collapsed_nodes.is_empty() {
        let started = std::time::Instant::now();
        let mut g = BlockRefs {
            index: HashMap::new(),
            refs: Vec::new(),
            memo: HashMap::new(),
            full: HashSet::new(),
        };
        collect_block_refs(fd, &mut g)?;
        let blocks = g.index.len();
        // the age each collapsed node carries into the ownership rule
        let keys: HashMap<u32, (u32, u16, u64)> = collapsed_nodes
            .values()
            .map(|&n| {
                let node = tree.nodes[n as usize];
                let d = &tree.dirs[node.extra as usize];
                (n, (node.btime, node.btime_ns, tree.subvols[d.subvol as usize].otime))
            })
            .collect();
        // every subvolume this scan covers: a reference that names one of them
        // is in scope, and if it resolves to nothing the bytes are lost
        let in_scan: HashSet<u64> = tree.subvols.iter().map(|s| s.id).collect();
        let inventory: HashSet<u64> = inv.iter().map(|r| r.treeid).collect();
        let (mut pairs, cstats) = collect_collapsed_refs(
            fd, &collapsed_nodes, &keys, &in_scan, &inventory, &mut g, &mut c.extents,
        )?;
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
    stats.extents = c.extents.len() as u64;
    stats.refs = ref_blocks[0].len() as u64;
    stats.elapsed = started.elapsed();
    tree.stats = stats.clone();
    tree.scanned_at = crate::cache::now();
    tree.scanned_roots = opts.roots.clone();

    let mut extents = c.extents;
    let subvol_otime: Vec<u64> = tree.subvols.iter().map(|s| s.otime).collect();
    tree.attribute_owned(&ref_blocks, &mut extents, &subvol_otime);
    tree.attribute_reach(&mut ref_blocks, &extents);
    tree.rollup();
    Ok((tree, stats))
}



/// Time the kernel's reference lookup on a sample of extents.
pub fn probe_logical_ino(fd: i32, sample: usize) -> io::Result<(usize, usize, f64)> {
    let mut extents: Vec<u64> = Vec::new();
    btrfs::for_each_extent_tree_item(fd, &mut |bytenr, ty, _len, payload| {
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
    btrfs::for_each_extent_tree_item(fd, &mut |bytenr, ty, len, payload| match ty {
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

/// Development aid: for a sample of data extents, what the block-reference
/// graph resolves their references to - to be diffed against the kernel's own
/// `LOGICAL_INO` answer for the same bytenrs.
pub fn probe_resolve(fd: i32, sample: usize, only: &[u64]) -> io::Result<()> {
    let mut g = BlockRefs {
        index: HashMap::new(),
        refs: Vec::new(),
        memo: HashMap::new(),
        full: HashSet::new(),
    };
    collect_block_refs(fd, &mut g)?;
    eprintln!("block graph: {} blocks, {} references", g.index.len(), g.refs.len());

    let mut out: Vec<String> = Vec::new();
    let mut seen = 0usize;
    let mut cur = u64::MAX;
    let mut cur_len = 0u64;
    let mut cur_refs: Vec<(u32, u64)> = Vec::new();
    let mut flush = |cur: u64, len: u64, refs: &Vec<(u32, u64)>, out: &mut Vec<String>| {
        if refs.is_empty() {
            return;
        }
        let mut parts: Vec<String> = Vec::new();
        for &(ty, v) in refs.iter() {
            if ty == 178 {
                parts.push(format!("dref {v}"));
            } else {
                parts.push(format!("parent {v}"));
            }
        }
        out.push(format!("{cur} {len} {}", parts.join(" ")));
    };
    btrfs::for_each_extent_tree_item(fd, &mut |bytenr, ty, key_off, payload| {
        if bytenr != cur {
            let wanted = only.is_empty() || only.contains(&cur);
            if cur_refs.len() > 0 && (wanted || (only.is_empty() && seen % sample == 0)) {
                flush(cur, cur_len, &cur_refs, &mut out);
            }
            if cur_refs.len() > 0 {
                seen += 1;
            }
            cur = bytenr;
            cur_len = key_off;
            cur_refs.clear();
        }
        match ty {
            btrfs::BTRFS_EXTENT_ITEM_KEY | btrfs::BTRFS_METADATA_ITEM_KEY => {
                let Some(h) = btrfs::parse_extent_item_hdr(payload) else {
                    return;
                };
                if btrfs::is_metadata_extent(ty, &h) {
                    return;
                }
                btrfs::for_each_inline_ref(ty, payload, &mut |ty, body| match ty {
                    btrfs::BTRFS_EXTENT_DATA_REF_KEY => {
                        let root = u64::from_le_bytes(body[..8].try_into().unwrap());
                        cur_refs.push((178, root));
                    }
                    btrfs::BTRFS_SHARED_DATA_REF_KEY => {
                        if let Some(s) = btrfs::parse_shared_data_ref(body) {
                            cur_refs.push((184, s.parent));
                        }
                    }
                    _ => {}
                });
            }
            _ => {}
        }
    })?;
    flush(cur, cur_len, &cur_refs, &mut out);
    for line in &out {
        let f: Vec<&str> = line.split(' ').collect();
        let mut roots: Vec<u64> = Vec::new();
        let mut i = 2;
        while i + 1 < f.len() {
            let value: u64 = f[i + 1].parse().unwrap();
            if f[i] == "dref" {
                roots.push(value);
            } else {
                roots.extend(g.roots_of(value));
            }
            i += 2;
        }
        roots.sort_unstable();
        roots.dedup();
        println!("{} {} {:?}", f[0], f[1], roots);
    }
    Ok(())
}

/// Development aid: what a type-restricted sweep of a subvolume costs against
/// the full enumeration the M2 walk does. This is the number that decides how
/// expensive "collapse the snapshots" is.
pub fn probe_sweep(fd: i32, root_id: u64) -> io::Result<()> {
    let t = std::time::Instant::now();
    let (mut items, mut bytes) = (0u64, 0u64);
    btrfs::for_each_fs_item(fd, root_id, &mut |_o, _t, _k, p| {
        items += 1;
        bytes += p.len() as u64;
    })?;
    let full = t.elapsed();
    let t = std::time::Instant::now();
    let (mut eitems, mut ebytes) = (0u64, 0u64);
    btrfs::for_each_fs_item_typed(fd, root_id, btrfs::BTRFS_EXTENT_DATA_KEY, &mut |_o, ty, _k, p| {
        if ty == btrfs::BTRFS_EXTENT_DATA_KEY {
            eitems += 1;
            ebytes += p.len() as u64;
        }
    })?;
    let typed = t.elapsed();
    println!(
        "subvolume {root_id}: full sweep {items} items {:.1} MiB in {:.2}s; EXTENT_DATA only {eitems} items {:.1} MiB in {:.2}s ({:.0}% of the time)",
        bytes as f64 / 1048576.0,
        full.as_secs_f64(),
        ebytes as f64 / 1048576.0,
        typed.as_secs_f64(),
        100.0 * typed.as_secs_f64() / full.as_secs_f64().max(1e-9)
    );
    Ok(())
}
