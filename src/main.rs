//! compsize-tree: extent-accurate btrfs disk usage.
//!
//! GUI (no arguments): open a saved scan or scan a path.
//! CLI: `scan` for a headless pass, `report` for compsize-compatible output.

mod btrfs;
mod cache;
mod chart;
mod gui;
mod listmodel;
mod metadata;
mod model;
mod report;
mod walk;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

extern "C" {
    fn geteuid() -> u32;
    fn chown(path: *const i8, uid: u32, gid: u32) -> i32;
}

const USAGE: &str = "\
usage: compsize-tree [command] [options] [paths...]

With no command, the graphical browser starts: open a saved scan or pick a path to scan.

commands:
  scan [options] PATH...   one pass, per-directory metrics on stdout
  report [-b] PATH...      compsize-compatible table (the verification mode)
  view FILE.cst            open a saved scan in the graphical browser
  check FILE.cst           load a saved scan and print what it holds (no GUI)

scan options:
  -j, --threads N          worker threads (default: number of CPUs)
  -L, --follow-symlinks    follow symlinks found while walking
      --exclude PATH       skip this path prefix (repeatable; makes shared
                           extents below it look exclusive to what remains)
      --buffer BYTES       kernel search buffer per thread (default 1048576)
      --depth N            directory levels to print (default 2)
      --top N              entries per level (default 12)
      --files N            biggest files to list (default 0)
  -b, --bytes              raw byte counts in the report
      --full-scan          walk snapshot-class subvolumes too, so their
                           interiors stay browsable (default: a snapshot is a
                           single node holding its subtree's aggregate)
      --dump-subvols       print the subvolume inventory and its classification
      --save FILE          also write the scan to FILE (the GUI can open it)
      --reproducible       omit timings and warnings from the saved scan, so two
                           scans of the same tree produce byte-identical files
      --compress           compress the saved scan with zstd
      --progress           print progress while scanning
  -h, --help               this help
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        None => gui::run(None),
        Some("-h") | Some("--help") | Some("help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("scan") => cmd_scan(&args[1..]),
        Some("report") => cmd_report(&args[1..]),
        Some("extent-probe") => match args.get(1) {
            Some(p) => cmd_extent_probe(&PathBuf::from(p)),
            None => {
                eprintln!("extent-probe needs a path inside the filesystem");
                ExitCode::from(2)
            }
        },
        Some("diff") => cmd_diff(&args[1..]),
        Some("check") => match args.get(1) {
            Some(f) => cmd_check(&PathBuf::from(f)),
            None => {
                eprintln!("check needs a file: compsize-tree check FILE.cst");
                ExitCode::from(2)
            }
        },
        Some("view") => match args.get(1) {
            Some(f) => gui::run(Some(PathBuf::from(f))),
            None => {
                eprintln!("view needs a file: compsize-tree view FILE.cst");
                ExitCode::from(2)
            }
        },
        Some(other) => {
            eprintln!("compsize-tree: unknown command '{other}'\n");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

pub fn is_root() -> bool {
    unsafe { geteuid() == 0 }
}

fn cmd_extent_probe(path: &PathBuf) -> ExitCode {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            return ExitCode::from(1);
        }
    };
    let fd = std::os::fd::AsRawFd::as_raw_fd(&f);
    let started = Instant::now();
    match metadata::probe_extent_tree(fd) {
        Ok((extents, bytes, refs, shared, metadata, roots)) => {
            println!("extent tree: {extents} data extents, {bytes} bytes, {refs} references, {shared} shared-refs, {metadata} metadata extents");
            println!("  {} subvolumes reference data; biggest:", roots.len());
            for (root, n) in roots.iter().take(6) {
                println!("    root {root}: {n} refs");
            }
            println!("  read in {:.1}s", started.elapsed().as_secs_f64());
            if let Ok((n, refs, secs)) = metadata::probe_logical_ino(fd, 20000) {
                println!(
                    "  logical-ino on {n} extents: {refs} refs in {secs:.2}s ({:.1} us/extent)",
                    secs * 1e6 / n.max(1) as f64
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("extent tree read failed: {e}");
            ExitCode::from(1)
        }
    }
}

/// Development aid: where two scans of the same tree disagree, by path.
/// Nodes inside a collapsed subvolume are skipped, since the collapsed scan
/// deliberately has nothing there.
fn cmd_diff(args: &[String]) -> ExitCode {
    let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
        eprintln!("diff needs two caches: compsize-tree diff A.cst B.cst [--top N]");
        return ExitCode::from(2);
    };
    let load = |p: &String| -> Result<model::Tree, String> {
        cache::Cache::load_auto(&PathBuf::from(p))
            .map(|c| c.into_tree())
            .map_err(|e| format!("{p}: {e}"))
    };
    let (ta, tb) = match (load(a), load(b)) {
        (Ok(x), Ok(y)) => (x, y),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let top: usize = args
        .iter()
        .position(|s| s == "--top")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let collect = |t: &model::Tree| {
        let mut out: std::collections::HashMap<Vec<u8>, (u64, u64)> = Default::default();
        let mut stack: Vec<u32> = t.roots.clone();
        while let Some(id) = stack.pop() {
            let n = t.node(id);
            if n.flags & model::FLAG_COLLAPSED != 0 {
                continue; // the collapsed side has no interior by design
            }
            out.insert(
                t.path_of(id),
                (n.owned, t.dir(id).compsize),
            );
            stack.extend(t.child_ids(id));
        }
        out
    };
    let (ma, mb) = (collect(&ta), collect(&tb));
    let mut diffs: Vec<(i64, Vec<u8>, (u64, u64), (u64, u64))> = Vec::new();
    for (path, va) in &ma {
        if let Some(vb) = mb.get(path) {
            let d = va.0 as i64 - vb.0 as i64;
            if d != 0 {
                diffs.push((d, path.clone(), *va, *vb));
            }
        }
    }
    diffs.sort_by_key(|(d, _, _, _)| -d.abs());
    let sum = |m: &std::collections::HashMap<Vec<u8>, (u64, u64)>| -> (u64, u64) {
        m.values().fold((0, 0), |a, v| (a.0 + v.0, a.1 + v.1))
    };
    let (sa, sb) = (sum(&ma), sum(&mb));
    println!(
        "compared paths (live subtrees): owned {} vs {} ({:+}), compsize {} vs {} ({:+})",
        sa.0, sb.0, sb.0 as i64 - sa.0 as i64, sa.1, sb.1, sb.1 as i64 - sa.1 as i64
    );
    println!(
        "whole tree: owned {} vs {} ({:+})",
        ta.node(ta.roots[0]).owned,
        tb.node(tb.roots[0]).owned,
        tb.node(tb.roots[0]).owned as i64 - ta.node(ta.roots[0]).owned as i64
    );
    println!("{} paths compared, {} differ", ma.len(), diffs.len());
    for (d, path, va, vb) in diffs.iter().take(top) {
        println!(
            "  {:>13}  owned {:>12} vs {:>12}  compsize {:>12} vs {:>12}  {}",
            d,
            va.0,
            vb.0,
            va.1,
            vb.1,
            String::from_utf8_lossy(path)
        );
    }
    ExitCode::SUCCESS
}

fn cmd_check(path: &PathBuf) -> ExitCode {
    let started = Instant::now();
    let cache = match cache::Cache::load_auto(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            return ExitCode::from(1);
        }
    };
    let tree = cache.into_tree();
    let root = tree.roots.first().copied().unwrap_or(tree.root);
    let stats = tree.stats.clone();
    println!(
        "{}: {} nodes, {} dirs, {} files, {} subvolumes, scanned {}",
        path.display(),
        tree.nodes.len(),
        tree.dirs.len(),
        stats.files,
        stats.subvolumes,
        tree.scanned_roots
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  {} extents, {} references, {} files, {} dirs",
        stats.extents, stats.refs, stats.files, stats.dirs
    );
    println!(
        "  root {}: owned {}, compsize {}, freeable {}, referenced {}, uncompressed {}",
        String::from_utf8_lossy(&tree.path_of(root)),
        model::human(tree.node(root).owned),
        model::human(tree.dir(root).compsize),
        model::human(tree.dir(root).freeable),
        model::human(tree.node(root).referenced),
        model::human(tree.dir(root).uncomp_reach),
    );
    println!("  loaded in {:.1}s", started.elapsed().as_secs_f64());
    ExitCode::SUCCESS
}

fn need_root() -> bool {
    let root = unsafe { geteuid() } == 0;
    if !root {
        eprintln!("warning: the btrfs extent ioctl needs root; run with sudo");
    }
    root
}

/// Hand a file we wrote as root to the user who invoked us.
pub fn hand_over(path: &std::path::Path) {
    let Some((uid, gid, _home)) = cache::invoking_user() else {
        return;
    };
    if let Ok(c) = CString::new(path.as_os_str().as_bytes()) {
        unsafe { chown(c.as_ptr(), uid, gid) };
    }
}

fn parse_usize(flag: &str, v: Option<&String>) -> Result<usize, ExitCode> {
    match v.and_then(|s| s.parse::<usize>().ok()) {
        Some(n) => Ok(n),
        None => {
            eprintln!("{flag} needs a number");
            Err(ExitCode::from(2))
        }
    }
}

pub fn fmt_bytes(x: u64) -> String {
    model::human(x)
}

// ---------------------------------------------------------------------------

fn cmd_report(args: &[String]) -> ExitCode {
    let mut bytes = false;
    let mut buf_size = 1usize << 20;
    let mut paths: Vec<&str> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-b" | "--bytes" => bytes = true,
            "--buffer" => {
                i += 1;
                match parse_usize("--buffer", args.get(i)) {
                    Ok(n) => buf_size = n,
                    Err(c) => return c,
                }
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            s if s.starts_with('-') && s.len() > 1 => {
                eprintln!("compsize-tree: unknown option '{s}'");
                return ExitCode::from(2);
            }
            s => paths.push(s),
        }
        i += 1;
    }
    if paths.is_empty() {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    }
    need_root();

    let mut walker = report::Walker::new(buf_size);
    for p in &paths {
        if let Err(e) = walker.walk_root(p.as_bytes()) {
            eprintln!("{p}: {e}");
            return ExitCode::from(1);
        }
    }
    for w in walker.warnings() {
        eprintln!("{w}");
    }
    let empty = walker.stats().uncomp_all() == 0;
    if let Err(e) = report::print_stats(walker.stats(), bytes) {
        eprintln!("write error: {e}");
        return ExitCode::from(1);
    }
    if empty {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_scan(args: &[String]) -> ExitCode {
    let mut opts = walk::ScanOptions::default();
    let mut depth = 2usize;
    let mut top = 12usize;
    let mut files = 0usize;
    let mut raw = false;
    let mut dump_subvols = false;
    let mut compress = false;
    let mut reproducible = false;
    let mut save: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-j" | "--threads" => {
                i += 1;
                match parse_usize("--threads", args.get(i)) {
                    Ok(n) => opts.threads = n,
                    Err(c) => return c,
                }
            }
            "-L" | "--follow-symlinks" => opts.follow_symlinks = true,
            "--exclude" => {
                i += 1;
                match args.get(i) {
                    Some(p) => opts.excludes.push(p.as_bytes().to_vec()),
                    None => {
                        eprintln!("--exclude needs a path");
                        return ExitCode::from(2);
                    }
                }
            }
            "--buffer" => {
                i += 1;
                match parse_usize("--buffer", args.get(i)) {
                    Ok(n) => opts.buf_size = n,
                    Err(c) => return c,
                }
            }
            "--depth" => {
                i += 1;
                match parse_usize("--depth", args.get(i)) {
                    Ok(n) => depth = n,
                    Err(c) => return c,
                }
            }
            "--top" => {
                i += 1;
                match parse_usize("--top", args.get(i)) {
                    Ok(n) => top = n,
                    Err(c) => return c,
                }
            }
            "--files" => {
                i += 1;
                match parse_usize("--files", args.get(i)) {
                    Ok(n) => files = n,
                    Err(c) => return c,
                }
            }
            "-b" | "--bytes" => raw = true,
            "--full-scan" => opts.full_scan = true,
            "--dump-subvols" => dump_subvols = true,
            "--compress" => compress = true,
            "--reproducible" => reproducible = true,
            "--save" => {
                i += 1;
                match args.get(i) {
                    Some(p) => save = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--save needs a file");
                        return ExitCode::from(2);
                    }
                }
            }
            "--progress" => opts.progress = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            s if s.starts_with('-') && s.len() > 1 => {
                eprintln!("compsize-tree: unknown option '{s}'");
                return ExitCode::from(2);
            }
            s => opts.roots.push(s.as_bytes().to_vec()),
        }
        i += 1;
    }
    if opts.roots.is_empty() {
        opts.roots.push(b"/".to_vec());
    }
    need_root();
    if !opts.excludes.is_empty() {
        eprintln!("note: --exclude makes extents shared with excluded paths look exclusive");
    }

    let started = Instant::now();
    // Reading the trees is the default: it is byte-identical to the file walk
    // (the cowdiff gate) and 2.5x faster. CST_BACKEND=files selects the file
    // walker, which is what the gate compares against and what `report` uses.
    let backend = std::env::var("CST_BACKEND").unwrap_or_default();
    let scan_result = if backend == "files" {
        walk::scan_tree(&opts)
    } else {
        metadata::scan_tree(&opts)
    };
    let (tree, stats) = match scan_result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("scan failed: {e}");
            return ExitCode::from(1);
        }
    };
    let ropt = report::TreeReportOptions {
        depth,
        top,
        raw,
        files,
    };
    print!("{}", report::render_tree_report(&tree, &stats, &ropt));
    if dump_subvols {
        let seen: std::collections::HashSet<u64> = tree.subvols.iter().map(|s| s.id).collect();
        println!("inventory: {} subvolumes", tree.inventory.len());
        for r in &tree.inventory {
            println!(
                "  id {:<6} uuid {} parent {} flags {:#x} otime {} {}",
                r.treeid,
                btrfs::fmt_uuid(&r.uuid),
                btrfs::fmt_uuid(&r.parent_uuid),
                r.flags,
                r.otime_sec,
                if seen.contains(&r.treeid) { "seen" } else { "" }
            );
        }
    }
    if !tree.warnings.is_empty() {
        eprintln!("\n{} warnings, first {}:", tree.warnings.len(), tree.warnings.len().min(10));
        for w in tree.warnings.iter().take(10) {
            eprintln!("  {w}");
        }
    }

    if let Some(path) = save {
        let mut c = cache::Cache::from_tree(&tree);
        if reproducible {
            c.scanned_at = 0;
            c.stats.elapsed = std::time::Duration::ZERO;
            c.stats.fd_fallback = 0;
            c.warnings.clear();
        }
        let res = if compress {
            c.write_compressed(&path)
        } else {
            c.write(&path)
        };
        match res {
            Ok(()) => {
                hand_over(&path);
                eprintln!(
                    "saved {} ({:.2} GiB)",
                    path.display(),
                    cache::size_of(&path) as f64 / (1024.0 * 1024.0 * 1024.0)
                );
            }
            Err(e) => eprintln!("could not save {}: {e}", path.display()),
        }
    }
    eprintln!("total {:.1}s", started.elapsed().as_secs_f64());
    ExitCode::SUCCESS
}

