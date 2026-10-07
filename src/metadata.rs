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
use std::collections::{HashMap, VecDeque};
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
) -> io::Result<Collected> {
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
                    btime: item.otime_sec.min(u32::MAX as u64) as u32,
                    btime_ns: (item.otime_nsec >> 14) as u16,
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
                let Some(item) = btrfs::parse_extent_item(payload, key_off) else {
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
fn build(c: &mut Collector, roots: &[Root]) -> (Tree, Vec<u64>) {
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
        let mut kids: Vec<(usize, u64, Vec<u8>, u8)> = Vec::new();
        {
            let entries = &c.subs[ci].entries;
            let names = &c.subs[ci].names;
            let lo = entries.partition_point(|e| e.parent < inode);
            let hi = entries.partition_point(|e| e.parent <= inode);
            for e in &entries[lo..hi] {
            let child_name = names[e.name_off as usize..e.name_off as usize + e.name_len as usize].to_vec();
            let (child_ci, child_inode, child_flags) = if e.loc_type == 132 {
                // a directory entry pointing at another subvolume's root
                let nested = c.subs[ci]
                    .nested
                    .iter()
                    .find(|(_, dirid, name)| *dirid == inode && name.as_slice() == child_name.as_slice());
                match nested {
                    Some((root_id, _, _)) => {
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
                                (child_ci, btrfs::BTRFS_FIRST_FREE_OBJECTID, f)
                            }
                            None => continue,
                        }
                    }
                    None => continue,
                }
            } else {
                (ci, e.child, 0)
            };
            kids.push((child_ci, child_inode, child_name, child_flags));
        }
        }
        for (child_ci, child_inode, child_name, child_flags) in kids {
            let Ok(pos) = c.subs[child_ci].recs.binary_search_by_key(&child_inode, |r| r.inode) else {
                continue;
            };
            let rec = c.subs[child_ci].recs[pos];
            let kind = match rec.mode & btrfs::S_IFMT {
                btrfs::S_IFDIR => KIND_DIR,
                btrfs::S_IFREG => KIND_FILE,
                _ => continue, // symlinks and specials, as in the file backend
            };
            // depth comes from the parent, which is already placed
            let depth = if node_idx == 0 {
                1
            } else {
                tree.dirs[tree.nodes[node_idx as usize].extra as usize].depth + 1
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

    (tree, final_refs)
}

impl Collected {
    fn subvol_snapshot(&self, table: &[Subvol]) -> bool {
        table[self.subvol as usize].is_snapshot()
    }
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
        let col = collect_subvolume(c, fd, root_id, subvol, otime)?;
        for (child_root, _dirid, name) in &col.nested {
            // the nested subvolume is reachable as a directory of this one, so
            // its identity is one open() away - no need to decode root items
            let mut child_path = path.clone();
            if !child_path.ends_with(b"/") {
                child_path.push(b'/');
            }
            child_path.extend_from_slice(name);
            let Ok(child) = open_dir(&child_path) else { continue };
            let Ok(info) = btrfs::get_subvol_info(std::os::fd::AsRawFd::as_raw_fd(&child)) else {
                continue;
            };
            if info.treeid != *child_root {
                continue;
            }
            let child_subvol = c.intern_subvol(&info, inventory, &child_path);
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
    };

    // inventory of every subvolume, for snapshot classification
    let first = opts.roots.first().cloned().unwrap_or_else(|| b"/".to_vec());
    let f = open_dir(&first)?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&f);
    let inventory = btrfs::list_subvolumes(fd).unwrap_or_default();


    let mut roots = Vec::new();
    for path in &opts.roots {
        let file = open_dir(path)?;
        let info = btrfs::get_subvol_info(std::os::fd::AsRawFd::as_raw_fd(&file))?;
        let subvol = c.intern_subvol(&info, &inventory, path);
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
        collect_all(&mut c, fd, &inventory, root)?;
    }

    let (mut tree, final_refs) = build(&mut c, &roots);
    // the assembled array, whose references name their owner node
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


