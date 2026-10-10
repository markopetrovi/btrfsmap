// SPDX-License-Identifier: GPL-2.0-or-later

//! The mount table: the one thing a tree walk cannot see.
//!
//! A subvolume mounted at a path is invisible to a reader of fs trees. The
//! containing tree holds an ordinary directory where the mount is - on this
//! machine `/home` is an empty directory of the root subvolume - and the mounted
//! subvolume's own tree hangs off nothing in it. `/proc/self/mountinfo` is the
//! only place that says a mount is there, so a scan of a path that holds one
//! has to read it or report less than the path holds.

use std::io;

/// One mount in this process's mount namespace.
pub struct Mount {
    /// Mount ID. The kernel hands these out in mount order, so a lower id is an
    /// older mount and no two are equal - which makes it the tie-break when one
    /// subvolume is mounted at several paths.
    pub id: u64,
    /// Where it is mounted, unescaped and absolute.
    pub path: Vec<u8>,
    /// The mounted filesystem's type, as the kernel spells it ("btrfs", "tmpfs").
    pub fs_type: Vec<u8>,
}

/// Every mount in the namespace, oldest first.
///
/// A line is
/// `id parent major:minor root mountpoint options [optional...] - fstype source superopts`,
/// with the optional fields of unknown count, so the type is read from the
/// first "-" field rather than from a fixed position.
pub fn list() -> io::Result<Vec<Mount>> {
    let text = std::fs::read("/proc/self/mountinfo")?;
    let mut out: Vec<Mount> = Vec::new();
    for line in text.split(|b| *b == b'\n') {
        let f: Vec<&[u8]> = line.split(|b| *b == b' ').collect();
        if f.len() < 6 {
            continue;
        }
        let Some(sep) = f.iter().position(|x| *x == b"-") else {
            continue;
        };
        let (Some(id), Some(fs_type)) = (num(f[0]), f.get(sep + 1)) else {
            continue;
        };
        out.push(Mount {
            id,
            path: unescape(f[4]),
            fs_type: fs_type.to_vec(),
        });
    }
    out.sort_by_key(|m| m.id);
    Ok(out)
}

fn num(s: &[u8]) -> Option<u64> {
    std::str::from_utf8(s).ok()?.parse().ok()
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
pub fn under(path: &[u8], root: &[u8]) -> bool {
    let Some(rest) = path.strip_prefix(root) else {
        return false;
    };
    rest.is_empty() || root.ends_with(b"/") || rest.starts_with(b"/")
}

/// The directory holding `path`, as a byte string: `/x` -> `/`.
pub fn parent_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|b| *b == b'/') {
        Some(0) => &path[..1],
        Some(i) => &path[..i],
        None => path,
    }
}

/// The last component of `path`: `/x/y` -> `y`.
pub fn name_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|b| *b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}
