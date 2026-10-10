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

## Building

Rust (any recent stable) with GTK4 development headers - `gtk4-devel` on
Fedora, `libgtk-4-dev` on Debian and Ubuntu - and a C toolchain for the `zstd`
crate:

```
cargo build --release
```

That leaves the binary at `target/release/compsize-tree`; install it wherever you
keep binaries, or run it from there.

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
| **compsize** | Exactly what `compsize <path>` prints as Disk Usage, inline payloads aside (see Limits): every extent referenced anywhere below, counted once. The cross-check number (it is order-independent even though compsize itself is not). |
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
selector, badges for snapshot, mount and shared entries, and a status line
summarising the scan. Files: `Open…` loads a saved scan, `Save scan…` writes the
current one (zstd-compressed), both through native file dialogs.

### `scan` (needs root)

```
sudo compsize-tree scan [options] PATH...
```

One pass, then per-directory metrics. The scan itself never needs a cache: the tree
lives in memory for the report, and a saved scan is there so browsing later does not
mean re-walking 37M files.

The scan reads the filesystem's own trees instead of the files - `INODE_ITEM`,
`DIR_INDEX` and `EXTENT_DATA` out of each subvolume's fs tree, subvolume nesting out
of the root tree - which needs no `statx` and no per-file ioctl and is about 2.5x
faster than the file walk for the same numbers. Scanning a subdirectory costs a
full sweep of its containing subvolume.

**Mounts under the scanned path are read.** A subvolume mounted inside the scan -
`/home` when scanning `/` - is nowhere in any fs tree: the containing tree has an
ordinary directory where the mount is. The scan reads `/proc/self/mountinfo` to
find them (one open, one stat and one ioctl per mount point, never per file) and
reads each mounted subvolume the way it reads a nested one. A mount of a
filesystem that is not this one is never entered - extent addresses are only
unique within one filesystem - and neither is a mount of a plain directory; both
show as a mount point with nothing under it.

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

`--save FILE` writes the scan wherever you point it, and hands the file to the invoking
user, so a scan run under `sudo` leaves something you can open without it. Budget ~90
bytes per file: ~3.5 GB for a 37M file `/home`, or ~750 MB with `--compress`.

| option | |
|---|---|
| `-j, --threads N` | worker threads for the fs-tree walks (default: CPUs; the scan they produce is the same at any count) |
| `--exclude PATH` | skip a path prefix; extents shared with it will look exclusive to what remains. Excluding a whole subvolume also skips *reading* it, which inside a snapshot is the difference between a sweep and nothing |
| `--save FILE` | also write the scan to FILE, so the app can open it later |
| `--compress` | compress the saved scan (3-5x smaller) |
| `--depth N`, `--top N`, `--files N` | how much of the text report to print |
| `-b, --bytes` | raw byte counts instead of human units |
| `--full-scan` | walk snapshot-class subvolumes too (default: one node per snapshot, aggregates only) |
| `--progress` | live file/dir counters |
| `--dump-subvols` | print the subvolume inventory with snapshot classification |

### `report` (the compsize-compatible verifier)

```
sudo compsize-tree report [-b] PATH...
```

Reproduces `compsize`'s table byte for byte from its own walk, at about compsize's
speed (`--buffer` trades memory for a little more; the output is unchanged at any
size).

It is a second implementation of the extent layer, which is what makes it a check:
the item decoding the scanners share is compared here against an independent
implementation of it.

## Verifying a change

The file walker is the reference implementation: what the tree reader produces has
to match it byte for byte. It no longer ships in this tree - the
`deprecated-files-backend` branch keeps it, and the gate is a build of that branch:

```
git worktree add ../cst-ref deprecated-files-backend
cargo build --release --manifest-path ../cst-ref/Cargo.toml
```

Scan the same tree with both and compare the caches:

```
sudo compsize-tree scan --full-scan --save /tmp/a.cst --reproducible PATH
sudo ../cst-ref/target/release/compsize-tree scan --save /tmp/b.cst --reproducible PATH
cmp /tmp/a.cst /tmp/b.cst
```

Run it on two trees. One is the small fixture - `scripts/make-gate-fixture.sh`
builds it as `.scratch/own` and says what shapes it puts in it; it must not be
modified afterwards, since it is the input to a byte comparison. The other is any
tree with a large image file in it (tens of gigabytes, hundreds of thousands of
extents), which is what exercises compression and files long enough to page the
kernel search.

`--reproducible` drops timings and warnings, which is what makes two scans of an
unchanged tree byte-identical. When the tree has snapshot-class subvolumes the
tree reader needs `--full-scan` to match the reference, which always walks them;
a collapsed scan is a deliberately different, aggregate tree, and is compared on
its numbers instead (see Limits).

