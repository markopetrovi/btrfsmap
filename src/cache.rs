//! Cache file: the result of a scan, in a fixed-width layout that loads back
//! with a single read per array.
//!
//! Written by `scan` (running as root), read by `serve` (unprivileged).

use crate::model::*;
use crate::walk::ScanStats;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"CSTREE\x01\n";
const VERSION: u32 = 5;

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
            self.write_to(&mut w)?;
            w.flush()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn write_to<W: Write>(&self, sink: &mut W) -> io::Result<()> {
        let mut w = &mut *sink;
        {
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
            put_u64(&mut w, self.stats.skipped_otherfs)?;
            put_u64(&mut w, self.stats.unreadable)?;
            put_u64(&mut w, self.stats.fd_fallback)?;
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
                put_u64(&mut w, s.otime)?;
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
        }
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
            skipped_otherfs: read_u64(&mut r)?,
            unreadable: read_u64(&mut r)?,
            fd_fallback: read_u64(&mut r)?,
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
            let otime = read_u64(&mut r)?;
            let mut flags2 = [0u8; 2];
            r.read_exact(&mut flags2)?;
            subvols.push(Subvol {
                id,
                uuid,
                parent_uuid,
                flags,
                otime,
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
    /// Snapshot a finished scan into the on-disk representation.
    pub fn from_tree(tree: &Tree) -> Cache {
        Cache {
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
        }
    }

    /// Compress on the way to disk, in-process: 4-5x smaller, and `load_auto`
    /// decompresses it again transparently.
    pub fn write_compressed(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let f = File::create(&tmp)?;
            let mut enc = zstd::stream::write::Encoder::new(BufWriter::with_capacity(8 << 20, f), 3)?;
            self.write_to(&mut enc)?;
            enc.finish()?.flush()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Read either a plain or a zstd-compressed cache.
    pub fn load_auto(path: &Path) -> io::Result<Cache> {
        let mut magic = [0u8; 4];
        {
            let mut f = File::open(path)?;
            if f.read(&mut magic)? < 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "cache too short"));
            }
        }
        if magic == [0x28, 0xb5, 0x2f, 0xfd] {
            let dec = zstd::stream::read::Decoder::new(File::open(path)?)?;
            Cache::read_from(io::BufReader::with_capacity(8 << 20, dec))
        } else {
            Cache::load(path)
        }
    }
}

/// Bytes on disk, for the UI status line.
pub fn size_of(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
