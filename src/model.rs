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

pub const NO_NODE: u32 = u32::MAX;

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
}

#[derive(Clone, Copy, Default)]
pub struct ExtentInfo {
    pub disk: u64,
    pub ram: u64,
    pub count_live: u32,
    pub count_snap: u32,
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
    pub ctime: u64,
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
    pub fn is_readonly(&self) -> bool {
        self.flags & 0b10 != 0
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
    /// `owned` / `uncomp` share. Exact: the remainder of the integer division
    /// goes to the last reference, so a subtree's total equals the true sum.
    pub fn attribute_owned(&mut self, refs: &[Vec<u64>], extents: &mut ExtentMap) {
        for e in extents.values_mut() {
            e.assigned = 0;
        }
        for i in 0..self.nodes.len() {
            let (kind, flags, nrefs, first_ref, block, inline_disk, inline_ram) = {
                let n = &self.nodes[i];
                (
                    n.kind,
                    n.flags,
                    n.nrefs as usize,
                    n.first_ref as usize,
                    n.extra as usize,
                    n.inline_disk as u64,
                    n.inline_ram as u64,
                )
            };
            if kind != KIND_FILE {
                continue;
            }
            let class_snap = flags & FLAG_SNAP_CLASS != 0;
            // Inline payloads are private to this file and need no splitting -
            // and a file can have inline data and no regular extents at all.
            let mut owned = inline_disk;
            let mut uncomp = inline_ram;
            for r in &refs[block][first_ref..first_ref + nrefs] {
                let Some(e) = extents.get_mut(&((*r >> 32) as u32)) else {
                    continue;
                };
                let (count, take) = if e.count_live > 0 {
                    (e.count_live, !class_snap)
                } else {
                    (e.count_snap, class_snap)
                };
                if !take || count == 0 {
                    continue;
                }
                let a = e.assigned;
                e.assigned += 1;
                let count = count as u64;
                if a as u64 + 1 == count {
                    owned += e.disk - (count - 1) * (e.disk / count);
                    uncomp += e.ram - (count - 1) * (e.ram / count);
                } else {
                    owned += e.disk / count;
                    uncomp += e.ram / count;
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