`sync` a fixture before comparing it: the tree reader reads the committed tree, so
a tree just written to can be missing entries the path walk still sees.
Name the scan root the same way in both runs: its spelling is part of the compared
bytes, since the path is recorded in `scanned_roots` and in the subvolume it opens.
An absolute root and a relative one differ by 144 bytes on the `.scratch/own` fixture
- enough to fail `cmp` on its own.

Nothing on the command line that changes *how* the scan is read changes what it
writes: `-j N` walks the same subvolumes and merges them in traversal order, so the
cache is byte-identical at any thread count (`-j 1` is the plain serial pass).

With `--exclude`, the two implementations agree on every number the report shows and
on the tree they build, but not byte for byte: the tree reader reads a subvolume's
items in one sweep, so an excluded directory inside one has already been read - its
nodes and references are dropped, and the extents it alone held come out unowned, but
its internal extent count still says the scan saw them. Excluding a whole subvolume is
exact, because it is skipped before anything is read.

`compsize-tree diff A.cst B.cst [--top N]` prints where two scans disagree, by path,
largest first. `extent-probe PATH` reports what one pass over the extent tree sees -
data extents, references, metadata extents, and how many items' inline references
failed to parse (the count has to be zero).

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
  two modes agree - on a real `/home`, `owned` came within 140 KB of the full
  scan at 597 GiB. Use `--full-scan` when the interiors of the collapsed
  subvolumes matter.
* The ownership rule treats a *snapshot-class* subvolume as one voice: its
  references carry no file birth time (unknown, with the subvolume's creation
  time as the tie-break), so a snapshot owns what the live trees have let go of
  and nothing they still hold, and among snapshots the oldest is the custodian.
  Its files' birth times are deliberately ignored there: a reflink copy and a
  rename are enough to hand a live file a younger inode, which would otherwise
  move ownership of data that never went anywhere to a snapshot.
* A few files can be missed: they live in a subvolume the root-tree backref walk
  does not reach (4 of a 38M-file `/home`, 78 MB, which shows up as that much in
  one subtree's `owned`).
* A collapsed snapshot's `uncompressed` is its compressed size for the extents
  only it holds - the extent tree has no uncompressed size. Its `referenced` is
  0 for the same reason: the logical sizes live in the files, which were not
  read.
* A scan is a snapshot in time; files changing during it are counted once, as
  seen - and it reads the committed tree, so a file written moments ago may not
  be in it until `sync`.
* An extent's address is carried as a 4 KiB page number in a `u32`, which is
  what packs a reference into one word - and caps the logical address it can
  name at 16 TiB. An extent above that is skipped, with a warning. (On an
  898 GiB filesystem the highest chunk ended at 2.30 TiB - addresses grow with
  allocation, not with what is held - so the cliff is several times away; a much
  larger filesystem would need the key widened.)
* A mount the scan does not enter - a non-btrfs filesystem (loop mounts,
  overlayfs, tmpfs, `/boot`), a *different* btrfs filesystem (its fsid is
  compared with the scanned one by `BTRFS_IOC_FS_INFO` before anything is read
  from it, since extent addresses are only unique within one filesystem), or a
  mount of a plain directory - is shown as a mount point with nothing under it.
  Scanning such a filesystem works by starting the scan there.
* A subvolume mounted under the scanned path is read at its mount, once. Of
  several mounts of one subvolume the *oldest* is the one read - mount IDs are
  handed out in mount order - and the others show as mount points with nothing
  under them, so its files do not gain a second reference. A subvolume that also
  lives inside the scanned tree keeps its tree location, and its mounts are only
  aliases. A mount inside a subvolume that was not walked (a collapsed snapshot)
  is read like a subvolume found inside one: it hangs off the collapsed node by
  the path the kernel gives for the directories between them.
* Several paths may be given to one scan; a path inside another one is not
  scanned separately, because it is already in that scan and a second placement
  would split every extent between the two copies. The scan says so when it
  drops one. A path under a mount the scan does not enter is *not* covered by
  the containing root, and keeps its own scan.
* Symlinks are skipped, like compsize; hardlinked paths are counted per path.
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
  granularity, deleting that snapshot frees *nothing*. The unreferenced
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
`ROOT_ITEM` records for the UUID inventory - so no file is `statx`ed or read on
its behalf; only subvolume roots are opened, one per subvolume walked, and
`INO_LOOKUP` names the directories inside a collapsed one. Subvolume identity comes from
`BTRFS_IOC_GET_SUBVOL_INFO`, so snapshots are recognised structurally rather
than by path convention. Compression, inline extents, prealloc and holes are all
accounted the way compsize accounts them.

`report` keeps the file walk: it is the compsize-compatible verifier, so it
works one path at a time, asks the kernel for that inode's `EXTENT_DATA` items
and reads the packed `btrfs_file_extent_item` layout directly, exactly like
compsize.

`CST_TIME=1` makes a scan print a phase-by-phase timing breakdown on stderr, the
quickest way to see where its seconds go; the other `CST_*` variables in the
source are development aids for poking at one scan in detail.
