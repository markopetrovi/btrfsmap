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
use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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
    /// walk snapshot-class subvolumes' trees instead of taking their
    /// references from the extent tree (their interiors stay browsable)
    pub full_scan: bool,
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
            full_scan: false,
            live: None,
        }
    }
}

pub struct DirResult {
    pub entries: Vec<Node>,
    pub names: Vec<u8>,
    pub subvol: u16,
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

struct Shared {
    queue: Mutex<VecDeque<WorkItem>>,
    cv: Condvar,
    pending: AtomicUsize,
    next_dir_id: AtomicU32,
    slots: Mutex<Vec<Option<DirResult>>>,
    subvols: Mutex<Vec<Subvol>>,
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
            // keep the lexicographically smallest path, so the recorded one does
            // not depend on which mount the walk happened to reach first
            if table[i].path.is_empty() || path < table[i].path.as_slice() {
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
    opts.excludes.iter().any(|ex| path.starts_with(ex))
}

/// Read one regular file's extent list. Returns its node with `owned` left at
/// zero (that needs global extent counts, so it happens after the walk).
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
    subvol_otime: u64,
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
    let btime = st.btime.min(u32::MAX as u64) as u32;
    let btime_ns = (st.btime_ns >> 14) as u16;

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
    let mut inline_disk = 0u32;
    let mut inline_ram = 0u32;
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
            // Inline payloads live in metadata, are never shared, and compsize
            // counts them per file: add them straight to this file.
            inline_disk = inline_disk.saturating_add(item.disk_num_bytes as u32);
            inline_ram = inline_ram.saturating_add(item.ram_bytes as u32);
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
        let e = extents.entry(page).or_insert_with(|| ExtentInfo {
            disk: item.disk_num_bytes,
            ram: item.ram_bytes,
            win_btime: u32::MAX,
            win_btime_ns: u16::MAX,
            win_otime: u64::MAX,
            win_count: 0,
            assigned: 0,
        });
        // keep the smallest (birth time, subvolume age) seen for this extent
        let key = (btime, btime_ns, subvol_otime);
        let best = (e.win_btime, e.win_btime_ns, e.win_otime);
        if key < best {
            e.win_btime = btime;
            e.win_btime_ns = btime_ns;
            e.win_otime = subvol_otime;
            e.win_count = 1;
        } else if key == best {
            e.win_count += 1;
        }
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
        inline_disk: inline_disk.min(u16::MAX as u32) as u16,
        inline_ram: inline_ram.min(u16::MAX as u32) as u16,
        kind: KIND_FILE,
        flags,
        ..Default::default()
    })
}

