// SPDX-License-Identifier: GPL-2.0-or-later

//! Parallel one-pass walker.
//!
//! One pass over the tree collects, for every regular file, the physical
//! extents it references (BTRFS_IOC_TREE_SEARCH_V2 per inode, exactly like
//! compsize) plus the directory structure. Everything the report and the GUI
//! need is derived from that single pass.
//!
//! Node indices are assigned *after* the walk (flatten pass), in an order that
//! keeps every directory before its children and each directory's children
//! contiguous - which is what makes the reverse-order rollup and the
//! per-directory child slices cheap.

use crate::btrfs::{self, ExtentItem, Searcher, BTRFS_FILE_EXTENT_INLINE, O_DIRECTORY, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK};
use crate::model::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirEntryExt, MetadataExt, OpenOptionsExt};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const ENOENT: i32 = 2;
const ENXIO: i32 = 6;
const EACCES: i32 = 13;
const ENODEV: i32 = 19;
const ELOOP: i32 = 40;
const ENOMEDIUM: i32 = 123;
const ENOTTY: i32 = 25;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

/// Live counters the GUI polls while a scan runs in the background.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub refs: AtomicU64,
    pub cancel: AtomicBool,
    pub running: AtomicBool,
    pub current: Mutex<Vec<u8>>,
}

impl Progress {
    pub fn snapshot(&self) -> (u64, u64, u64, bool, String) {
        (
            self.files.load(Ordering::Relaxed),
            self.dirs.load(Ordering::Relaxed),
            self.refs.load(Ordering::Relaxed),
            self.running.load(Ordering::Relaxed),
            String::from_utf8_lossy(&self.current.lock().unwrap()).into_owned(),
        )
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub struct ScanOptions {
    pub roots: Vec<Vec<u8>>,
    pub threads: usize,
    pub follow_symlinks: bool,
    pub buf_size: usize,
    pub excludes: Vec<Vec<u8>>,
    pub progress: bool,
    /// drop timings and warnings from the saved scan, so two scans of the same
    /// tree produce byte-identical files
    pub reproducible: bool,
    /// shared counters for a GUI; `scan` fills this in itself when absent
    pub live: Option<Arc<Progress>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            roots: Vec::new(),
            threads: 0,
            follow_symlinks: false,
            buf_size: 1 << 20,
            excludes: Vec::new(),
            progress: false,
            reproducible: false,
            live: None,
        }
    }
}

pub struct DirResult {
    pub entries: Vec<Node>,
    pub names: Vec<u8>,
    pub subvol: u16,
    /// false for a directory the walk did not enter (a mount whose content is
    /// read elsewhere, or is not this filesystem's): it still stands in the
    /// tree - and in the dirs table, since a node's subvolume and depth come
    /// from there - but it has no children, so its first_child stays zero
    /// exactly as the tree reader leaves it.
    pub entered: bool,
}

struct WorkItem {
    dir_id: u32,
    path: Box<[u8]>,
    subvol: u16,
    snap_class: bool,
    dev: u64,
}

#[derive(Default, Clone)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    pub extents: u64,
    pub refs: u64,
    pub subvolumes: u64,
    pub skipped_nonbtrfs: u64,
    /// subvolumes skipped because they belong to a different btrfs filesystem
    pub skipped_otherfs: u64,
    pub unreadable: u64,
    /// files whose extent ioctl could not reuse the directory descriptor
    pub fd_fallback: u64,
    pub elapsed: Duration,
}

/// One mount in this process's mount namespace.
struct Mount {
    /// Mount ID. The kernel hands these out in mount order, so a lower id is an
    /// older mount and no two are equal.
    id: u64,
    /// Where it is mounted, unescaped and absolute.
    path: Vec<u8>,
    /// The mounted filesystem's type, as the kernel spells it ("btrfs", "tmpfs").
    fs_type: Vec<u8>,
}

/// The mounts under the scan roots, decided once before the walk.
///
/// A path walk meets a mount where the tree reader cannot see one at all, but
/// it must still know which of several mounts of one subvolume is the one that
/// reads it, and it must recognize a mount whose filesystem is not this one
/// before it walks into it: the containing tree holds an ordinary directory
/// where a mount is, and entering it would read a stranger's tree.
struct Mounts {
    /// Every mount point under the scan roots, spelled the way the walk spells
    /// paths, so a directory being processed can be looked up by the path it
    /// already has.
    points: HashSet<Vec<u8>>,
    /// uuid -> the path of the mount that reads that subvolume: the oldest
    /// (lowest mount id) of its mounts under the roots, when that mount is a
    /// btrfs subvolume root of this filesystem. Subvolumes mounted at several
    /// paths are read once, at their oldest mount, exactly as the tree reader
    /// picks them.
    chosen: HashMap<[u8; 16], Vec<u8>>,
}

impl Mounts {
    fn is_point(&self, path: &[u8]) -> bool {
        self.points.contains(path)
    }

    /// The path at which the subvolume with this uuid is read, if this scan
    /// reads it at all.
    fn reads(&self, uuid: &[u8; 16]) -> Option<&[u8]> {
        self.chosen.get(uuid).map(|p| p.as_slice())
    }
}

/// Read `/proc/self/mountinfo`, oldest mount first.
///
/// A line is `id parent major:minor root mountpoint options [optional...] -
/// fstype source superopts`, with an unknown number of optional fields, so the
/// type is read from the first "-" field rather than from a fixed position. The
/// mount point is field 5, octal-escaped for space, tab, newline and backslash.
/// Only the id, the path and the type are needed: mount ids are handed out in
/// mount order, which is what makes "the oldest mount of a subvolume" a fact
/// the table can answer.
fn read_mounts() -> Vec<Mount> {
    let mut out: Vec<Mount> = Vec::new();
    let Ok(text) = fs::read("/proc/self/mountinfo") else {
        return out;
    };
    for line in text.split(|b| *b == b'\n') {
        let f: Vec<&[u8]> = line.split(|b| *b == b' ').collect();
        if f.len() < 6 {
            continue;
        }
        let Some(sep) = f.iter().position(|x| *x == b"-") else {
            continue;
        };
        let (Some(id), Some(fs_type)) = (
            std::str::from_utf8(f[0]).ok().and_then(|s| s.parse().ok()),
            f.get(sep + 1),
        ) else {
            continue;
        };
        out.push(Mount {
            id,
            path: unescape(f[4]),
            fs_type: fs_type.to_vec(),
        });
    }
    out.sort_by_key(|m| m.id);
    out
}

