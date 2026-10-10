# compsize-tree

Extent-accurate disk usage for btrfs: **one pass** over the tree gives every
directory's true size, and a GTK4 browser shows it the way baobab would - if baobab
understood reflinks, snapshots and compression.

```
sudo compsize-tree                     # the app: scan a path, or open a saved scan
sudo compsize-tree scan /home          # headless: one pass, per-directory metrics
```

The graphical browser (GTK4) starts with no arguments. From there you can pick a
directory to scan - with live progress and a cancel button - or open a scan saved
earlier, and save the current one wherever you like through a normal file dialog.
Nothing is written to disk unless you ask for it, and there is no cache directory:
a saved scan is just a file you choose the location of.

Scanning needs root (the kernel's extent ioctl requires CAP_SYS_ADMIN), so start the
app with `sudo compsize-tree`; opening a saved scan works unprivileged.

## Why `du`/baobab are wrong here, and compsize is tedious

`du` and baobab sum `st_blocks` per file. On btrfs that counts every reflink,
every snapshot copy and every container layer in full, so a disk with ~605 GB
of real data reports 27 TB. `compsize` is correct - it dedupes physical
extents - but it works one path at a time, serially, and each run dedupes only
within itself, so per-directory results do not add up and you cannot compare
them.

This tool walks the tree once, builds the global extent -> references map, and
then answers for *every* directory at once.

## The numbers

For every extent `e` (physical, deduplicated) with size `L(e)` and a set of
file references, each reference belongs to a class taken from the btrfs
subvolume graph: **live** (a subvolume that is not a snapshot of another
scanned subvolume) or **snapshot** (its `parent_uuid` still exists on this
filesystem).

| metric | meaning |
|---|---|
| **owned** | Semantic ownership: **the oldest holder of an extent owns it**. Every reference carries a key of (the file's birth time, the creation time of the subvolume it lives in); the smallest key owns the extent, and references sharing that smallest key split it evenly. A snapshot shares its inodes with the subvolume it was taken from, so the two tie on birth time and the older subvolume - the original - keeps ownership; a reflink copy is strictly younger than its source, whichever subvolume it landed in. Delete the parent and the oldest snapshot becomes the new custodian, exactly as if ownership had transferred. Additive: children sum to the parent and the whole tree sums to the true physical usage. This is the default. |
| **freeable** | Bytes that deleting this path would actually free: extents whose references are *all* below it. Same idea as `btrfs fi du`'s "Exclusive", but per subtree and compression-aware. |
| **compsize** | Exactly what `compsize <path>` prints as Disk Usage: every extent referenced anywhere below, counted once. The cross-check number (it is order-independent even though compsize itself is not). |
| **referenced** | What the files claim (du/baobab's number, the 27 TB). |
| **uncompressed** | Deduplicated data before compression; `compsize / uncompressed` is the effective compression ratio. |

A fresh snapper snapshot of `/home` therefore shows a few hundred MB owned (its CoW cost so
far) instead of "another 605 GB"; when the live tree overwrites or deletes something, that data's
ownership moves to the snapshot holding it, and `/home/you` keeps its compsize number even
though 190 snapshots reference the same extents. In a reflink farm the original keeps the shared
bytes and every copy owns only its own deltas, so the numbers do not move when an unrelated copy
appears or is deleted.

## Commands

### the app (no arguments, or `view FILE`)

Needs root to scan. The window has a lazily-loaded file list, a sunburst ring chart
(click a wedge to descend, hover for details) that switches to a treemap, a metric
selector, badges for snapshot and shared entries, and a status line summarising the
scan. Files: `Open…` loads a saved scan, `Save scan…` writes the current one
(zstd-compressed when it helps), both through native file dialogs.

### `scan` (needs root)

```
sudo compsize-tree scan [options] PATH...
```

One pass, then per-directory metrics. The scan itself never needs a cache: the tree
lives in memory for the report, and a saved scan is there so browsing later does not
mean re-walking 37M files.

`--save FILE` writes the scan wherever you point it, and hands the file to the invoking
user, so a scan run under `sudo` leaves something you can open without it. Budget ~90
bytes per file: ~3.5 GB for a 37M file `/home`, or ~750 MB with `--compress`.

| option | |
|---|---|
| `-j, --threads N` | worker threads (default: CPUs) |
| `-L, --follow-symlinks` | follow symlinks found while walking (off: matches compsize) |
| `--exclude PATH` | skip a path prefix; extents shared with it will look exclusive to what remains |
| `--save FILE` | also write the scan to FILE, so the app can open it later |
| `--compress` | compress the saved scan (3-5x smaller) |
| `--buffer BYTES` | kernel search buffer per thread (default 1 MiB; larger is faster on files with millions of extents) |
| `--depth N`, `--top N`, `--files N` | how much of the text report to print |
| `-b, --bytes` | raw byte counts instead of human units |
| `--progress` | live file/dir counters |
| `--dump-subvols` | print the subvolume inventory with snapshot classification |

### `report` (the compsize-compatible verifier)

```
sudo compsize-tree report [-b] PATH...
```

Reproduces `compsize`'s table byte for byte from its own walk. Useful to prove
the extent layer: `compsize-tree report -b /some/dir` and
`compsize -b /some/dir` produce identical output, including on multi-million
extent files where this implementation is ~5x faster thanks to a larger search
buffer.

## Limits

* Sizes are relative to the scanned roots: an extent shared with a subvolume
  that was not walked is credited here. `scan` warns when it can see snapshots
  of a scanned subvolume outside the scanned paths. Scanning `/` (the default)
  avoids this entirely.
* A scan is a snapshot in time; files changing during it are counted once, as
  seen.
* Non-btrfs filesystems inside the tree (loop mounts, overlayfs, `/boot`) are
  detected and skipped, and counted in the summary. So is a *different* btrfs
  filesystem mounted inside the scanned tree: extent addresses are only unique
  within one filesystem, so mixing two would corrupt the dedup. The check uses
  the btrfs superblock UUID (`BTRFS_IOC_FS_INFO`), which is per filesystem and
  stable - not `statfs`'s `f_fsid`, which varies per subvolume. Scanning such a
  filesystem works by starting the scan there, and the summary says how many
  were skipped.
* A subvolume reachable through more than one mount point is walked once, so
  its files do not gain a second reference.
* Symlinks are skipped unless `--follow-symlinks`; hardlinked paths are counted
  per path, like compsize does.
* `owned`/`referenced` for a *file* is its share; `compsize`/`freeable` are
  stored per directory (a file's value shows its owned share).
* `owned` and `freeable` are *physical*: a snapshot only owns bytes that would
  actually be released if the live tree let go of them. A snapshot holding an
  old version of a file whose physical extent the live tree still partly uses
  therefore shows 0 - deleting it would free nothing, which is what the number
  is for. Use `compsize`/`referenced` for the logical view.
* A partial overwrite is not always a clean split. btrfs keeps a file extent
  item pointing at the *whole* physical extent with an offset into it, so after
  overwriting the first 5 MB of a 20 MB file the old extent (still referenced
  by the snapshot) is also referenced by the live file's remaining 15 MB. Since
  btrfs frees an extent only when its refcount reaches zero, at extent
  granularity, deleting that snapshot frees *nothing* - measured with
  `btrfs filesystem usage` as a -4096 byte change (noise). The unreferenced
  head of such an extent is dead space inside live data, and this tool reports
  it that way: the snapshot shows `freeable 0`, while `compsize` still shows
  the 25 MB it logically holds. When the live file is deleted instead of
  overwritten, the extent becomes snapshot-only and `freeable` matches
  `compsize` - the "ownership transfer" case.
* No cleanup actions: the tool only reads.

## Implementation notes

The scanner is std-only Rust; the browser is GTK4 (gtk4-rs). Per file it asks the kernel for the inode's
`EXTENT_DATA` items with `BTRFS_IOC_TREE_SEARCH_V2`, exactly like compsize,
and reads the packed `btrfs_file_extent_item` layout directly. One `statx` per file supplies the
inode, mtime and birth time the ownership rule needs; that replaced `fstat`, and the ioctl reuses the
containing directory's descriptor (it only needs *a* descriptor from the file's subvolume, since the
inode number travels in the search key), so a file costs two syscalls instead of four. Subvolume
identity comes from `BTRFS_IOC_GET_SUBVOL_INFO` at `st_dev` boundaries plus a
root-tree scan for the UUID inventory, so snapshots are recognised
structurally rather than by path convention. Compression, inline extents,
prealloc and holes are all accounted the way compsize accounts them.

--- this branch ---