fn process_dir(w: &mut Worker, shared: &Shared, opts: &ScanOptions, item: WorkItem) {
    if shared.prog.cancelled() {
        store_slot(shared, item.dir_id, DirResult {
            entries: Vec::new(),
            names: Vec::new(),
            subvol: item.subvol,
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
    let subvol_otime = {
        let table = shared.subvols.lock().unwrap();
        table.get(item.subvol as usize).map(|s| s.otime).unwrap_or(0)
    };
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
            if md.dev() != item.dev {
                let probe = open_dir(&path).and_then(|f| {
                    let fd = f.as_raw_fd();
                    let info = btrfs::get_subvol_info(fd)?;
                    Ok((info, btrfs::fs_identity(fd).ok()))
                });
                match probe {
                    Ok((info, ident)) => {
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
                        // The same subvolume reachable through another mount
                        // would add a second reference to every file in it.
                        if !shared.visited_subvols.lock().unwrap().insert(info.uuid) {
                            shared.warn(format!(
                                "{}: subvolume {} already walked through another mount, \
                                 subtree skipped",
                                String::from_utf8_lossy(&path),
                                btrfs::fmt_uuid(&info.uuid)
                            ));
                            continue;
                        }
                        subvol = shared.intern_subvol(&info, &path);
                        let table = shared.subvols.lock().unwrap();
                        let s = &table[subvol as usize];
                        snap_class = s.is_snapshot();
                        if s.is_readonly() {
                            flags |= FLAG_READONLY;
                        }
                        if snap_class {
                            flags |= FLAG_SNAPSHOT;
                        }
                        flags |= FLAG_SUBVOL;
                    }
                    Err(e) => {
                        let raw = e.raw_os_error().unwrap_or(0);
                        if raw == ENOTTY || raw == EINVAL || raw == EOPNOTSUPP {
                            shared.nonbtrfs.fetch_add(1, Ordering::Relaxed);
                            shared.warn(format!(
                                "{}: not btrfs, subtree skipped",
                                String::from_utf8_lossy(&path)
                            ));
                            continue;
                        }
                        shared.warn(format!(
                            "{}: subvolume info failed: {e}",
                            String::from_utf8_lossy(&path)
                        ));
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
            let name_off = result.names.len() as u32;
            result.names.extend_from_slice(name);
            // birth time, which only statx reports; a directory's is not used by
            // the ownership rule, but leaving it zero would make two otherwise
            // identical scans differ
            let (btime, btime_ns) = {
                let mut z = path.clone();
                z.push(0);
                match btrfs::stat_path(&z, !opts.follow_symlinks) {
                    Ok(st) => (
                        st.btime.min(u32::MAX as u64) as u32,
                        (st.btime_ns >> 14) as u16,
                    ),
                    Err(_) => (0, 0),
                }
            };
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
                subvol_otime,
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
    store_slot(shared, item.dir_id, result);
}

/// Sort a directory's entries by name. The file walker sees readdir order and
/// a metadata scan sees inode order, so without this the two would lay the tree
/// out differently - and so would two runs on kernels that iterate differently.
/// Done here, in the worker that built the entries, so it is parallel.
fn sort_entries(result: &mut DirResult) {
    let DirResult { entries, names, .. } = result;
    entries.sort_by(|a, b| {
        let an = &names[a.name_off as usize..a.name_off as usize + a.name_len as usize];
        let bn = &names[b.name_off as usize..b.name_off as usize + b.name_len as usize];
        an.cmp(bn)
    });
    // Rebuild the blob so the names follow the sorted entries: a metadata scan
    // has no readdir order to reproduce, so node order is the convention both
    // have to agree on.
    let old = std::mem::take(names);
    for e in entries.iter_mut() {
        let a = e.name_off as usize;
        let b = a + e.name_len as usize;
        e.name_off = names.len() as u32;
        names.extend_from_slice(&old[a..b]);
    }
}

fn store_slot(shared: &Shared, dir_id: u32, mut result: DirResult) {
    sort_entries(&mut result);
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

    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        inventory,
        cv: Condvar::new(),
        pending: AtomicUsize::new(0),
        next_dir_id: AtomicU32::new(1), // 0 is the synthetic root
        slots: Mutex::new(vec![None]),
        subvols: Mutex::new(Vec::new()),
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
            let name_off = root_result.names.len() as u32;
            root_result.names.extend_from_slice(name);
            // the root's own birth time, which statx is the only way to get
            let mut btime = 0u32;
            let mut btime_ns = 0u16;
            {
                let mut z = root.clone();
                z.push(0);
                if let Ok(st) = btrfs::stat_path(&z, true) {
                    btime = st.btime.min(u32::MAX as u64) as u32;
                    btime_ns = (st.btime_ns >> 14) as u16;
                }
            }
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
                scan_file(&mut main_worker, &shared, root, false, -1, 0, 0, false)
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
    let mut tree = Tree {
        nodes: Vec::new(),
        names: Vec::new(),
        dirs: Vec::new(),
        roots: Vec::new(),
        root: 0,
        subvols: raw.subvols,
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
            subvol: slot.subvol,
            depth,
            ..Default::default()
        });
        tree.nodes[node_id as usize].extra = extra;
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

    // Deterministic subvolume numbering: sort by UUID and remap every index the
    // tree already stored.
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

/// Merge the per-thread extent maps into one table.
pub fn merge_extents(maps: Vec<ExtentMap>) -> ExtentMap {
    let mut maps = maps;
    maps.sort_by_key(|m| std::cmp::Reverse(m.len()));
    let mut out = maps.pop().unwrap_or_default();
    for m in maps {
        for (k, v) in m {
            match out.get_mut(&k) {
                Some(e) => {
                    let mine = (e.win_btime, e.win_btime_ns, e.win_otime);
                    let theirs = (v.win_btime, v.win_btime_ns, v.win_otime);
                    if theirs < mine {
                        e.win_btime = v.win_btime;
                        e.win_btime_ns = v.win_btime_ns;
                        e.win_otime = v.win_otime;
                        e.win_count = v.win_count;
                    } else if theirs == mine {
                        e.win_count += v.win_count;
                    }
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
    tree.attribute_owned(&ref_blocks, &mut extents, &subvol_otime);
    tree.attribute_reach(&mut ref_blocks, &extents);
    tree.rollup();
    drop(ref_blocks);
    Ok((tree, stats))
}