/// mountinfo escapes space, tab, newline and backslash as `\NNN` octal, so a
/// path with a space in it does not split into two fields.
fn unescape(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' && i + 3 < s.len() {
            let d = &s[i + 1..i + 4];
            if d.iter().all(|c| (b'0'..=b'7').contains(c)) {
                out.push((d[0] - b'0') * 64 + (d[1] - b'0') * 8 + (d[2] - b'0'));
                i += 4;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// Is `path` inside `root` (which is a directory, not a file)? Component-wise,
/// so `/home2` is not inside `/home`.
fn under(path: &[u8], root: &[u8]) -> bool {
    let Some(rest) = path.strip_prefix(root) else {
        return false;
    };
    rest.is_empty() || root.ends_with(b"/") || rest.starts_with(b"/")
}

/// Find the mounts under the scan roots, and which mount reads each mounted
/// subvolume.
///
/// One open and two ioctls per mount point - never per file - and a mount whose
/// type is not btrfs is not even opened. A scan root is not a mount *under*
/// itself: it is entered because the user asked for it, and its node says so
/// instead.
fn discover_mounts(roots: &[Vec<u8>], fs_id: ([u8; 16], [u8; 16]), excludes: &[Vec<u8>]) -> Mounts {
    let mut mounts = Mounts {
        points: HashSet::new(),
        chosen: HashMap::new(),
    };
    // Roots are compared canonicalized - a mount point is absolute and spelled
    // the kernel's way - but mounts are keyed by the path the walk will build,
    // which follows the spelling the user gave.
    let canon: Vec<Vec<u8>> = roots
        .iter()
        .map(|r| {
            fs::canonicalize(OsStr::from_bytes(r))
                .map(|p| p.as_os_str().as_bytes().to_vec())
                .unwrap_or_else(|_| r.clone())
        })
        .collect();
    // Oldest first: the first mount of a subvolume is the one that reads it.
    for m in read_mounts() {
        let Some(ri) = canon.iter().position(|r| under(&m.path, r)) else {
            continue;
        };
        if m.path == canon[ri] {
            continue;
        }
        let rel = &m.path[canon[ri].len()..];
        let mut path = roots[ri].clone();
        path.push(b'/');
        path.extend_from_slice(rel.strip_prefix(b"/").unwrap_or(rel));
        if is_excluded_path(&path, excludes) {
            continue;
        }
        if m.fs_type == b"btrfs" {
            if let Ok(f) = open_dir(&path) {
                let fd = f.as_raw_fd();
                let ours = btrfs::fs_identity(fd)
                    .map(|id| id == fs_id)
                    .unwrap_or(false);
                if ours {
                    if let Ok(info) = btrfs::get_subvol_info(fd) {
                        mounts.chosen.entry(info.uuid).or_insert_with(|| path.clone());
                    }
                }
            }
        }
        mounts.points.insert(path);
    }
    mounts
}

struct Shared {
    queue: Mutex<VecDeque<WorkItem>>,
    cv: Condvar,
    pending: AtomicUsize,
    next_dir_id: AtomicU32,
    slots: Mutex<Vec<Option<DirResult>>>,
    subvols: Mutex<Vec<Subvol>>,
    /// the mounts under the roots, decided before the walk started
    mounts: Mounts,
    /// every subvolume on the filesystem (uuid, parent_uuid, treeid)
    inventory: Vec<btrfs::RootItem>,
    visited_dirs: Mutex<HashSet<(u64, u64)>>,
    /// subvolume UUIDs already walked, so the same subvolume mounted at several
    /// paths is not scanned (and its references not multiplied) once per mount
    visited_subvols: Mutex<HashSet<[u8; 16]>>,
    /// identity of the filesystem the scan started on; other btrfs filesystems
    /// found under the scanned paths are skipped, since extent addresses are
    /// only unique within one filesystem
    fs_id: ([u8; 16], [u8; 16]),
    warnings: Mutex<Vec<String>>,
    unreadable: AtomicU64,
    nonbtrfs: AtomicU64,
    otherfs: AtomicU64,
    no_btime: AtomicBool,
    /// how often the directory descriptor could not be used (device mismatch)
    fd_fallback: AtomicU64,
    prog: Arc<Progress>,
}

impl Shared {
    fn warn(&self, msg: String) {
        let mut w = self.warnings.lock().unwrap();
        if w.len() < 200 {
            w.push(msg);
        }
    }

    fn push_work(&self, wi: WorkItem) {
        let mut q = self.queue.lock().unwrap();
        self.pending.fetch_add(1, Ordering::SeqCst);
        q.push_back(wi);
        drop(q);
        self.cv.notify_one();
    }

    fn intern_subvol(&self, info: &btrfs::SubvolInfo, path: &[u8]) -> u16 {
        let mut table = self.subvols.lock().unwrap();
        if let Some(i) = table.iter().position(|s| s.id == info.treeid) {
            // One subvolume reachable through several paths (or given on the
            // command line twice): keep the lexicographically smallest path, so
            // the recorded one does not depend on which entry the walk reached
            // first.
            if path < table[i].path.as_slice() {
                table[i].path = path.to_vec();
            }
            return i as u16;
        }
        // A subvolume is a snapshot only when the subvolume it was snapshotted
        // from still exists. `home` here has a parent_uuid pointing at a
        // subvolume that is long gone - it is an original, not a snapshot.
        let snapshot = info.parent_uuid != [0u8; 16]
            && self.inventory.iter().any(|r| r.uuid == info.parent_uuid);
        table.push(Subvol {
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
        (table.len() - 1) as u16
    }

    /// Intern a subvolume and return what a node standing for it needs: its
    /// index, the flags of the node itself, and whether it is snapshot-class
    /// (which the caller also hands to the work item, because everything
    /// *inside* the subvolume is marked with it).
    fn subvol_node(&self, info: &btrfs::SubvolInfo, path: &[u8]) -> (u16, u8, bool) {
        let subvol = self.intern_subvol(info, path);
        let table = self.subvols.lock().unwrap();
        let s = &table[subvol as usize];
        let snap_class = s.is_snapshot();
        let mut flags = FLAG_SUBVOL;
        if s.is_readonly() {
            flags |= FLAG_READONLY;
        }
        if snap_class {
            flags |= FLAG_SNAPSHOT;
        }
        (subvol, flags, snap_class)
    }
}

/// The times of the directory a mount point covers.
///
/// A mount hides the directory it sits over: statx on the path reports the
/// mounted subvolume's root, never what the containing tree holds there - and
/// that directory is what the tree reader puts in the node for a mount it does
/// not enter. readdir hands out its inode number (the entry comes from the
/// containing tree), so one inode-item search on the containing directory's own
/// descriptor recovers the record statx cannot reach. `None` when there is no
/// btrfs tree to search, in which case the caller keeps the statx result.
fn covered_dir_times(
    w: &mut Worker,
    shared: &Shared,
    holding_subvol: u16,
    dir_fd: i32,
    ino: u64,
) -> Option<btrfs::InodeTimes> {
    if dir_fd < 0 {
        return None;
    }
    let tree_id = shared
        .subvols
        .lock()
        .unwrap()
        .get(holding_subvol as usize)?
        .id;
    let spec = btrfs::SearchSpec {
        tree_id,
        item_type: btrfs::BTRFS_INODE_ITEM_KEY,
        min_objectid: ino,
        max_objectid: ino,
    };
    let mut times = None;
    w.searcher
        .for_each_item(dir_fd, &spec, &mut |obj, _off, payload| {
            if obj == ino && times.is_none() {
                times = btrfs::parse_inode_times(payload);
            }
        })
        .ok()?;
    times
}

/// Push the node for a directory the walk does not enter - a mount whose
/// content belongs to another mount, or to a filesystem this scan does not
/// read. It is the node the walk would have pushed for that path, so its name
/// and its times are the directory the containing tree holds; it just has no
/// children.
///
/// The slot matters even though the directory holds nothing: flatten gives a
/// directory node its DirExtra - its subvolume and its depth - from a slot, and
/// taking it in the same breadth-first order as every other directory is what
/// keeps the dirs table laid out the way the tree reader lays it out.
#[allow(clippy::too_many_arguments)]
fn push_dir_not_entered(
    shared: &Shared,
    w: &mut Worker,
    result: &mut DirResult,
    item: &WorkItem,
    path: &[u8],
    name: &[u8],
    mtime: u32,
    mut flags: u8,
    nofollow: bool,
    dir_fd: i32,
    ino: u64,
) {
    let subvol = item.subvol;
    let snap_class = item.snap_class;
    if snap_class {
        flags |= FLAG_SNAP_CLASS;
    }
    let (mtime, btime, btime_ns) = match covered_dir_times(w, shared, subvol, dir_fd, ino) {
        Some(t) => (
            t.mtime_sec.min(u32::MAX as u64) as u32,
            if snap_class {
                u32::MAX
            } else {
                t.otime_sec.min(u32::MAX as u64) as u32
            },
            if snap_class {
                u16::MAX
            } else {
                (t.otime_nsec >> 14) as u16
            },
        ),
        None => {
            let (btime, btime_ns) = dir_btime(w, path, snap_class, nofollow);
            (mtime, btime, btime_ns)
        }
    };
    let dir_id = shared.next_dir_id.fetch_add(1, Ordering::Relaxed);
    store_slot(
        shared,
        dir_id,
        DirResult {
            entries: Vec::new(),
            names: Vec::new(),
            subvol,
            entered: false,
        },
    );
    // The directory count is a count of nodes, the way the tree reader counts
    // them; this node has no read_dir behind it.
    shared.prog.dirs.fetch_add(1, Ordering::Relaxed);
    let name_off = result.names.len() as u32;
    result.names.extend_from_slice(name);
    result.entries.push(Node {
        parent: NO_NODE,
        name_off,
        name_len: name.len() as u16,
        kind: KIND_DIR,
        flags,
        extra: dir_id,
        mtime,
        btime,
        btime_ns,
        ..Default::default()
    });
}

struct Worker {
    searcher: Searcher,
    refs: Vec<u64>,
    extents: ExtentMap,
    block: u32,
    /// reusable NUL-terminated copy of the current path for statx
    pathz: Vec<u8>,
}

/// `nofollow` mirrors compsize (and is the default); with --follow-symlinks the
/// entry we resolved may itself be a symlink, so it must be opened through.
fn open_file(path: &[u8], nofollow: bool) -> io::Result<File> {
    let mut flags = O_NOCTTY | O_NONBLOCK;
    if nofollow {
        flags |= O_NOFOLLOW;
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(OsStr::from_bytes(path))
}

fn open_dir(path: &[u8]) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW | O_NOCTTY | O_NONBLOCK)
        .open(OsStr::from_bytes(path))
}

/// compsize's errno policy: symlinks, devices and files that vanished are
/// silent; permission and other problems are reported once each.
fn note_open_error(shared: &Shared, path: &[u8], e: &io::Error) {
    match e.raw_os_error().unwrap_or(0) {
        ELOOP | ENXIO | ENODEV | ENOMEDIUM | ENOENT => {}
        EACCES => {
            shared.unreadable.fetch_add(1, Ordering::Relaxed);
            shared.warn(format!("{}: {e}", String::from_utf8_lossy(path)));
        }
        _ => {
            shared.unreadable.fetch_add(1, Ordering::Relaxed);
            shared.warn(format!("{}: {e} (skipped)", String::from_utf8_lossy(path)));
        }
    }
}

fn is_excluded(opts: &ScanOptions, path: &[u8]) -> bool {
    is_excluded_path(path, &opts.excludes)
}

fn is_excluded_path(path: &[u8], excludes: &[Vec<u8>]) -> bool {
    excludes.iter().any(|ex| path.starts_with(ex))
}

/// Read one regular file's extent list. Returns its node with `owned` left at
/// zero (that needs the global extent table, so it happens after the walk).
///
/// One syscall for the file itself - `statx`, which yields the birth time the
/// ownership rule keys on - plus the extent ioctl. No open/close per file: the
/// ioctl only needs *a* descriptor from the file's subvolume, so the containing
/// directory's descriptor serves every entry in it.
#[allow(clippy::too_many_arguments)]
fn scan_file(
    w: &mut Worker,
    shared: &Shared,
    path: &[u8],
    snap_class: bool,
    dir_fd: i32,
    dir_dev: u64,
    nofollow: bool,
) -> Option<Node> {
    let st = {
        w.pathz.clear();
        w.pathz.extend_from_slice(path);
        w.pathz.push(0);
        match btrfs::stat_path(&w.pathz, nofollow) {
            Ok(s) => s,
            Err(e) => {
                note_open_error(shared, path, &e);
                return None;
            }
        }
    };
    if st.mode & btrfs::S_IFMT != btrfs::S_IFREG {
        return None;
    }
    if !st.has_btime && !shared.no_btime.swap(true, Ordering::Relaxed) {
        shared.warn(
            "this filesystem does not report file birth times; ownership falls back to \
             subvolume age alone"
                .to_string(),
        );
    }
    let ino = st.ino;
    // A snapshot's files are copies of the original's, so their birth times are
    // not a fact about the data: reflinking the live file and renaming it is
    // enough to hand the live side a younger inode, which would move ownership
    // of data that never went anywhere. Such a reference therefore carries no
    // birth time at all, and only the subvolume's age decides.
    let (btime, btime_ns) = if snap_class {
        (u32::MAX, u16::MAX)
    } else {
        (st.btime.min(u32::MAX as u64) as u32, (st.btime_ns >> 14) as u16)
    };

    // A followed symlink can lead into another subvolume, where the directory's
    // descriptor would search the wrong tree; fall back to the file's own.
    let owned_fd;
    let fd = if dir_fd >= 0 && st.dev == dir_dev {
        dir_fd
    } else {
        shared.fd_fallback.fetch_add(1, Ordering::Relaxed);
        match open_file(path, nofollow) {
            Ok(f) => {
                owned_fd = f;
                owned_fd.as_raw_fd()
            }
            Err(e) => {
                note_open_error(shared, path, &e);
                return None;
            }
        }
    };

    let mut referenced = 0u64;
    let mut overflow = false;

    let Worker {
        searcher,
        refs,
        extents,
        ..
    } = w;
    let first_ref = refs.len() as u32;
    let res = searcher.for_each_extent(fd, ino, &mut |item: &ExtentItem| {
        if item.kind == BTRFS_FILE_EXTENT_INLINE {
            // An inline payload is stored inside the inode's item in the fs
            // tree: it is metadata, not a data extent. It has no extent
            // identity to share or to count, so it never becomes a reference;
            // the file still claims those bytes, so `referenced` counts them.
            referenced += item.ram_bytes;
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
        // The extent's ownership window is not decided here: it is computed
        // from the references that end up belonging to nodes, once the tree is
        // assembled (see `Tree::compute_windows`).
        extents.entry(page).or_insert_with(|| ExtentInfo {
            disk: item.disk_num_bytes,
            ram: item.ram_bytes,
            ..Default::default()
        });
        refs.push((page as u64) << 32);
        referenced += item.num_bytes;
    });
    if overflow {
        shared.warn(format!(
            "{}: extent above the 16 TiB addressing limit, not counted",
            String::from_utf8_lossy(path)
        ));
    }
    if let Err(e) = res {
        shared.unreadable.fetch_add(1, Ordering::Relaxed);
        shared.warn(format!(
            "{}: {e} (extents not counted)",
            String::from_utf8_lossy(path)
        ));
    }

    let mut flags = 0u8;
    if snap_class {
        flags |= FLAG_SNAP_CLASS;
    }
    Some(Node {
        parent: NO_NODE,
        first_ref,
        nrefs: (w.refs.len() as u32) - first_ref,
        extra: w.block,
        mtime: st.mtime.min(u32::MAX as u64) as u32,
        btime,
        btime_ns,
        referenced,
        kind: KIND_FILE,
        flags,
        ..Default::default()
    })
}

/// `statx` on a path, with the NUL-terminated copy kept in the worker's
/// reusable scratch buffer.
fn statz(w: &mut Worker, path: &[u8], nofollow: bool) -> Option<btrfs::StatInfo> {
    w.pathz.clear();
    w.pathz.extend_from_slice(path);
    w.pathz.push(0);
    btrfs::stat_path(&w.pathz, nofollow).ok()
}

/// Birth time of a directory, as the ownership key's primary part. Inside a
/// snapshot-class subvolume a directory is a copy like everything else, so it
/// carries no birth time either - which also keeps a scan's bytes from
/// depending on when a directory happened to be created.
fn dir_btime(w: &mut Worker, path: &[u8], snap_class: bool, nofollow: bool) -> (u32, u16) {
    if snap_class {
        return (u32::MAX, u16::MAX);
    }
    match statz(w, path, nofollow) {
        Some(s) => (
            s.btime.min(u32::MAX as u64) as u32,
            (s.btime_ns >> 14) as u16,
        ),
        None => (0, 0),
    }
}

fn process_dir(w: &mut Worker, shared: &Shared, opts: &ScanOptions, item: WorkItem) {
    if shared.prog.cancelled() {
        store_slot(shared, item.dir_id, DirResult {
            entries: Vec::new(),
            names: Vec::new(),
            subvol: item.subvol,
            entered: true,
        });
        return;
    }
    {
        let mut cur = shared.prog.current.lock().unwrap();
        cur.clear();
        let p = &item.path;
        let keep = p.len().min(120);
        cur.extend_from_slice(&p[..keep]);
    }
    let mut result = DirResult {
        entries: Vec::new(),
        names: Vec::new(),
        subvol: item.subvol,
        entered: true,
    };
    let mut path = item.path.to_vec();
    let base_len = path.len();

    // One descriptor for every file in this directory: the extent ioctl only
    // needs a descriptor from the file's subvolume, and the inode number is
    // part of the search key, so this replaces open/close per file.
    let dir_file = open_dir(&item.path).ok();
    let dir_fd = dir_file
        .as_ref()
        .map(|f| std::os::fd::AsRawFd::as_raw_fd(f))
        .unwrap_or(-1);
    // Same source as the per-file value, so the comparison is meaningful
    // (stat(2) and statx(2) pack the device differently).
    let dir_dev = {
        let mut z = Vec::with_capacity(item.path.len() + 1);
        z.extend_from_slice(&item.path);
        z.push(0);
        btrfs::stat_path(&z, true).map(|s| s.dev).unwrap_or(u64::MAX)
    };

    let rd = match fs::read_dir(OsStr::from_bytes(&item.path)) {
        Ok(rd) => rd,
        Err(e) => {
            note_open_error(shared, &item.path, &e);
            store_slot(shared, item.dir_id, result);
            return;
        }
    };
    shared.prog.dirs.fetch_add(1, Ordering::Relaxed);

    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                note_open_error(shared, &item.path, &e);
                continue;
            }
        };
        let name = entry.file_name();
        let name = name.as_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };

        path.truncate(base_len);
        path.push(b'/');
        path.extend_from_slice(name);

        let mut is_dir = ft.is_dir();
        let mut is_file = ft.is_file();
        if !is_dir && !is_file {
            if ft.is_symlink() && opts.follow_symlinks {
                match fs::metadata(OsStr::from_bytes(&path)) {
                    Ok(m) => {
                        is_dir = m.is_dir();
                        is_file = m.is_file();
                    }
                    Err(_) => continue,
                }
            } else {
                continue; // like compsize: symlinks and specials are skipped
            }
        }
        if is_excluded(opts, &path) {
            continue;
        }
        let followed_symlink = !ft.is_dir() && !ft.is_file();

        if is_dir {
            let mut flags = 0u8;
            let mut subvol = item.subvol;
            let mut snap_class = item.snap_class;
            let mut dev = item.dev;
            // A different st_dev means a subvolume boundary (or a foreign
            // filesystem mounted inside our tree).
            let md = if followed_symlink {
                fs::metadata(OsStr::from_bytes(&path)).ok()
            } else {
                entry.metadata().ok()
            };
            let Some(md) = md else { continue };
            let mtime = md.mtime().clamp(0, u32::MAX as i64) as u32;
            // A mount point is a boundary even where st_dev does not change: a
            // bind mount of a plain directory carries the device of the tree it
            // was bound from. mountinfo, read before the walk, is the only
            // thing that says a filesystem is mounted here, and it also says
            // which of a subvolume's mounts is the one that reads it.
            let mounted = shared.mounts.is_point(&path);
            // The entry's inode number comes from the containing tree, so at a
            // mount point it still names the directory that is covered - which
            // is the one a node for a mount the walk does not enter describes.
            let entry_ino = entry.ino();
            if mounted || md.dev() != item.dev {
                let probe = open_dir(&path).and_then(|f| {
                    let fd = f.as_raw_fd();
                    let info = btrfs::get_subvol_info(fd)?;
                    // The root of a subvolume always carries
                    // BTRFS_FIRST_FREE_OBJECTID as its inode, while a bind
                    // mount of a plain directory keeps that directory's own -
                    // so one stat on the descriptor the probe already holds
                    // tells the two apart.
                    let ino = f.metadata().map(|m| m.ino()).unwrap_or(0);
                    Ok((info, btrfs::fs_identity(fd).ok(), ino))
                });
                match probe {
                    Ok((info, ident, ino)) => {
                        // What a mounted path must be for the walk to enter it:
                        // a subvolume root of the filesystem the scan started
                        // on. Anything else is left alone - another
                        // filesystem's extent addresses are only unique within
                        // itself, and a bind mount of a plain directory is not
                        // a subvolume at all - and the node that stands for it
                        // says a mount is there and holds none of what is
                        // under it.
                        let ours = ident == Some(shared.fs_id)
                            && ino == btrfs::BTRFS_FIRST_FREE_OBJECTID;
                        if mounted && !ours {
                            match ident {
                                Some(id) if id != shared.fs_id => {
                                    shared.otherfs.fetch_add(1, Ordering::Relaxed);
                                    shared.warn(format!(
                                        "{}: different btrfs filesystem ({}), subtree skipped",
                                        String::from_utf8_lossy(&path),
                                        btrfs::fmt_fsid(&id)
                                    ));
                                }
                                _ if ino != btrfs::BTRFS_FIRST_FREE_OBJECTID => {
                                    shared.warn(format!(
                                        "{}: mount of a plain directory, subtree skipped",
                                        String::from_utf8_lossy(&path)
                                    ));
                                }
                                _ => {}
                            }
                            flags |= FLAG_MOUNT_OTHER;
                            push_dir_not_entered(
                                shared,
                                w,
                                &mut result,
                                &item,
                                &path,
                                name,
                                mtime,
                                flags,
                                !followed_symlink,
                                dir_fd,
                                entry_ino,
                            );
                            continue;
                        }
                        // Another btrfs filesystem? Extent addresses are only
                        // unique within one filesystem, so mixing them would
                        // corrupt the dedup. Skip it; scanning it on its own
                        // works, because then it is the reference filesystem.
                        if let Some(id) = ident {
                            if shared.fs_id != ([0u8; 16], [0u8; 16]) && id != shared.fs_id {
                                shared.otherfs.fetch_add(1, Ordering::Relaxed);
                                shared.warn(format!(
                                    "{}: different btrfs filesystem ({}), subtree skipped",
                                    String::from_utf8_lossy(&path),
                                    btrfs::fmt_fsid(&id)
                                ));
                                continue;
                            }
                        }
                        // Of several mounts of one subvolume only the oldest is
                        // read: reading a second one would add a second
                        // reference to every file in it. An alias mount is like
                        // a mount of a foreign filesystem here - its node
                        // stands for the directory the containing tree holds,
                        // which is what the reader of that tree would place,
                        // and it is not descended into.
                        if mounted && shared.mounts.reads(&info.uuid) != Some(path.as_slice()) {
                            match shared.mounts.reads(&info.uuid) {
                                Some(p) => shared.warn(format!(
                                    "{}: subvolume {} is read at {}, this mount is not entered",
                                    String::from_utf8_lossy(&path),
                                    btrfs::fmt_uuid(&info.uuid),
                                    String::from_utf8_lossy(p)
                                )),
                                // no mount under the roots reads this subvolume
                                // (its filesystem could not be identified when
                                // the mounts were read)
                                None => shared.warn(format!(
                                    "{}: subvolume {} is not read anywhere under these roots, \
                                     subtree skipped",
                                    String::from_utf8_lossy(&path),
                                    btrfs::fmt_uuid(&info.uuid)
                                )),
                            }
                            flags |= FLAG_MOUNT_OTHER;
                            push_dir_not_entered(
                                shared,
                                w,
                                &mut result,
                                &item,
                                &path,
                                name,
                                mtime,
                                flags,
                                !followed_symlink,
                                dir_fd,
                                entry_ino,
                            );
                            continue;
                        }
                        // Intern before the duplicate check, so a subvolume
                        // reached through another path still gets its path
                        // compared and the smallest one recorded.
                        let (sv, f, sc) = shared.subvol_node(&info, &path);
                        subvol = sv;
                        snap_class = sc;
                        flags |= f;
                        // The same subvolume reachable through another mount
                        // would add a second reference to every file in it.
                        if !shared.visited_subvols.lock().unwrap().insert(info.uuid) {
                            if mounted {
                                // the subvolume is read at its tree location,
                                // or was met through an earlier path: this
                                // mount holds none of it here
                                flags |= FLAG_MOUNT_OTHER;
                                shared.warn(format!(
                                    "{}: subvolume {} already read elsewhere, subtree skipped",
                                    String::from_utf8_lossy(&path),
                                    btrfs::fmt_uuid(&info.uuid)
                                ));
                                push_dir_not_entered(
                                    shared,
                                    w,
                                    &mut result,
                                    &item,
                                    &path,
                                    name,
                                    mtime,
                                    flags,
                                    !followed_symlink,
                                    dir_fd,
                                    entry_ino,
                                );
                                continue;
                            }
                            shared.warn(format!(
                                "{}: subvolume {} already walked through another mount, \
                                 subtree skipped",
                                String::from_utf8_lossy(&path),
                                btrfs::fmt_uuid(&info.uuid)
                            ));
                            continue;
                        }
                        if mounted {
                            // this is the mount that reads the subvolume: the
                            // walk enters the tree here
                            flags |= FLAG_MOUNT;
                        }
                    }
                    Err(e) => {
                        let raw = e.raw_os_error().unwrap_or(0);
                        if raw == ENOTTY || raw == EINVAL || raw == EOPNOTSUPP {
                            shared.nonbtrfs.fetch_add(1, Ordering::Relaxed);
                            shared.warn(format!(
                                "{}: not btrfs, subtree skipped",
                                String::from_utf8_lossy(&path)
                            ));
                            if mounted {
                                // a mount of a filesystem that is not btrfs at
                                // all: its node says a mount is here
                                flags |= FLAG_MOUNT_OTHER;
                                push_dir_not_entered(
                                    shared,
                                    w,
                                    &mut result,
                                    &item,
                                    &path,
                                    name,
                                    mtime,
                                    flags,
                                    !followed_symlink,
                                    dir_fd,
                                    entry_ino,
                                );
                            }
                            continue;
                        }
                        shared.warn(format!(
                            "{}: subvolume info failed: {e}",
                            String::from_utf8_lossy(&path)
                        ));
                        if mounted {
                            // a mount whose filesystem could not be identified
                            // is not entered either
                            flags |= FLAG_MOUNT_OTHER;
                            push_dir_not_entered(
                                shared,
                                w,
                                &mut result,
                                &item,
                                &path,
                                name,
                                mtime,
                                flags,
                                !followed_symlink,
                                dir_fd,
                                entry_ino,
                            );
                            continue;
                        }
                    }
                }
                dev = md.dev();
            }
            // Loop protection: bind mounts and followed symlinks must not be
            // walked twice.
            {
                let key = (md.dev(), md.ino());
                let mut seen = shared.visited_dirs.lock().unwrap();
                if !seen.insert(key) {
                    shared.warn(format!(
                        "{}: already visited (bind mount or symlink loop), skipped",
                        String::from_utf8_lossy(&path)
                    ));
                    continue;
                }
            }
            if snap_class {
                flags |= FLAG_SNAP_CLASS;
            }

            if shared.prog.cancelled() {
                continue;
            }
            let dir_id = shared.next_dir_id.fetch_add(1, Ordering::Relaxed);
            shared.push_work(WorkItem {
                dir_id,
                path: path.clone().into_boxed_slice(),
                subvol,
                snap_class,
                dev,
            });
            let (btime, btime_ns) = dir_btime(w, &path, snap_class, !followed_symlink);
            let name_off = result.names.len() as u32;
            result.names.extend_from_slice(name);
            result.entries.push(Node {
                parent: NO_NODE,
                name_off,
                name_len: name.len() as u16,
                kind: KIND_DIR,
                flags,
                extra: dir_id,
                mtime,
                btime,
                btime_ns,
                ..Default::default()
            });
        } else if is_file {
            let Some(mut node) = scan_file(
                w,
                shared,
                &path,
                item.snap_class,
                dir_fd,
                dir_dev,
                !opts.follow_symlinks,
            ) else {
                continue;
            };
            let name_off = result.names.len() as u32;
            result.names.extend_from_slice(name);
            node.name_off = name_off;
            node.name_len = name.len() as u16;
            result.entries.push(node);
            shared.prog.files.fetch_add(1, Ordering::Relaxed);
        }
    }
    sort_entries(&mut result);
    store_slot(shared, item.dir_id, result);
}

