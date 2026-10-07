//! compsize-tree: extent-accurate btrfs disk usage in one pass.
//!
//! Subcommands:
//!   report  - compsize-compatible table for the given paths (verification tool)
//!   scan    - one-pass scan of a whole tree, with per-directory metrics
//!   serve   - local web UI over a scan

mod btrfs;
mod cache;
mod model;
mod report;
mod server;
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
usage: compsize-tree <command> [options] [paths...]

commands:
  report [options] PATH...   print a compsize-compatible usage table
  scan   [options] PATH...   one pass, then per-directory metrics
  serve  [options]           browse a saved scan in a local web UI

report options:
  -b, --bytes              print raw byte counts instead of human-readable sizes
      --buffer BYTES       kernel search buffer per thread (default 1048576)

scan options:
  -j, --threads N          worker threads (default: number of CPUs)
  -L, --follow-symlinks    follow symlinks found while walking
      --exclude PATH       skip this path prefix (repeatable; makes shared
                           extents below it look exclusive - it is reported)
      --buffer BYTES       kernel search buffer per thread (default 1048576)
      --depth N            directory levels to print (default 2)
      --top N              entries per level (default 12)
      --files N            biggest files to list (default 0)
  -b, --bytes              raw byte counts in the report
      --dump-subvols       print the subvolume inventory and its classification
      --save               keep the scan as a cache so a later `serve` does not
                           have to re-walk the disk (nothing is written without
                           it; the scan itself never needs the cache)
      --cache PATH         write the cache to PATH (implies --save)
      --compress           write the cache through zstd (3-4x smaller, needs
                           the zstd binary; serve decompresses it)
      --progress           print progress while scanning
  -h, --help               this help

