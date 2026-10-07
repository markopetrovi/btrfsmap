//! Cache file: the result of a scan, in a fixed-width layout that loads back
//! with a single read per array.
//!
//! Written by `scan` (running as root), read by `serve` (unprivileged).

use crate::model::*;
use crate::walk::ScanStats;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"CSTREE\x01\n";
const VERSION: u32 = 2;

pub struct Cache {
    pub nodes: Vec<Node>,
    pub names: Vec<u8>,
    pub dirs: Vec<DirExtra>,
    pub roots: Vec<u32>,
    pub root: u32,
    pub subvols: Vec<Subvol>,
    pub stats: ScanStats,
    pub warnings: Vec<String>,
    pub scanned_roots: Vec<Vec<u8>>,
    pub scanned_at: u64,
}

/// Field offsets are baked into the file format; a mismatch means the cache
/// would be read back as garbage, so refuse it loudly instead.
fn layout_fingerprint() -> u64 {
    let n = std::mem::size_of::<Node>() as u64;
    let d = std::mem::size_of::<DirExtra>() as u64;
    n << 32 | d
}

fn put_u32<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn put_u64<W: Write>(w: &mut W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn put_bytes<W: Write>(w: &mut W, b: &[u8]) -> io::Result<()> {
    put_u32(w, b.len() as u32)?;
    w.write_all(b)
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_bytes<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let n = read_u32(r)? as usize;
    let mut v = vec![0u8; n];
    r.read_exact(&mut v)?;
    Ok(v)
}

fn read_array<T: Copy, R: Read>(r: &mut R, count: usize) -> io::Result<Vec<T>> {
    let mut v: Vec<T> = Vec::with_capacity(count);
    let bytes = std::mem::size_of::<T>() * count;
    // Safety: T is a plain-old-data struct of integers; any bit pattern is valid.
    unsafe {
        v.set_len(count);
        let dst = std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, bytes);
        r.read_exact(dst)?;
    }
    Ok(v)
}

impl Cache {
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let f = File::create(&tmp)?;
            let mut w = BufWriter::with_capacity(8 << 20, f);
            w.write_all(MAGIC)?;
            put_u32(&mut w, VERSION)?;
            put_u64(&mut w, layout_fingerprint())?;
            put_u64(&mut w, self.scanned_at)?;
            put_u32(&mut w, self.nodes.len() as u32)?;
            put_u32(&mut w, self.dirs.len() as u32)?;
            put_u32(&mut w, self.names.len() as u32)?;
            put_u32(&mut w, self.roots.len() as u32)?;
            put_u32(&mut w, self.root)?;
            put_u32(&mut w, self.subvols.len() as u32)?;
            put_u32(&mut w, self.warnings.len() as u32)?;
            put_u32(&mut w, self.scanned_roots.len() as u32)?;

            put_u64(&mut w, self.stats.files)?;
            put_u64(&mut w, self.stats.dirs)?;
            put_u64(&mut w, self.stats.extents)?;
            put_u64(&mut w, self.stats.refs)?;
            put_u64(&mut w, self.stats.subvolumes)?;
            put_u64(&mut w, self.stats.skipped_nonbtrfs)?;
            put_u64(&mut w, self.stats.unreadable)?;
            put_u64(&mut w, self.stats.elapsed.as_millis() as u64)?;