/// Store a directory's entries sorted by name, rebuilding the name blob to
/// match. The tree's layout then depends on the tree alone, not on the order
/// the kernel happened to return entries in, so two scans - or two machines -
/// produce the same file.
fn sort_entries(result: &mut DirResult) {
    if result.entries.len() < 2 {
        return;
    }
    let mut pairs: Vec<(Vec<u8>, Node)> = Vec::with_capacity(result.entries.len());
    for e in result.entries.drain(..) {
        let off = e.name_off as usize;
        let len = e.name_len as usize;
        pairs.push((result.names[off..off + len].to_vec(), e));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    result.names.clear();
    for (name, mut e) in pairs {
        e.name_off = result.names.len() as u32;
        result.names.extend_from_slice(&name);
        result.entries.push(e);
    }
}

fn store_slot(shared: &Shared, dir_id: u32, result: DirResult) {
    let mut slots = shared.slots.lock().unwrap();
    if slots.len() <= dir_id as usize {
        slots.resize_with(dir_id as usize + 1, || None);
    }
    slots[dir_id as usize] = Some(result);
}

pub struct RawScan {
    inventory: Vec<btrfs::RootItem>,
    slots: Vec<Option<DirResult>>,
    refs: Vec<Vec<u64>>,
    extents: Vec<ExtentMap>,
    pub subvols: Vec<Subvol>,
    pub warnings: Vec<String>,
    pub stats: ScanStats,
}

pub fn scan(opts: &ScanOptions) -> io::Result<RawScan> {
    let started = Instant::now();
    let nthreads = if opts.threads > 0 {
        opts.threads
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    };
    // Subvolume inventory first: it decides what counts as a snapshot.
    let mut inventory: Vec<btrfs::RootItem> = Vec::new();
    let mut fs_id: ([u8; 16], [u8; 16]) = ([0u8; 16], [0u8; 16]);
    if let Some(first) = opts.roots.first() {
        let file = match open_dir(first).or_else(|_| open_file(first, true)) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!(
                    "warning: cannot open {}: {e}",
                    String::from_utf8_lossy(first)
                );
                None
            }
        };
        if let Some(file) = file {
            let fd = file.as_raw_fd();
            match btrfs::fs_identity(fd) {
                Ok(id) => {
                    fs_id = id;
                    eprintln!(
                        "filesystem: {} (from {})",
                        btrfs::fmt_fsid(&fs_id),
                        String::from_utf8_lossy(first)
                    );
                }
                Err(e) => eprintln!(
                    "warning: cannot read the filesystem identity of {}: {e} - other btrfs \
                     filesystems will not be detected",
                    String::from_utf8_lossy(first)
                ),
            }
            match btrfs::list_subvolumes(fd) {
                Ok(list) => inventory = list,
                Err(e) if e.raw_os_error() == Some(ENOTTY) => eprintln!(
                    "warning: {} is not a btrfs filesystem - extent data is unavailable there",
                    String::from_utf8_lossy(first)
                ),
                Err(e) => eprintln!("warning: cannot list subvolumes: {e}"),
            }
        }
    }

    // The mounts, before anything is walked: which mounts under the roots the
    // walk must recognize, and which of them reads each mounted subvolume.
    let mounts = discover_mounts(&opts.roots, fs_id, &opts.excludes);

    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        inventory,
        cv: Condvar::new(),
        pending: AtomicUsize::new(0),
        next_dir_id: AtomicU32::new(1), // 0 is the synthetic root
        slots: Mutex::new(vec![None]),
        subvols: Mutex::new(Vec::new()),
        mounts,
        visited_dirs: Mutex::new(HashSet::new()),
        visited_subvols: Mutex::new(HashSet::new()),
        fs_id,
        warnings: Mutex::new(Vec::new()),
        unreadable: AtomicU64::new(0),
        nonbtrfs: AtomicU64::new(0),
        otherfs: AtomicU64::new(0),
        no_btime: AtomicBool::new(false),
        fd_fallback: AtomicU64::new(0),
        prog: opts
            .live
            .clone()
            .unwrap_or_else(|| Arc::new(Progress::default())),
    });
    shared.prog.running.store(true, Ordering::SeqCst);

    // The synthetic root's entries are the paths given on the command line.
    let mut root_result = DirResult {
        entries: Vec::new(),
        names: Vec::new(),
        subvol: 0,
        entered: true,
    };
    let mut main_worker = Worker {
        searcher: Searcher::new(opts.buf_size),
        refs: Vec::new(),
        extents: ExtentMap::new(),
        block: 0,
        pathz: Vec::with_capacity(4096),
    };
    for root in &opts.roots {
        let md = match fs::metadata(OsStr::from_bytes(root)) {
            Ok(m) => m,
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("{}: {e}", String::from_utf8_lossy(root)),
                ));
            }
        };
        let name = root
            .rsplit(|b| *b == b'/')
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or(root);
        // A root on a different btrfs filesystem cannot share the extent table
        // with the first one: addresses are only unique within a filesystem.
        if fs_id != ([0u8; 16], [0u8; 16]) {
            if let Ok(f) = open_dir(root).or_else(|_| open_file(root, true)) {
                match btrfs::fs_identity(f.as_raw_fd()) {
                    Ok(id) if id != fs_id => {
                        eprintln!(
                            "{}: different btrfs filesystem ({}) than {}; scan it separately",
                            String::from_utf8_lossy(root),
                            btrfs::fmt_fsid(&id),
                            String::from_utf8_lossy(&opts.roots[0])
                        );
                        continue;
                    }
                    _ => {}
                }
            }
        }
        let mut flags = 0u8;
        if md.is_dir() {
            let mut subvol = 0u16;
            let mut snap_class = false;
            if let Ok(f) = open_dir(root) {
                if let Ok(info) = btrfs::get_subvol_info(f.as_raw_fd()) {
                    subvol = shared.intern_subvol(&info, root);
                    let table = shared.subvols.lock().unwrap();
                    let s = &table[subvol as usize];
                    snap_class = s.is_snapshot();
                    if s.is_readonly() {
                        flags |= FLAG_READONLY;
                    }
                    if snap_class {
                        flags |= FLAG_SNAPSHOT;
                    }
                }
            }
            flags |= FLAG_SUBVOL;
            if snap_class {
                flags |= FLAG_SNAP_CLASS;
            }
            let dir_id = shared.next_dir_id.fetch_add(1, Ordering::Relaxed);
            shared.visited_dirs.lock().unwrap().insert((md.dev(), md.ino()));
            if let Ok(f) = open_dir(root) {
                if let Ok(info) = btrfs::get_subvol_info(f.as_raw_fd()) {
                    shared.visited_subvols.lock().unwrap().insert(info.uuid);
                }
            }
            shared.push_work(WorkItem {
                dir_id,
                path: root.clone().into_boxed_slice(),
                subvol,
                snap_class,
                dev: md.dev(),
            });
            let (btime, btime_ns) = dir_btime(&mut main_worker, root, snap_class, false);
            let name_off = root_result.names.len() as u32;
            root_result.names.extend_from_slice(name);
            root_result.entries.push(Node {
                parent: NO_NODE,
                name_off,
                name_len: name.len() as u16,
                kind: KIND_DIR,
                flags,
                extra: dir_id,
                mtime: md.mtime().clamp(0, u32::MAX as i64) as u32,
                btime,
                btime_ns,
                ..Default::default()
            });
        } else if md.is_file() {
            let Some(mut node) =
                scan_file(&mut main_worker, &shared, root, false, -1, 0, false)
            else {
                continue;
            };
            let name_off = root_result.names.len() as u32;
            root_result.names.extend_from_slice(name);
            node.name_off = name_off;
            node.name_len = name.len() as u16;
            root_result.entries.push(node);
        } else {
            shared.warn(format!(
                "{}: not a regular file or directory, skipped",
                String::from_utf8_lossy(root)
            ));
        }
    }
    sort_entries(&mut root_result);
    shared.slots.lock().unwrap()[0] = Some(root_result);

    let show_progress = opts.progress;
    let next_block = Arc::new(AtomicU32::new(1));
    let mut collected: Vec<(u32, Vec<u64>, ExtentMap)> = Vec::new();

    std::thread::scope(|scope| {
        let progress_handle = if show_progress {
            let shared = Arc::clone(&shared);
            Some(scope.spawn(move || {
                let t0 = Instant::now();
                while shared.pending.load(Ordering::Relaxed) > 0 {
                    std::thread::sleep(Duration::from_millis(1500));
                    if shared.pending.load(Ordering::Relaxed) == 0 {
                        break;
                    }
                    eprintln!(
                        "  {:>7.0}s  {} files, {} dirs  at {}",
                        t0.elapsed().as_secs_f64(),
                        shared.prog.files.load(Ordering::Relaxed),
                        shared.prog.dirs.load(Ordering::Relaxed),
                        String::from_utf8_lossy(&shared.prog.current.lock().unwrap())
                    );
                }
            }))
        } else {
            None
        };

        let mut handles = Vec::new();
        for _ in 0..nthreads {
            let shared = Arc::clone(&shared);
            let opts = opts.clone();
            let next_block = Arc::clone(&next_block);
            handles.push(scope.spawn(move || {
                let block = next_block.fetch_add(1, Ordering::Relaxed);
                let mut w = Worker {
                    searcher: Searcher::new(opts.buf_size),
                    refs: Vec::new(),
                    extents: ExtentMap::new(),
                    block,
                    pathz: Vec::with_capacity(4096),
                };
                loop {
                    let item = {
                        let mut q = shared.queue.lock().unwrap();
                        loop {
                            if let Some(it) = q.pop_front() {
                                break it;
                            }
                            if shared.pending.load(Ordering::SeqCst) == 0 {
                                return (block, w.refs, w.extents);
                            }
                            q = shared.cv.wait(q).unwrap();
                        }
                    };
                    process_dir(&mut w, &shared, &opts, item);
                    // Decrement under the lock so a waiter cannot miss the
                    // final wakeup.
                    let q = shared.queue.lock().unwrap();
                    if shared.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
                        shared.cv.notify_all();
                    }
                    drop(q);
                }
            }));
        }
        for h in handles {
            collected.push(h.join().unwrap());
        }
        if let Some(h) = progress_handle {
            let _ = h.join();
        }
    });

    collected.push((0, main_worker.refs, main_worker.extents));
    let nblocks = collected.iter().map(|(b, _, _)| *b as usize + 1).max().unwrap_or(1);
    let mut refs: Vec<Vec<u64>> = vec![Vec::new(); nblocks];
    let mut extents: Vec<ExtentMap> = Vec::with_capacity(nblocks);
    for (block, r, m) in collected {
        refs[block as usize] = r;
        extents.push(m);
    }

    let inventory_items = shared.inventory.clone();
    let slots = std::mem::take(&mut *shared.slots.lock().unwrap());
    let subvols = std::mem::take(&mut *shared.subvols.lock().unwrap());
    let mut warnings = std::mem::take(&mut *shared.warnings.lock().unwrap());

    // Sharing partners we did not walk: their references are invisible to us,
    // so extents shared with them look exclusive to what we did scan.
    {
        let seen_ids: HashSet<u64> = subvols.iter().map(|s| s.id).collect();
        for s in &subvols {
            if s.uuid == [0u8; 16] {
                continue;
            }
            let missing = shared.inventory
                .iter()
                .filter(|r| r.parent_uuid == s.uuid && !seen_ids.contains(&r.treeid))
                .count();
            if missing > 0 {
                warnings.push(format!(
                    "subvolume {} (id {}) has {} snapshot(s) outside the scanned paths; \
                     shared extents will look exclusive to what was scanned",
                    String::from_utf8_lossy(&s.name),
                    s.id,
                    missing
                ));
            }
        }
    }

    let stats = ScanStats {
        files: shared.prog.files.load(Ordering::Relaxed),
        dirs: shared.prog.dirs.load(Ordering::Relaxed),
        extents: 0,
        refs: refs.iter().map(|r| r.len() as u64).sum(),
        subvolumes: subvols.len() as u64,
        skipped_nonbtrfs: shared.nonbtrfs.load(Ordering::Relaxed),
        skipped_otherfs: shared.otherfs.load(Ordering::Relaxed),
        unreadable: shared.unreadable.load(Ordering::Relaxed),
        fd_fallback: shared.fd_fallback.load(Ordering::Relaxed),
        elapsed: started.elapsed(),
    };
    shared.prog.running.store(false, Ordering::SeqCst);
    Ok(RawScan {
        inventory: inventory_items,
        slots,
        refs,
        extents,
        subvols,
        warnings,
        stats,
    })
}

