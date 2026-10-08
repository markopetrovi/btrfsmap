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
| **owned** | Semantic ownership: **the oldest holder of an extent owns it**. Every reference carries a key of (the file's birth time, the creation time of the subvolume it lives in); the smallest key owns the extent, and references sharing that smallest key split it evenly. A reference from inside a *snapshot-class* subvolume carries **no** birth time: `(unknown, the snapshot's own creation time)`. So a snapshot owns what the live trees have let go of, and nothing they still hold - its files' birth times are not a fact about the data, since a reflink copy and a rename are enough to hand a live file a younger inode - and among snapshots the oldest is the custodian, so deleting the parent transfers ownership to it. A reflink copy inside the live tree is strictly younger than its source, whichever live subvolume it landed in. Additive: children sum to the parent and the whole tree sums to the true physical usage. This is the default. |
| **freeable** | Bytes that deleting this path would actually free: extents whose references are *all* below it. Same idea as `btrfs fi du`'s "Exclusive", but per subtree and compression-aware. |
| **compsize** | Exactly what `compsize <path>` prints as Disk Usage: every extent referenced anywhere below, counted once. The cross-check number (it is order-independent even though compsize itself is not). |
| **referenced** | What the files claim (du/baobab's number, the 27 TB). |
| **uncompressed** | Deduplicated data before compression; `compsize / uncompressed` is the effective compression ratio. |

A fresh snapper snapshot of `/home` therefore shows a few hundred MB owned (its CoW cost so
far) instead of "another 605 GB"; when the live tree overwrites or deletes something, that data's
ownership moves to the snapshot holding it, and `/home/marko` keeps its compsize number even
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
lives in memory for the report, and `serve` reads it back from a saved cache only so
that browsing later does not mean re-walking 37M files.

The scan reads the filesystem's own trees instead of the files - `INODE_ITEM`,
`DIR_INDEX` and `EXTENT_DATA` out of each subvolume's fs tree, subvolume nesting out
of the root tree - which needs no `statx` and no per-file ioctl and is about 2.5x
faster than the file walk for the same numbers.

**Snapshots are not walked.** A subvolume whose `parent_uuid` still resolves to
another subvolume of this filesystem is left as a single node holding its subtree's
aggregate (`owned`, `compsize`, `freeable`, `uncompressed`); its references come from
the extent tree instead, which is one pass over the whole filesystem no matter how
many snapshots there are. Inside it there is nothing to browse, and its `referenced`
- the logical size of the files in it - is not known, because that is only recorded
per file. On a `/home` with 30 snapper snapshots and ~90 container layers this is the
difference between ~160 s (and ~280 s through the files) and ~20 s warm, 36 s cold.
`--full-scan` walks them after all, for the interiors and for `referenced`, and it is
the exact answer; the collapsed scan lands within 1.5% of it on that tree, for the
reasons in [Limits](#limits).

With `--save` the cache goes to `$XDG_CACHE_HOME/compsize-tree/<roots>.cst`
(`~/.cache/...` by default, named after the scanned roots so scans do not clobber each
other) and is chowned to the invoking user, which is what lets `serve` run unprivileged.
Budget ~90 bytes per file: ~3.5 GB for a 37M file `/home`, or ~750 MB with `--compress`.
The UI footer reports the cache size and offers to delete it.

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
| `--full-scan` | walk snapshot-class subvolumes too (default: one node per snapshot, aggregates only) |
| `--progress` | live file/dir counters |
| `--dump-subvols` | print the subvolume inventory with snapshot classification |

### `serve`

```
compsize-tree serve [--cache PATH] [--port 8787] [--open]
```

Serves the newest cache in `$XDG_CACHE_HOME/compsize-tree` (or `--cache PATH`) and prints the
URL; `--open` launches the browser for you.
Local HTTP server on 127.0.0.1 serving a single-page UI: a lazily expanding
file list, a sunburst ring chart (click a wedge to descend) or treemap, a
metric selector, badges for snapshot/shared entries, biggest files/ directories
and name search. Nothing is sent anywhere; the server only reads the cache.

### `report` (the compsize-compatible verifier)

```
sudo compsize-tree report [-b] PATH...
```

Reproduces `compsize`'s table byte for byte from its own walk. Useful to prove
the extent layer: `compsize-tree report -b /some/dir` and
`compsize -b /some/dir` produce identical output, including on multi-million
extent files where this implementation is ~5x faster thanks to a larger search
buffer.

## Verifying a change

The file walker is the reference implementation: what the tree reader produces has
to match it byte for byte. It no longer ships in this tree - the
`deprecated-files-backend` branch keeps it, and its binary is kept around as the
gate. Scan the same tree with both and compare the caches:

```
sudo compsize-tree scan --full-scan --save /tmp/a.cst --reproducible PATH
sudo /tmp/cst-old-files scan --save /tmp/b.cst --reproducible PATH
cmp /tmp/a.cst /tmp/b.cst
```

`--reproducible` drops timings and warnings, which is what makes two scans of an
unchanged tree byte-identical. Pass `--full-scan` on both sides when the tree has
snapshot-class subvolumes: the collapsed scan is deliberately a different, aggregate
tree, so it is compared on its numbers instead (see Limits).

`compsize-tree diff A.cst B.cst [--top N]` prints where two scans disagree, by path,
largest first. `extent-probe PATH` reports what one pass over the extent tree sees -
data extents, references, metadata extents, and how many items' inline references
failed to parse (the count has to be zero). `resolve-probe PATH N` prints, for every
Nth data extent, the subvolumes the block-reference graph resolves its shared
references to, to be diffed against the kernel's own answer from
`btrfs inspect-internal logical-resolve`.

## Limits

* Sizes are relative to the scanned roots: an extent shared with a subvolume
  that was not walked is credited here. `scan` warns when it can see snapshots
  of a scanned subvolume outside the scanned paths. Scanning `/` (the default)
  avoids this entirely.
* **Inline payloads are not counted.** A file small enough to live inside its
  own inode's item - an *inline* extent - is stored in metadata, not in a data
  extent, so there is no extent item for it anywhere. It is not data usage, and
  counting it would mean also counting xattrs and directory items, which is
  metadata accounting this tool does not do. Inline bytes therefore appear only
  in `referenced` (what the file claims), never in `owned`, `compsize`,
  `freeable` or `uncompressed`. The one place they are counted as usage is
  `report`, which reproduces `compsize` byte for byte and compsize counts them.

  Everything else in a collapsed subvolume *is* accounted for: its references
  come from the extent tree, and subvolumes nested inside it are found through
  the root tree (`ROOT_BACKREF`) and have their own trees read. So the fast
  scan's aggregate is exact apart from the interiors being unbrowsable, and the
  two modes agree - on the tree measured (`/home`), `owned` came out within
  140 KB of each other at 597 GiB. Use `--full-scan` when the interiors of the
  collapsed subvolumes matter.
* `CST_RESOLVE_SECONDS=<n>` spends up to n seconds asking the kernel about the
  largest unresolved extents - the exact route, for a filesystem where the
  missing ones are the big ones. Default 0: it costs ~80-400 us per extent and
  found nothing on the tree above, so it is a knob rather than a default.
* The ownership rule treats a *snapshot-class* subvolume as one voice: its
  references carry no file birth time (unknown, with the subvolume's creation
  time as the tie-break), so a snapshot owns what the live trees have let go of
  and nothing they still hold, and among snapshots the oldest is the custodian.
  Its files' birth times are deliberately ignored there: a reflink copy and a
  rename are enough to hand a live file a younger inode, which would otherwise
  move ownership of data that never went anywhere to a snapshot. The rule is the
  same in the file walker and the tree reader, so the reference scan and this
  one agree.
* The file walker finds a handful of files the tree reader does not (4 of
  /home's 38M, 78 MB): they live in a subvolume the root-tree backref walk does
  not reach. It shows up as a `~78 MB` difference in `owned` for one subtree,
  and it predates the collapsed scan.
* A collapsed snapshot's `uncompressed` is its compressed size for the extents
  only it holds - the extent tree has no uncompressed size. Its `referenced` is
  0 for the same reason: the logical sizes live in the files, which were not
  read.
* A scan is a snapshot in time; files changing during it are counted once, as
  seen.
* Non-btrfs filesystems inside the tree (loop mounts, overlayfs, `/boot`) are
  never entered: the scan follows btrfs fs trees, and those hold only this
  filesystem's own files, so nothing under such a mount is read or counted.
  That is also what keeps a *different* btrfs filesystem mounted inside the
  scanned tree out of the dedup: its subvolumes are not in this filesystem's
  root tree, and extent addresses are only unique within one filesystem.
  Scanning such a filesystem works by starting the scan there. (The retired
  file walker had to detect this with `BTRFS_IOC_FS_INFO`, since it walked
  paths rather than trees; those stats are still in the cache layout and read
  zero.)
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

The scanner is std-only Rust; the browser is GTK4 (gtk4-rs). The scan reads the
filesystem's own trees with `BTRFS_IOC_TREE_SEARCH_V2`: `INODE_ITEM` (mode,
size, mtime and birth time), `DIR_INDEX` (the directory structure) and
`EXTENT_DATA` (the extents) out of each subvolume's fs tree, and the root tree's
`ROOT_ITEM` records for the UUID inventory - so no file is `statx`ed, opened or
even named by the kernel on its behalf. Subvolume identity comes from
`BTRFS_IOC_GET_SUBVOL_INFO`, so snapshots are recognised structurally rather
than by path convention. Compression, inline extents, prealloc and holes are all
accounted the way compsize accounts them.

`report` keeps the file walk: it is the compsize-compatible verifier, so it
works one path at a time, asks the kernel for that inode's `EXTENT_DATA` items
and reads the packed `btrfs_file_extent_item` layout directly, exactly like
compsize.