            let nodes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    self.nodes.as_ptr() as *const u8,
                    std::mem::size_of_val(self.nodes.as_slice()),
                )
            };
            w.write_all(nodes)?;
            let dirs: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    self.dirs.as_ptr() as *const u8,
                    std::mem::size_of_val(self.dirs.as_slice()),
                )
            };
            w.write_all(dirs)?;
            w.write_all(&self.names)?;
            for r in &self.roots {
                put_u32(&mut w, *r)?;
            }
            for s in &self.subvols {
                put_u64(&mut w, s.id)?;
                w.write_all(&s.uuid)?;
                w.write_all(&s.parent_uuid)?;
                put_u64(&mut w, s.flags)?;
                put_u64(&mut w, s.ctime)?;
                w.write_all(&[s.snapshot as u8, s.seen as u8])?;
                put_bytes(&mut w, &s.name)?;
                put_bytes(&mut w, &s.path)?;
            }
            for w2 in &self.warnings {
                put_bytes(&mut w, w2.as_bytes())?;
            }
            for r in &self.scanned_roots {
                put_bytes(&mut w, r)?;
            }
            w.flush()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> io::Result<Cache> {
        let f = File::open(path)?;
        Cache::read_from(io::BufReader::with_capacity(8 << 20, f))
    }

    /// Parse a cache from any reader, so a compressed cache can be streamed
    /// straight out of `zstd -d -c` without a temporary copy on disk.
    pub fn read_from<R: Read>(r: R) -> io::Result<Cache> {
        let mut r = r;
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a compsize-tree cache",
            ));
        }
        let version = read_u32(&mut r)?;
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cache version {version}, expected {VERSION} - rescan"),
            ));
        }
        let fp = read_u64(&mut r)?;
        if fp != layout_fingerprint() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cache was written by an incompatible build - rescan",
            ));
        }
        let scanned_at = read_u64(&mut r)?;
        let n_nodes = read_u32(&mut r)? as usize;
        let n_dirs = read_u32(&mut r)? as usize;
        let n_names = read_u32(&mut r)? as usize;
        let n_roots = read_u32(&mut r)? as usize;
        let root = read_u32(&mut r)?;
        let n_subvols = read_u32(&mut r)? as usize;
        let n_warnings = read_u32(&mut r)? as usize;
        let n_scanned_roots = read_u32(&mut r)? as usize;

        let stats = ScanStats {
            files: read_u64(&mut r)?,
            dirs: read_u64(&mut r)?,
            extents: read_u64(&mut r)?,
            refs: read_u64(&mut r)?,
            subvolumes: read_u64(&mut r)?,
            skipped_nonbtrfs: read_u64(&mut r)?,
            unreadable: read_u64(&mut r)?,
            elapsed: std::time::Duration::from_millis(read_u64(&mut r)?),
        };

        let nodes: Vec<Node> = read_array(&mut r, n_nodes)?;
        let dirs: Vec<DirExtra> = read_array(&mut r, n_dirs)?;
        let mut names = vec![0u8; n_names];
        r.read_exact(&mut names)?;
        let mut roots = Vec::with_capacity(n_roots);
        for _ in 0..n_roots {
            roots.push(read_u32(&mut r)?);
        }
        let mut subvols = Vec::with_capacity(n_subvols);
        for _ in 0..n_subvols {
            let id = read_u64(&mut r)?;
            let mut uuid = [0u8; 16];
            r.read_exact(&mut uuid)?;
            let mut parent_uuid = [0u8; 16];
            r.read_exact(&mut parent_uuid)?;
            let flags = read_u64(&mut r)?;
            let ctime = read_u64(&mut r)?;
            let mut flags2 = [0u8; 2];
            r.read_exact(&mut flags2)?;
            subvols.push(Subvol {
                id,
                uuid,
                parent_uuid,
                flags,
                ctime,
                name: read_bytes(&mut r)?,
                path: read_bytes(&mut r)?,
                seen: flags2[1] != 0,
                snapshot: flags2[0] != 0,
            });
        }
        let mut warnings = Vec::with_capacity(n_warnings);
        for _ in 0..n_warnings {
            warnings.push(String::from_utf8_lossy(&read_bytes(&mut r)?).into_owned());
        }
        let mut scanned_roots = Vec::with_capacity(n_scanned_roots);
        for _ in 0..n_scanned_roots {
            scanned_roots.push(read_bytes(&mut r)?);
        }
        Ok(Cache {
            nodes,
            names,
            dirs,
            roots,
            root,
            subvols,
            stats,
            warnings,
            scanned_roots,
            scanned_at,
        })
    }

    /// Move the loaded arrays into the tree the report and the server use.
    pub fn into_tree(self) -> Tree {
        Tree {
            nodes: self.nodes,
            names: self.names,
            dirs: self.dirs,
            roots: self.roots,
            root: self.root,
            subvols: self.subvols,
            inventory: Vec::new(),
            warnings: self.warnings,
            stats: self.stats,
            scanned_at: self.scanned_at,
            scanned_roots: self.scanned_roots,
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Look up the user who invoked sudo, so the cache can be readable by them
/// while the scan itself runs as root.
pub fn invoking_user() -> Option<(u32, u32, String)> {
    let uid: u32 = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid: u32 = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 6 && f[2].parse::<u32>().ok() == Some(uid) {
            return Some((uid, gid, f[5].to_string()));
        }
    }
    None
}

impl Cache {
    /// Write through the `zstd` binary: 3-4x smaller on disk, and `load`
    /// transparently decompresses it again.
    pub fn write_compressed(&self, path: &Path) -> io::Result<()> {
        let plain = path.with_extension("plain.tmp");
        self.write(&plain)?;
        let out = std::process::Command::new("zstd")
            .args(["-q", "-f", "-3", "-o"])
            .arg(path.with_extension("zst.tmp"))
            .arg(&plain)
            .status();
        let _ = fs::remove_file(&plain);
        match out {
            Ok(s) if s.success() => {
                fs::rename(path.with_extension("zst.tmp"), path)?;
                Ok(())
            }
            Ok(s) => Err(io::Error::other(format!("zstd exited with {s}"))),
            Err(e) => Err(io::Error::other(format!("cannot run zstd: {e}"))),
        }
    }

    /// Read either a plain or a zstd-compressed cache.
    pub fn load_auto(path: &Path) -> io::Result<Cache> {
        let mut magic = [0u8; 4];
        {
            let mut f = File::open(path)?;
            use std::io::Read as _;
            if f.read(&mut magic)? < 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "cache too short"));
            }
        }
        if magic == [0x28, 0xb5, 0x2f, 0xfd] {
            // zstd frame: stream the decompressed bytes straight into the parser
            let mut child = std::process::Command::new("zstd")
                .args(["-q", "-d", "-c"])
                .arg(path)
                .stdout(std::process::Stdio::piped())
                .spawn()?;
            let out = child.stdout.take().expect("piped");
            let r = io::BufReader::with_capacity(8 << 20, out);
            let c = Cache::read_from(r);
            let _ = child.wait();
            c
        } else {
            Cache::load(path)
        }
    }
}