/// Assign final node indices: parents before children, each directory's
/// children contiguous, and fill in every reference's owner node.
pub fn flatten(
    mut raw: RawScan,
    inventory_items: Vec<btrfs::RootItem>,
) -> (Tree, Vec<Vec<u64>>, Vec<ExtentMap>, ScanStats) {
    // The subvolume table goes into the cache as it stands, so its order has
    // to be a property of the tree, not of which worker happened to meet a
    // subvolume first (or of how the threads interleaved). Sorting by uuid
    // gives every run of the same tree the same table; the directories'
    // indices are renumbered to match.
    let mut order: Vec<u16> = (0..raw.subvols.len() as u16).collect();
    order.sort_by(|&a, &b| {
        let (x, y) = (&raw.subvols[a as usize], &raw.subvols[b as usize]);
        x.uuid.cmp(&y.uuid).then(x.id.cmp(&y.id))
    });
    let mut renumbered = vec![0u16; order.len()];
    for (new, &old) in order.iter().enumerate() {
        renumbered[old as usize] = new as u16;
    }
    let subvols: Vec<Subvol> = order
        .iter()
        .map(|&old| raw.subvols[old as usize].clone())
        .collect();

    let mut tree = Tree {
        nodes: Vec::new(),
        names: Vec::new(),
        dirs: Vec::new(),
        roots: Vec::new(),
        root: 0,
        subvols,
        inventory: Vec::new(),
        warnings: raw.warnings,
        stats: ScanStats::default(),
        scanned_at: 0,
        scanned_roots: Vec::new(),
    };
    tree.nodes.push(Node {
        parent: NO_NODE,
        kind: KIND_DIR,
        flags: FLAG_ROOT,
        ..Default::default()
    });

    let mut queue: VecDeque<(u32, u32)> = VecDeque::new(); // (temp dir id, node id)
    queue.push_back((0, 0));

    while let Some((dir_id, node_id)) = queue.pop_front() {
        let mut slot = match raw.slots[dir_id as usize].take() {
            Some(s) => s,
            None => continue,
        };
        let depth = if node_id == 0 {
            0
        } else {
            let parent = tree.nodes[node_id as usize].parent;
            tree.dirs[tree.nodes[parent as usize].extra as usize].depth + 1
        };
        let extra = tree.dirs.len() as u32;
        tree.dirs.push(DirExtra {
            // 0 doubles as the "no subvolume" sentinel, so an index the table
            // does not cover is left alone rather than remapped to a stranger
            subvol: renumbered
                .get(slot.subvol as usize)
                .copied()
                .unwrap_or(slot.subvol),
            depth,
            ..Default::default()
        });
        tree.nodes[node_id as usize].extra = extra;
        // A directory the walk did not enter holds nothing and is not
        // descended; its first_child stays zero, as the tree reader leaves it.
        if !slot.entered {
            continue;
        }
        tree.nodes[node_id as usize].first_child = tree.nodes.len() as u32;

        let first_child = tree.nodes.len() as u32;
        let name_base = tree.names.len() as u32;
        tree.names.extend_from_slice(&slot.names);
        let mut n = 0u32;
        for mut e in slot.entries.drain(..) {
            let idx = tree.nodes.len() as u32;
            e.parent = node_id;
            e.name_off += name_base;
            if e.kind == KIND_DIR {
                let child_dir_id = e.extra;
                queue.push_back((child_dir_id, idx));
            } else {
                let block = e.extra as usize;
                let start = e.first_ref as usize;
                let end = start + e.nrefs as usize;
                for r in &mut raw.refs[block][start..end] {
                    *r |= idx as u64;
                }
            }
            tree.nodes.push(e);
            n += 1;
        }
        tree.nodes[node_id as usize].nchildren = n;
        if node_id == 0 {
            tree.roots = (first_child..first_child + n).collect();
        }
    }

    tree.inventory = inventory_items;
    let mut stats = raw.stats.clone();
    stats.extents = raw
        .extents
        .iter()
        .map(|m| m.len() as u64)
        .sum::<u64>()
        .min(u64::MAX);
    (tree, raw.refs, raw.extents, stats)
}

