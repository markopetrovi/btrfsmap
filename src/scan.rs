//! What a scan is handed and what it reports back.
//!
//! These types describe a scan, not how one is carried out: `ScanOptions` is
//! the input the CLI and the GUI assemble, `ScanStats` is the summary a
//! finished scan leaves behind (and the cache stores), and `Progress` is the
//! live counter set a GUI polls while the scan runs. They live here rather
//! than in a scanning module so the model, the cache, the report and the GUI
//! never have to name one.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// live counters for a GUI - which subvolume is being read, how many
    /// inodes have gone by, and the flag its cancel button sets - and `None`
    /// for a headless scan, which has nobody to report to
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
