# compsize-tree

Extent-accurate disk usage for btrfs: **one pass** over the tree gives every
directory's true size, and a local web UI shows it the way baobab would - if
baobab understood reflinks, snapshots and compression.

```
sudo compsize-tree scan --save /home   # one pass, ~5 min for 37M files
compsize-tree serve                    # browse it (no root needed, opens a browser)
```

`scan` alone just measures and prints - nothing is written to disk. `--save` keeps the
result as a cache so a later `serve` does not have to re-walk the tree (the UI shows how
much the cache occupies and can delete it again).

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
| **owned** | Semantic ownership. An extent is owned by its live references if it has any, otherwise by its snapshot references; within a class the bytes are split evenly (2 references -> 50/50). Additive: children sum to the parent and the whole tree sums to the true physical usage. A snapshot at creation owns nothing and grows only as the live tree overwrites data. This is the default. |
| **freeable** | Bytes that deleting this path would actually free: extents whose references are *all* below it. Same idea as `btrfs fi du`'s "Exclusive", but per subtree and compression-aware. |
| **compsize** | Exactly what `compsize <path>` prints as Disk Usage: every extent referenced anywhere below, counted once. The cross-check number (it is order-independent even though compsize itself is not). |
| **referenced** | What the files claim (du/baobab's number, the 27 TB). |
| **uncompressed** | Deduplicated data before compression; `compsize / uncompressed` is the effective compression ratio. |

A fresh snapper snapshot of `/home` therefore shows a few hundred MB owned (its
CoW cost so far) instead of "another 605 GB", and `/home/marko` keeps its
compsize number even though 190 snapshots reference the same extents.

## Commands

### `scan` (needs root - the extent ioctl requires CAP_SYS_ADMIN)

```
sudo compsize-tree scan [options] PATH...
```

One pass, then per-directory metrics. The scan itself never needs a cache: the tree
lives in memory for the report, and `serve` reads it back from a saved cache only so
that browsing later does not mean re-walking 37M files.

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
| `--cache PATH` | where to write the cache (also how you keep it off a full disk) |
| `--compress` | write the cache through `zstd` (3-4x smaller; `serve` streams it back) |
| `--buffer BYTES` | kernel search buffer per thread (default 1 MiB; larger is faster on files with millions of extents) |
| `--depth N`, `--top N`, `--files N` | how much of the text report to print |
| `-b, --bytes` | raw byte counts instead of human units |
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

## Limits

* Sizes are relative to the scanned roots: an extent shared with a subvolume
  that was not walked is credited here. `scan` warns when it can see snapshots
  of a scanned subvolume outside the scanned paths. Scanning `/` (the default)
  avoids this entirely.
* A scan is a snapshot in time; files changing during it are counted once, as
  seen.
* Non-btrfs filesystems inside the tree (loop mounts, overlayfs, `/boot`) are
  detected and skipped, and counted in the summary.
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

No dependencies (std only). Per file it asks the kernel for the inode's
`EXTENT_DATA` items with `BTRFS_IOC_TREE_SEARCH_V2`, exactly like compsize,
and reads the packed `btrfs_file_extent_item` layout directly. Subvolume
identity comes from `BTRFS_IOC_GET_SUBVOL_INFO` at `st_dev` boundaries plus a
root-tree scan for the UUID inventory, so snapshots are recognised
structurally rather than by path convention. Compression, inline extents,
prealloc and holes are all accounted the way compsize accounts them.
