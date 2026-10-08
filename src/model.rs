//! The usage model: one physical extent table plus a node tree, from which
//! every per-directory number is derived.
//!
//! Metrics, per node (files hold their own, directories hold their subtree):
//!
//! * `owned`        bytes whose *semantic owner* is here. A reference counts
//!                  in full when its subvolume class is the extent's priority
//!                  class (live data first, snapshots only for extents the
//!                  live tree no longer references), split evenly between
//!                  same-class references. Additive: the tree sums to the true
//!                  physical usage.
//! * `compsize`     (dirs) Σ extent size over extents referenced anywhere
//!                  below - exactly what `compsize <dir>` prints.
//! * `freeable`     (dirs) Σ extent size over extents whose references are all
//!                  below - what deleting this directory would free.
//! * `referenced`   Σ bytes the files claim (du/baobab's number).
//! * `uncomp`       owned/uncompressed, deduped the same way as `owned`.

use std::collections::HashMap;

pub const KIND_DIR: u8 = 0;
pub const KIND_FILE: u8 = 1;

pub const FLAG_ROOT: u8 = 1 << 0; // synthetic root of the whole scan
pub const FLAG_SUBVOL: u8 = 1 << 1; // directory is a subvolume root
pub const FLAG_SNAPSHOT: u8 = 1 << 2; // that subvolume is a snapshot (has parent_uuid)
pub const FLAG_READONLY: u8 = 1 << 3; // subvolume is read-only
pub const FLAG_SNAP_CLASS: u8 = 1 << 4; // node lives inside a snapshot-class subvolume
/// a snapshot-class subvolume that was *not* walked: the node stands for the
/// whole subtree. Its references come from the extent tree, so its interior is
/// not browsable and its own metrics are the subtree's aggregate.
pub const FLAG_COLLAPSED: u8 = 1 << 5;

pub const NO_NODE: u32 = u32::MAX;