/// Cache directory, honouring XDG_CACHE_HOME.
pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_CACHE_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join("compsize-tree");
        }
    }
    let home = invoking_user()
        .map(|(_, _, h)| PathBuf::from(h))
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".cache/compsize-tree")
}

/// One cache per scanned root set, so scans do not clobber each other.
pub fn default_path(roots: &[Vec<u8>]) -> PathBuf {
    let mut slug = String::new();
    for r in roots {
        if !slug.is_empty() {
            slug.push('+');
        }
        for b in r {
            slug.push(match b {
                b'/' => '_',
                b if b.is_ascii_alphanumeric() || *b == b'.' || *b == b'-' || *b == b'_' => *b as char,
                _ => '_',
            });
        }
    }
    if slug.is_empty() {
        slug = "scan".into();
    }
    if slug.len() > 60 {
        slug.truncate(60);
    }
    dir().join(format!("{slug}.cst"))
}

#[derive(Clone)]
pub struct CacheFile {
    pub name: String,
    pub bytes: u64,
    pub mtime: u64,
}

/// Every cache on disk, newest first - this is what the UI reports and cleans.
pub fn list() -> Vec<CacheFile> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir()) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("cst") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        out.push(CacheFile {
            name: e.file_name().to_string_lossy().into_owned(),
            bytes: md.len(),
            mtime: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.mtime));
    out
}

pub fn newest() -> Option<PathBuf> {
    list().first().map(|c| dir().join(&c.name))
}

/// Delete one cache (or all of them). Only ever touches `*.cst` directly inside
/// the cache directory - names are validated, never joined blindly.
pub fn delete(name: Option<&str>) -> io::Result<usize> {
    let mut n = 0;
    for c in list() {
        if let Some(want) = name {
            if c.name != want {
                continue;
            }
        }
        if c.name.contains('/') || !c.name.ends_with(".cst") {
            continue;
        }
        if fs::remove_file(dir().join(&c.name)).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

pub fn size_of(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