serve options:
      --cache PATH         cache to serve (default: the newest one in
                           ~/.cache/compsize-tree)
      --port N             port on 127.0.0.1 (default 8787, else a free one)
      --open               also launch the browser (off by default)
  -h, --help               this help
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    match cmd.as_str() {
        "report" => cmd_report(&args[1..]),
        "scan" => cmd_scan(&args[1..]),
        "serve" => cmd_serve(&args[1..]),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("compsize-tree: unknown command '{other}'\n");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn fmt_uuid(u: &[u8; 16]) -> String {
    if *u == [0u8; 16] {
        return "-".into();
    }
    let mut s = String::with_capacity(36);
    for (i, b) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Give the invoking user (not root) access to the cache and its directory.
fn hand_over(path: &std::path::Path) {
    let Some((uid, gid, _home)) = cache::invoking_user() else { return };
    for p in [path.parent(), Some(path)].into_iter().flatten() {
        if let Ok(c) = CString::new(p.as_os_str().as_bytes()) {
            unsafe { chown(c.as_ptr(), uid, gid) };
        }
    }
}

fn need_root() {
    if unsafe { geteuid() } != 0 {
        eprintln!("warning: the btrfs extent ioctl needs root; run with sudo");
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

// ---------------------------------------------------------------------------

fn cmd_scan(args: &[String]) -> ExitCode {
    let mut opts = walk::ScanOptions::default();
    let mut depth = 2usize;
    let mut top = 12usize;
    let mut raw = false;
    let mut files = 0usize;
    let mut dump_subvols = false;
    let mut compress = false;
    let mut save = false;
    let mut cache_path: Option<PathBuf> = None;

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
            "-b" | "--bytes" => raw = true,
            "--files" => {
                i += 1;
                match parse_usize("--files", args.get(i)) {
                    Ok(n) => files = n,
                    Err(c) => return c,
                }
            }
            "--dump-subvols" => dump_subvols = true,
            "--cache" => {
                i += 1;
                match args.get(i) {
                    Some(p) => cache_path = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--cache needs a path");
                        return ExitCode::from(2);
                    }
                }
            }
            "--compress" => compress = true,
            "--save" => save = true,
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
        eprintln!(
            "note: --exclude makes extents shared with excluded paths look exclusive to what remains"
        );
    }

    let started = Instant::now();
    let (tree, stats) = match walk::scan_tree(&opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("scan failed: {e}");
            return ExitCode::from(1);
        }
    };
    if dump_subvols {
        let seen: std::collections::HashSet<u64> = tree.subvols.iter().map(|s| s.id).collect();
        println!("inventory: {} subvolumes", tree.inventory.len());
        for r in &tree.inventory {
            println!(
                "  id {:<6} uuid {} parent {} flags {:#x} otime {} {}",
                r.treeid,
                fmt_uuid(&r.uuid),
                fmt_uuid(&r.parent_uuid),
                r.flags,
                r.otime_sec,
                if seen.contains(&r.treeid) { "seen" } else { "" }
            );
        }
    }
    let ropt = report::TreeReportOptions {
        depth,
        top,
        raw,
        files,
    };
    print!("{}", report::render_tree_report(&tree, &stats, &ropt));
    if !tree.warnings.is_empty() {
        eprintln!("\n{} warnings, first {}:", tree.warnings.len(), tree.warnings.len().min(10));
        for w in tree.warnings.iter().take(10) {
            eprintln!("  {w}");
        }
    }
    if !save && cache_path.is_none() {
        eprintln!("scan not saved (add --save to keep it for compsize-tree serve)");
        eprintln!("total {:.1}s", started.elapsed().as_secs_f64());
        return ExitCode::SUCCESS;
    }
    let path = cache_path.unwrap_or_else(|| cache::default_path(&opts.roots));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let c = cache::Cache {
        nodes: tree.nodes.clone(),
        names: tree.names.clone(),
        dirs: tree.dirs.clone(),
        roots: tree.roots.clone(),
        root: tree.root,
        subvols: tree.subvols.clone(),
        stats: tree.stats.clone(),
        warnings: tree.warnings.clone(),
        scanned_roots: tree.scanned_roots.clone(),
        scanned_at: tree.scanned_at,
    };
    let est = c.nodes.len() * std::mem::size_of::<model::Node>()
        + c.names.len()
        + c.dirs.len() * std::mem::size_of::<model::DirExtra>();
    let write_res = if compress {
        c.write_compressed(&path)
    } else {
        c.write(&path)
    };
    match write_res {
        Ok(()) => {
            hand_over(&path);
            let on_disk = cache::size_of(&path);
            eprintln!(
                "cache: {} ({:.2} GiB on disk) - browse with: compsize-tree serve",
                path.display(),
                on_disk as f64 / (1024.0 * 1024.0 * 1024.0)
            );
            if !compress && est > 1 << 30 {
                eprintln!("       (--compress writes it 3-4x smaller)");
            }
        }
        Err(e) => eprintln!(
            "could not write cache {} ({e}) - need about {:.1} GiB free; use --cache elsewhere",
            path.display(),
            est as f64 / (1024.0 * 1024.0 * 1024.0)
        ),
    }
    eprintln!("total {:.1}s", started.elapsed().as_secs_f64());
    ExitCode::SUCCESS
}

fn cmd_serve(args: &[String]) -> ExitCode {
    let mut cache_path: Option<PathBuf> = None;
    let mut port = 8787u16;
    let mut open = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cache" => {
                i += 1;
                match args.get(i) {
                    Some(p) => cache_path = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--cache needs a path");
                        return ExitCode::from(2);
                    }
                }
            }
            "--port" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse().ok()) {
                    Some(n) => port = n,
                    None => {
                        eprintln!("--port needs a number");
                        return ExitCode::from(2);
                    }
                }
            }
            "--open" => open = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            s if s.starts_with('-') && s.len() > 1 => {
                eprintln!("compsize-tree: unknown option '{s}'");
                return ExitCode::from(2);
            }
            s => cache_path = Some(PathBuf::from(s)),
        }
        i += 1;
    }
    let path = match cache_path.or_else(cache::newest) {
        Some(p) => p,
        None => {
            eprintln!(
                "no cache in {} - save one with: sudo compsize-tree scan --save /",
                cache::dir().display()
            );
            return ExitCode::from(1);
        }
    };
    let c = match cache::Cache::load_auto(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read cache {}: {e}", path.display());
            eprintln!("save one with: sudo compsize-tree scan --save /");
            return ExitCode::from(1);
        }
    };
    let tree = c.into_tree();
    let cache_bytes = cache::size_of(&path);
    match server::run(tree, port, open, path.display().to_string(), cache_bytes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("server error: {e}");
            ExitCode::from(1)
        }
    }
}