const _: () = {
    assert!(std::mem::size_of::<Node>() == 72);
    assert!(std::mem::size_of::<DirExtra>() == 64);
};

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Node {
    pub parent: u32,
    pub name_off: u32,
    pub first_child: u32,
    pub nchildren: u32,
    pub first_ref: u32,
    pub nrefs: u32,
    /// dir: index into `dirs`; file: ref block index
    pub extra: u32,
    pub mtime: u32,
    pub owned: u64,
    pub referenced: u64,
    pub uncomp: u64,
    pub name_len: u16,
    pub inline_disk: u16,
    pub kind: u8,
    pub flags: u8,
    pub inline_ram: u16,
    /// inode creation time (unix seconds): with the subvolume's age this is the
    /// ownership key - the oldest holder of an extent owns it
    pub btime: u32,
    /// sub-second part of the birth time, nanoseconds >> 14 (~16 us steps, so
    /// files created microseconds apart still order correctly). Fits in what
    /// used to be struct padding.
    pub btime_ns: u16,
    /// declared so the struct has no implicit padding: these bytes go into the
    /// cache file, and compiler padding is not guaranteed to be zeroed, which
    /// made otherwise identical scans differ
    pub _pad: u16,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct DirExtra {
    /// Σ extent size over extents referenced anywhere below (compsize's TOTAL).
    pub compsize: u64,
    /// Σ extent size over extents referenced only below (delete-to-free).
    pub freeable: u64,
    /// Deduped uncompressed size of everything referenced below.
    pub uncomp_reach: u64,
    pub nfiles: u64,
    pub ndirs: u64,
    /// inline extents live in metadata and are always private to one file.
    pub inline_disk_sub: u64,
    pub inline_ram_sub: u64,
    pub subvol: u16,
    pub depth: u16,
    pub _pad: u32,
}

#[derive(Clone, Copy, Default)]
pub struct ExtentInfo {
    pub disk: u64,
    pub ram: u64,
    /// smallest (birth time, subvolume creation time) seen for this extent,
    /// tracked incrementally during the walk
    pub win_btime: u32,
    pub win_btime_ns: u16,
    pub win_otime: u64,
    /// how many references share that smallest key
    pub win_count: u32,
    /// attribution cursor, used only while walking refs in file order
    pub assigned: u32,
}

pub type ExtentMap = HashMap<u32, ExtentInfo>;

#[derive(Clone, Default)]
pub struct Subvol {
    pub id: u64,
    pub uuid: [u8; 16],
    pub parent_uuid: [u8; 16],
    pub flags: u64,
    /// subvolume creation time: the tiebreak when two holders share a file's
    /// birth time, which is exactly the snapshot case
    pub otime: u64,
    pub name: Vec<u8>,
    pub path: Vec<u8>,
    pub seen: bool,
    /// parent_uuid resolves to a subvolume that exists on this filesystem
    pub snapshot: bool,
}

impl Subvol {
    pub fn is_snapshot(&self) -> bool {
        self.snapshot
    }
    /// The flags are `btrfs_root_item.flags`, where read-only is **bit 0**
    /// (`BTRFS_ROOT_SUBVOL_RDONLY`). Bit 1 is `BTRFS_SUBVOL_RDONLY` from the
    /// *setflags* ioctl - a different constant in a different namespace, and
    /// testing it silently reported every subvolume, snapshots included, as
    /// writable.
    pub fn is_readonly(&self) -> bool {
        self.flags & 1 != 0
    }
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub names: Vec<u8>,
    pub dirs: Vec<DirExtra>,
    pub roots: Vec<u32>,
    pub root: u32,
    pub subvols: Vec<Subvol>,
    pub inventory: Vec<crate::btrfs::RootItem>,
    pub warnings: Vec<String>,
    pub stats: crate::walk::ScanStats,
    pub scanned_at: u64,
    pub scanned_roots: Vec<Vec<u8>>,
}

impl Tree {
    pub fn node(&self, id: u32) -> &Node {
        &self.nodes[id as usize]
    }
    pub fn name(&self, id: u32) -> &[u8] {
        let n = self.node(id);
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }
    pub fn dir(&self, id: u32) -> &DirExtra {
        &self.dirs[self.node(id).extra as usize]
    }
    pub fn dir_mut(&mut self, id: u32) -> &mut DirExtra {
        let e = self.node(id).extra as usize;
        &mut self.dirs[e]
    }
    pub fn is_dir(&self, id: u32) -> bool {
        self.node(id).kind == KIND_DIR
    }
    pub fn child_ids(&self, id: u32) -> std::ops::Range<u32> {
        let n = self.node(id);
        n.first_child..n.first_child + n.nchildren
    }
    /// Path bytes of a node, from the synthetic root down.
    pub fn path_of(&self, id: u32) -> Vec<u8> {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut cur = id;
        while cur != NO_NODE && cur != self.root {
            parts.push(self.name(cur));
            cur = self.node(cur).parent;
        }
        let mut out = Vec::new();
        for p in parts.iter().rev() {
            out.push(b'/');
            out.extend_from_slice(p);
        }
        if out.is_empty() {
            out.push(b'/');
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Attribution passes
// ---------------------------------------------------------------------------

impl Tree {
    /// Split every extent between its references and give each file its
    /// `owned` / `uncomp` share.
    ///
    /// The ownership rule: every reference carries a key of (inode birth time,
    /// creation time of the subvolume it lives in), and the smallest key owns
    /// the extent. A reference from inside a snapshot-class subvolume has **no**
    /// birth time - its subvolume's creation time stands for it instead - so a
    /// snapshot owns only what no walked tree still holds, and among snapshots
    /// the oldest is the custodian. A reflink copy within the live tree is
    /// strictly younger than its source, whichever subvolume it landed in, so
    /// the source keeps ownership. References that share the smallest key split
    /// the extent evenly, and the integer remainder goes to the last of them, so
    /// every subtree's total still equals the real physical usage.
    pub fn attribute_owned(
        &mut self,
        refs: &[Vec<u64>],
        extents: &mut ExtentMap,
        subvol_otime: &[u64],
    ) {
        for e in extents.values_mut() {
            e.assigned = 0;
        }
        for i in 0..self.nodes.len() {
            let (kind, nrefs, first_ref, block, inline_disk, inline_ram, btime, btime_ns, parent, flags) = {
                let n = &self.nodes[i];
                (
                    n.kind,
                    n.nrefs as usize,
                    n.first_ref as usize,
                    n.extra as usize,
                    n.inline_disk as u64,
                    n.inline_ram as u64,
                    n.btime,
                    n.btime_ns,
                    n.parent,
                    n.flags,
                )
            };
            let collapsed = flags & FLAG_COLLAPSED != 0;
            if kind != KIND_FILE && !collapsed {
                continue;
            }
            // A file's `extra` is its reference block; a directory's is its
            // dir index - so a collapsed subvolume, which is a directory, still
            // needs `extra` to find its own subvolume. Its references were
            // appended to the one shared reference block.
            let ref_block = if collapsed { 0 } else { block };
            let ctime = if collapsed {
                // A collapsed subvolume stands in for every file it holds, so
                // its key cannot be any one file's birth time. It carries
                // `u32::MAX` instead, which is the honest statement: nothing is
                // known about the age of the files inside, so it owns only what
                // no walked (live) holder claims. Among collapsed subvolumes
                // the key then ties on birth time and the *subvolume* ages
                // decide - which is exactly what the file backend does for a
                // snapshot, whose files are shared with its parent and
                // therefore tie on birth time too.
                *subvol_otime
                    .get(self.dirs[block].subvol as usize)
                    .unwrap_or(&0)
            } else {
                let p = &self.nodes[parent as usize];
                let sv = if p.kind == KIND_DIR {
                    self.dirs[p.extra as usize].subvol
                } else {
                    0
                };
                *subvol_otime.get(sv as usize).unwrap_or(&0)
            };
            // Inline payloads are private to this file and need no splitting.
            let mut owned = inline_disk;
            let mut uncomp = inline_ram;
            for r in &refs[ref_block][first_ref..first_ref + nrefs] {
                let Some(e) = extents.get_mut(&((*r >> 32) as u32)) else {
                    continue;
                };
                // only the holders of the extent's smallest key own it
                if btime != e.win_btime
                    || btime_ns != e.win_btime_ns
                    || ctime != e.win_otime
                    || e.win_count == 0
                {
                    continue;
                }
                let n = e.win_count as u64;
                let a = e.assigned;
                e.assigned += 1;
                if a as u64 + 1 == n {
                    owned += e.disk - (n - 1) * (e.disk / n);
                    uncomp += e.ram - (n - 1) * (e.ram / n);
                } else {
                    owned += e.disk / n;
                    uncomp += e.ram / n;
                }
            }
            let n = &mut self.nodes[i];
            n.owned = owned;
            n.uncomp = uncomp;
        }
    }

    /// `compsize` (reach) and `freeable` (exclusive) for every directory, from
    /// references grouped by extent. Unique extents - the common case - are a
    /// single ancestor walk shared by both metrics.
    pub fn attribute_reach(&mut self, refs: &mut [Vec<u64>], extents: &ExtentMap) {
        let nblocks = refs.len();
        for r in refs.iter_mut() {
            r.sort_unstable();
        }
        let mut heads = vec![0usize; nblocks];
        // generation-stamped marker array: much cheaper than a set per extent
        let mut stamp = vec![0u32; self.nodes.len()];
        let mut gen: u32 = 0;
        let mut union_list: Vec<u32> = Vec::new();
        let mut owners: Vec<u32> = Vec::new();

        loop {
            // smallest page still available across all sorted blocks
            let mut page: Option<u32> = None;
            for b in 0..nblocks {
                if heads[b] < refs[b].len() {
                    let p = (refs[b][heads[b]] >> 32) as u32;
                    page = Some(match page {
                        Some(cur) => cur.min(p),
                        None => p,
                    });
                }
            }
            let Some(page) = page else { break };

            owners.clear();
            for b in 0..nblocks {
                while heads[b] < refs[b].len() && (refs[b][heads[b]] >> 32) as u32 == page {
                    owners.push((refs[b][heads[b]] & 0xffff_ffff) as u32);
                    heads[b] += 1;
                }
            }
            if owners.is_empty() {
                continue;
            }
            let Some(e) = extents.get(&page) else { continue };

            gen = gen.wrapping_add(1);
            if gen == 0 {
                stamp.fill(0);
                gen = 1;
            }

            // reach: union of the ancestor chains of every reference
            union_list.clear();
            for &owner in &owners {
                let mut cur = self.dir_start(owner);
                while cur != NO_NODE && stamp[cur as usize] != gen {
                    stamp[cur as usize] = gen;
                    union_list.push(cur);
                    cur = self.nodes[cur as usize].parent;
                }
            }
            for &d in &union_list {
                let dd = self.dir_mut(d);
                dd.compsize += e.disk;
                dd.uncomp_reach += e.ram;
            }

            // freeable: the chain above the lowest common ancestor of the refs
            if owners.len() == 1 {
                for &d in &union_list {
                    self.dir_mut(d).freeable += e.disk;
                }
            } else {
                let mut lca = self.dir_start(owners[0]);
                for &owner in &owners[1..] {
                    let d = self.dir_start(owner);
                    if lca != d {
                        lca = self.lca(lca, d);
                    }
                }
                let mut cur = lca;
                while cur != NO_NODE {
                    self.dir_mut(cur).freeable += e.disk;
                    cur = self.nodes[cur as usize].parent;
                }
            }
        }
    }

    /// Lowest common ancestor of two directory nodes (depth-based).
    fn lca(&self, a: u32, b: u32) -> u32 {
        if a == NO_NODE {
            return b;
        }
        if b == NO_NODE {
            return a;
        }
        let (mut a, mut b) = (a, b);
        let mut da = self.depth_of(a);
        let mut db = self.depth_of(b);
        while da > db {
            a = self.nodes[a as usize].parent;
            da -= 1;
        }
        while db > da {
            b = self.nodes[b as usize].parent;
            db -= 1;
        }
        while a != b {
            a = self.nodes[a as usize].parent;
            b = self.nodes[b as usize].parent;
        }
        a
    }

    fn depth_of(&self, d: u32) -> u32 {
        let n = &self.nodes[d as usize];
        if n.kind == KIND_DIR {
            self.dirs[n.extra as usize].depth as u32
        } else {
            0
        }
    }

    /// Directory node that should collect a reference owned by `owner`.
    fn dir_start(&self, owner: u32) -> u32 {
        let n = &self.nodes[owner as usize];
        if n.kind == KIND_DIR {
            owner
        } else {
            n.parent
        }
    }

    /// Roll child values into parents. Nodes are ordered parent-before-child,
    /// so a single reverse pass is a valid bottom-up traversal.
    pub fn rollup(&mut self) {
        for i in (1..self.nodes.len()).rev() {
            let parent = self.nodes[i].parent as usize;
            let (left, right) = self.nodes.split_at_mut(i);
            let child = &right[0];
            let p = &mut left[parent];
            p.owned += child.owned;
            p.referenced += child.referenced;
            p.uncomp += child.uncomp;
            if child.kind == KIND_DIR {
                let cd = self.dirs[child.extra as usize];
                let pd = &mut self.dirs[p.extra as usize];
                pd.nfiles += cd.nfiles;
                pd.ndirs += cd.ndirs + 1;
                pd.inline_disk_sub += cd.inline_disk_sub + child.inline_disk as u64;
                pd.inline_ram_sub += cd.inline_ram_sub + child.inline_ram as u64;
            } else {
                let pd = &mut self.dirs[p.extra as usize];
                pd.nfiles += 1;
                pd.inline_disk_sub += child.inline_disk as u64;
                pd.inline_ram_sub += child.inline_ram as u64;
            }
        }
        // inline payloads are private, so they also count as reach and as
        // freeable for every directory above them
        for d in self.dirs.iter_mut() {
            d.compsize += d.inline_disk_sub;
            d.freeable += d.inline_disk_sub;
            d.uncomp_reach += d.inline_ram_sub;
        }
    }
}

/// Bytes for verification runs, human-readable otherwise.
pub fn size(bytes: u64, raw: bool) -> String {
    if raw {
        bytes.to_string()
    } else {
        human(bytes)
    }
}

/// Which number the chart and the list are showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Owned,
    Freeable,
    Compsize,
    Referenced,
    Uncompressed,
}

impl Metric {
    pub const ALL: [Metric; 5] = [
        Metric::Owned,
        Metric::Freeable,
        Metric::Compsize,
        Metric::Referenced,
        Metric::Uncompressed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Metric::Owned => "owned",
            Metric::Freeable => "freeable",
            Metric::Compsize => "compsize",
            Metric::Referenced => "referenced",
            Metric::Uncompressed => "uncompressed",
        }
    }

    /// Short explanation, shown under the selector.
    pub fn help(self) -> &'static str {
        match self {
            Metric::Owned => "the oldest holder of a shared extent owns it: file birth time first, then the age of the subvolume it lives in. A snapshot has no birth time of its own to offer - only its creation time - so it owns what the live trees let go of and nothing they still hold, and among snapshots the oldest is the custodian. A reflink copy in the live tree is younger than its source and owns only what is unique to it. Adds up.", 
            Metric::Freeable => "what would actually be freed by deleting this subtree",
            Metric::Compsize => "every extent referenced anywhere below, counted once - what compsize prints for that path",
            Metric::Referenced => "what the files claim, the number du and baobab show",
            Metric::Uncompressed => "deduplicated bytes before compression",
        }
    }

    /// The value for a node. Per-file reach/exclusive are not stored, so a file
    /// shows its owned share for those two.
    pub fn value(self, tree: &Tree, id: u32) -> u64 {
        let n = tree.node(id);
        if n.kind == KIND_FILE {
            return match self {
                Metric::Referenced => n.referenced,
                Metric::Uncompressed => n.uncomp,
                _ => n.owned,
            };
        }
        let d = tree.dir(id);
        match self {
            Metric::Owned => n.owned,
            Metric::Freeable => d.freeable,
            Metric::Compsize => d.compsize,
            Metric::Referenced => n.referenced,
            Metric::Uncompressed => d.uncomp_reach,
        }
    }
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}
