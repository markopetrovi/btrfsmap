//! M1: a single-walk accounting pass that reproduces `compsize`'s output
//! byte for byte, so the extent layer can be verified against it.
//!
//! This is deliberately serial and mirrors compsize.c's `do_recursive_search`
//! (same open flags, same errno policy, same dedup key, same counters). The
//! parallel walker and the per-directory model build on the same primitives.

use crate::btrfs::{self, ExtentItem, Searcher, BTRFS_FILE_EXTENT_INLINE, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK};
use crate::model::*;
use crate::walk::ScanStats;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

pub const MAX_ENTRIES: usize = 257;
pub const FRAGEND_NONE: u64 = u64::MAX;

const ENOENT: i32 = 2;
const ENXIO: i32 = 6;
const EACCES: i32 = 13;
const ENODEV: i32 = 19;
const ELOOP: i32 = 40;
const ENOMEDIUM: i32 = 123;

#[derive(Clone, Debug)]
pub struct Stats {
    pub disk: [u64; MAX_ENTRIES],
    pub uncomp: [u64; MAX_ENTRIES],
    pub refd: [u64; MAX_ENTRIES],
    pub nfiles: u64,
    pub nextents: u64,
    pub nrefs: u64,
    pub ninline: u64,
    pub nfrag: u64,
    /// Dedup key is the extent's start page, exactly like compsize's radix tree.
    seen: HashSet<u64>,
    fragend: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            disk: [0; MAX_ENTRIES],
            uncomp: [0; MAX_ENTRIES],
            refd: [0; MAX_ENTRIES],
            nfiles: 0,
            nextents: 0,
            nrefs: 0,
            ninline: 0,
            nfrag: 0,
            seen: HashSet::new(),
            fragend: FRAGEND_NONE,
        }
    }
}

impl Stats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn disk_all(&self) -> u64 {
        self.disk.iter().sum()
    }
    pub fn uncomp_all(&self) -> u64 {
        self.uncomp.iter().sum()
    }
    pub fn refd_all(&self) -> u64 {
        self.refd.iter().sum()
    }

    fn account(&mut self, item: &ExtentItem) {
        let t = item.comp_type as usize;
        if item.kind == BTRFS_FILE_EXTENT_INLINE {
            self.disk[t] += item.disk_num_bytes;
            self.uncomp[t] += item.ram_bytes;
            self.refd[t] += item.ram_bytes;
            self.ninline += 1;
            self.nfrag += 1;
            self.fragend = FRAGEND_NONE;
            return;
        }
        if item.disk_bytenr == 0 {
            return; // hole: no disk, no reference
        }
        if self.seen.insert(item.disk_bytenr >> 12) {
            self.disk[t] += item.disk_num_bytes;
            self.uncomp[t] += item.ram_bytes;
            self.nextents += 1;
        }
        self.refd[t] += item.num_bytes;
        self.nrefs += 1;
        if item.disk_bytenr != self.fragend {
            self.nfrag += 1;
        }
        self.fragend = item.disk_bytenr + item.disk_num_bytes;
    }
}

pub struct Walker {
    searcher: Searcher,
    stats: Stats,
    path: Vec<u8>,
    warnings: Vec<String>,
}