/// Merge the per-thread extent maps into one table. Only the extent's size
/// needs merging: the ownership window is not a walk-time value any more, it is
/// folded out of the tree's own references afterwards.
pub fn merge_extents(maps: Vec<ExtentMap>) -> ExtentMap {
    let mut maps = maps;
    maps.sort_by_key(|m| std::cmp::Reverse(m.len()));
    let mut out = maps.pop().unwrap_or_default();
    for m in maps {
        for (k, v) in m {
            match out.get_mut(&k) {
                Some(_) => {
                    // disk/ram keep the first-seen value, as compsize does
                }
                None => {
                    out.insert(k, v);
                }
            }
        }
    }
    out
}

/// One call: walk, index, and attribute. Returns the tree plus scan stats.
pub fn scan_tree(opts: &ScanOptions) -> io::Result<(Tree, ScanStats)> {
    let raw = scan(opts)?;
    let inventory = raw.inventory.clone();
    let (mut tree, mut ref_blocks, maps, mut stats) = flatten(raw, inventory);
    let mut extents = merge_extents(maps);
    stats.extents = extents.len() as u64;
    let subvol_otime: Vec<u64> = tree.subvols.iter().map(|s| s.otime).collect();
    tree.stats = stats.clone();
    tree.scanned_at = crate::cache::now();
    tree.scanned_roots = opts.roots.clone();
    tree.compute_windows(&ref_blocks, &mut extents, &subvol_otime);
    tree.attribute_owned(&ref_blocks, &mut extents, &subvol_otime);
    tree.attribute_reach(&mut ref_blocks, &extents);
    tree.rollup();
    tree.clear_ref_bookkeeping();
    drop(ref_blocks);
    Ok((tree, stats))
}