impl Walker {
    pub fn new(buf_size: usize) -> Self {
        Walker {
            searcher: Searcher::new(buf_size),
            stats: Stats::new(),
            path: Vec::with_capacity(4096),
            warnings: Vec::new(),
        }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Walk an absolute or relative path, following the path itself (a symlink
    /// given on the command line is resolved) but not symlinks found inside.
    pub fn walk_root(&mut self, root: &[u8]) -> io::Result<()> {
        self.path.clear();
        self.path.extend_from_slice(root);
        let md = fs::metadata(std::ffi::OsStr::from_bytes(root))?;
        if md.is_dir() {
            self.walk_dir()
        } else if md.is_file() {
            match Self::open_file(&self.path) {
                Ok(f) => self.do_file(f)?,
                Err(e) => self.report_open_error(&e),
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    fn open_file(path: &[u8]) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NOCTTY | O_NONBLOCK)
            .open(std::ffi::OsStr::from_bytes(path))
    }

    fn report_open_error(&mut self, e: &io::Error) {
        match e.raw_os_error().unwrap_or(0) {
            // compsize's silent-skip list: symlinks, devices, deleted files.
            ELOOP | ENXIO | ENODEV | ENOMEDIUM | ENOENT => {}
            EACCES => {
                if self.warnings.len() < 100 {
                    self.warnings
                        .push(format!("{}: {e}", String::from_utf8_lossy(&self.path)));
                }
            }
            _ => {
                if self.warnings.len() < 100 {
                    self.warnings.push(format!(
                        "{}: {e} (skipped)",
                        String::from_utf8_lossy(&self.path)
                    ));
                }
            }
        }
    }

    fn walk_dir(&mut self) -> io::Result<()> {
        let dir_path = std::ffi::OsStr::from_bytes(&self.path).to_owned();
        let entries = match fs::read_dir(&dir_path) {
            Ok(e) => e,
            Err(e) => {
                self.report_open_error(&e);
                return Ok(());
            }
        };
        let base_len = self.path.len();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    self.report_open_error(&e);
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
            let is_dir = ft.is_dir();
            let is_file = ft.is_file();
            let unknown = !is_dir && !is_file && !ft.is_symlink();
            if !is_dir && !is_file && !unknown {
                continue; // compsize only descends into DT_DIR/DT_REG/DT_UNKNOWN
            }
            self.path.truncate(base_len);
            self.path.push(b'/');
            self.path.extend_from_slice(name);
            if is_dir || (unknown && fs::metadata(std::ffi::OsStr::from_bytes(&self.path)).map(|m| m.is_dir()).unwrap_or(false)) {
                self.walk_dir()?;
            } else {
                match Self::open_file(&self.path) {
                    Ok(f) => self.do_file(f)?,
                    Err(e) => self.report_open_error(&e),
                }
            }
        }
        self.path.truncate(base_len);
        Ok(())
    }

    fn do_file(&mut self, file: File) -> io::Result<()> {
        let md = file.metadata()?;
        if !md.is_file() {
            return Ok(());
        }
        self.stats.nfiles += 1;
        self.stats.fragend = FRAGEND_NONE;
        let fd = file.as_raw_fd();
        let ino = md.ino();
        let Walker { searcher, stats, .. } = self;
        let mut account = |item: &ExtentItem| stats.account(item);
        match searcher.for_each_extent(fd, ino, &mut account) {
            Ok(_) => Ok(()),
            Err(e) => {
                if self.warnings.len() < 100 {
                    self.warnings.push(format!(
                        "{}: {e} (extents not counted)",
                        String::from_utf8_lossy(&self.path)
                    ));
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// compsize-compatible rendering
// ---------------------------------------------------------------------------

pub fn human_bytes(x: u64, bytes_mode: bool) -> String {
    if bytes_mode {
        return format!("{x}");
    }
    const UNITS: &[u8] = b"BKMGTPE";
    let mut u = 0usize;
    let mut v = x;
    while v >= 10240 {
        u += 1;
        v >>= 10;
    }
    if v >= 1024 {
        format!(" {}.{}{}", v >> 10, v * 10 / 1024 % 10, UNITS[u + 1] as char)
    } else {
        format!("{:>4}{}", v, UNITS[u] as char)
    }
}

fn type_name(t: usize) -> String {
    match t {
        0 => "none".into(),
        1 => "zlib".into(),
        2 => "lzo".into(),
        3 => "zstd".into(),
        256 => "prealloc".into(),
        n => format!("?{n}"),
    }
}

fn table_row(out: &mut String, ty: &str, perc: &str, disk: &str, uncomp: &str, refd: &str) {
    out.push_str(&format!(
        "{ty:<10} {perc:<8} {disk:<12} {uncomp:<12} {refd:<12}\n"
    ));
}

/// Render compsize's stats table. Returns None when compsize would print
/// "All empty or still-delalloced files." instead.
pub fn render_stats(st: &Stats, bytes_mode: bool) -> Option<String> {
    let (disk_all, uncomp_all, refd_all) = (st.disk_all(), st.uncomp_all(), st.refd_all());
    if uncomp_all == 0 {
        return None;
    }
    let mut out = String::new();
    out.push_str(&format!(
        "Processed {} file{}, {} regular extents ({} refs), {} inline, {} fragments.\n",
        st.nfiles,
        if st.nfiles > 1 { "s" } else { "" },
        st.nextents,
        st.nrefs,
        st.ninline,
        st.nfrag
    ));
    table_row(
        &mut out,
        "Type",
        "Perc",
        "Disk Usage",
        "Uncompressed",
        "Referenced",
    );
    table_row(
        &mut out,
        "TOTAL",
        &format!("{:>3}%", disk_all * 100 / uncomp_all),
        &human_bytes(disk_all, bytes_mode),
        &human_bytes(uncomp_all, bytes_mode),
        &human_bytes(refd_all, bytes_mode),
    );
    for t in 0..MAX_ENTRIES {
        if st.uncomp[t] == 0 {
            continue;
        }
        table_row(
            &mut out,
            &type_name(t),
            &format!("{:>3}%", st.disk[t] * 100 / st.uncomp[t]),
            &human_bytes(st.disk[t], bytes_mode),
            &human_bytes(st.uncomp[t], bytes_mode),
            &human_bytes(st.refd[t], bytes_mode),
        );
    }
    Some(out)
}

pub fn print_stats(st: &Stats, bytes_mode: bool) -> io::Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match render_stats(st, bytes_mode) {
        Some(text) => lock.write_all(text.as_bytes()),
        None => {
            if st.nfiles == 0 {
                eprintln!("No files.");
            } else {
                eprintln!("All empty or still-delalloced files.");
            }
            Ok(())
        }
    }
}

/// Validate our hand-written struct layouts against the kernel headers at
/// compile time: the ioctl numbers are baked in from these sizes.
const _: () = {
    assert!(btrfs::FILE_EXTENT_ITEM_SIZE == 53);
};

// ---------------------------------------------------------------------------
// Tree report: the one-pass, per-directory view (what `scan` prints)
// ---------------------------------------------------------------------------

pub struct TreeReportOptions {
    pub depth: usize,
    pub top: usize,
    pub raw: bool,
    pub files: usize,
}

/// Biggest individual files by owned bytes (bounded heap, no full sort).
pub fn top_files(tree: &Tree, want: usize) -> Vec<u32> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    if want == 0 {
        return Vec::new();
    }
    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::with_capacity(want + 1);
    for i in 0..tree.nodes.len() as u32 {
        if tree.nodes[i as usize].kind != KIND_FILE {
            continue;
        }
        let v = tree.nodes[i as usize].owned;
        if heap.len() < want {
            heap.push(Reverse((v, i)));
        } else if let Some(Reverse((min, _))) = heap.peek() {
            if v > *min {
                heap.pop();
                heap.push(Reverse((v, i)));
            }
        }
    }
    let mut out: Vec<(u64, u32)> = heap.into_iter().map(|Reverse(x)| x).collect();
    out.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
    out.into_iter().map(|(_, i)| i).collect()
}

fn dir_tags(tree: &Tree, id: u32) -> String {
    let n = tree.node(id);
    let d = tree.dir(id);
    let mut tags: Vec<String> = Vec::new();
    if n.flags & FLAG_SNAPSHOT != 0 {
        tags.push("snapshot".into());
    } else if n.flags & FLAG_SUBVOL != 0 {
        tags.push("subvolume".into());
    }
    if n.flags & FLAG_READONLY != 0 {
        tags.push("ro".into());
    }
    if d.freeable < n.owned {
        tags.push(format!("free {}", human(d.freeable)));
    }
    if tags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", tags.join(", "))
    }
}

fn render_dir(
    tree: &Tree,
    id: u32,
    indent: usize,
    opt: &TreeReportOptions,
    parent_owned: u64,
    out: &mut String,
) {
    let n = tree.node(id);
    let name = if id == tree.root {
        "/".to_string()
    } else {
        String::from_utf8_lossy(tree.name(id)).into_owned()
    };
    let pct = if parent_owned > 0 {
        n.owned as f64 * 100.0 / parent_owned as f64
    } else {
        0.0
    };
    out.push_str(&format!(
        "{:indent$}{:>10}  {:>5.1}%  {}{}\n",
        "",
        size(n.owned, opt.raw),
        pct,
        name,
        dir_tags(tree, id),
        indent = indent
    ));
    if (indent / 2) + 1 > opt.depth {
        return;
    }
    let mut kids: Vec<u32> = tree
        .child_ids(id)
        .filter(|&c| tree.is_dir(c) && c != id)
        .collect();
    kids.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).owned));
    for c in kids.iter().take(opt.top) {
        render_dir(tree, *c, indent + 2, opt, n.owned, out);
    }
    if kids.len() > opt.top {
        out.push_str(&format!(
            "{:indent$}... {} more directories\n",
            "",
            kids.len() - opt.top,
            indent = indent + 2
        ));
    }
}

/// Count snapshot subvolumes that are not nested inside another one.
fn top_level_snapshots(tree: &Tree) -> Vec<u32> {
    let mut out = Vec::new();
    for i in 0..tree.nodes.len() as u32 {
        let n = &tree.nodes[i as usize];
        if n.kind != KIND_DIR || n.flags & FLAG_SNAPSHOT == 0 {
            continue;
        }
        let mut cur = n.parent;
        let mut nested = false;
        while cur != NO_NODE && cur != tree.root {
            if tree.nodes[cur as usize].flags & FLAG_SNAPSHOT != 0 {
                nested = true;
                break;
            }
            cur = tree.nodes[cur as usize].parent;
        }
        if !nested {
            out.push(i);
        }
    }
    out
}

pub fn render_tree_report(tree: &Tree, stats: &ScanStats, opt: &TreeReportOptions) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "scanned {} files, {} dirs, {} extents, {} refs, {} subvolumes in {:.1}s\n",
        stats.files, stats.dirs, stats.extents, stats.refs, stats.subvolumes,
        stats.elapsed.as_secs_f64()
    ));
    if stats.skipped_nonbtrfs > 0 {
        out.push_str(&format!(
            "  ({} non-btrfs mounts skipped, {} unreadable)\n",
            stats.skipped_nonbtrfs, stats.unreadable
        ));
    }

    let snaps = top_level_snapshots(tree);
    if !snaps.is_empty() {
        let owned: u64 = snaps.iter().map(|&s| tree.node(s).owned).sum();
        let free: u64 = snaps.iter().map(|&s| tree.dir(s).freeable).sum();
        out.push_str(&format!(
            "\nsnapshots: {} subvolumes, {} owned, {} freeable (deleting them all would free this)\n",
            snaps.len(),
            human(owned),
            human(free)
        ));
        let mut sorted = snaps.clone();
        sorted.sort_by_key(|&s| std::cmp::Reverse(tree.node(s).owned));
        for s in sorted.iter().take(opt.top.min(8)) {
            let path = tree.path_of(*s);
            out.push_str(&format!(
                "    {:>10}  {:>10} freeable  {}\n",
                human(tree.node(*s).owned),
                human(tree.dir(*s).freeable),
                String::from_utf8_lossy(&path)
            ));
        }
        if sorted.len() > opt.top.min(8) {
            out.push_str(&format!("    ... {} more\n", sorted.len() - opt.top.min(8)));
        }
    }

    for &root in &tree.roots {
        let n = tree.node(root);
        let d = tree.dir(root);
        out.push_str(&format!(
            "\n{}  [subvolume {}]\n",
            String::from_utf8_lossy(&tree.path_of(root)),
            d.subvol
        ));
        out.push_str(&format!(
            "  owned        {:>10}   semantic share of physical extents (adds up)\n",
            size(n.owned, opt.raw)
        ));
        out.push_str(&format!(
            "  freeable     {:>10}   deleting this path would free\n",
            size(d.freeable, opt.raw)
        ));
        out.push_str(&format!(
            "  compsize     {:>10}   what `compsize <path>` prints\n",
            size(d.compsize, opt.raw)
        ));
        out.push_str(&format!(
            "  uncompressed {:>10}   deduped, before compression\n",
            size(d.uncomp_reach, opt.raw)
        ));
        out.push_str(&format!(
            "  referenced   {:>10}   what du/baobab count\n",
            size(n.referenced, opt.raw)
        ));
        out.push_str(&format!(
            "  {} files, {} dirs\n",
            d.nfiles,
            d.ndirs
        ));
        out.push_str("\n  largest directories by owned:\n");
        let mut kids: Vec<u32> = tree.child_ids(root).filter(|&c| tree.is_dir(c)).collect();
        kids.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).owned));
        for c in kids.iter().take(opt.top) {
            render_dir(tree, *c, 4, opt, n.owned, &mut out);
        }
    }
    if opt.files > 0 {
        out.push_str("\nlargest files by owned:\n");
        for id in top_files(tree, opt.files) {
            let n = tree.node(id);
            out.push_str(&format!(
                "  {:>12}  ref {:>10}  {}\n",
                size(n.owned, opt.raw),
                size(n.referenced, opt.raw),
                String::from_utf8_lossy(&tree.path_of(id))
            ));
        }
    }
    out
}
