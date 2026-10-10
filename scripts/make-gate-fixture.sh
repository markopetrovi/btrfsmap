#!/bin/sh
# Build the gate fixture: the small btrfs tree that "Verifying a change" scans
# with both implementations. It is created as .scratch/own in the current
# directory (run it from the checkout), and it must not be modified afterwards -
# it is the input to a byte comparison, and a fixture that changes would hide
# the differences the gate is looking for.
#
# The shapes are the point: a file small enough to live inline in its inode, a
# hardlinked pair, a symlink, files with real extents, a directory nested two
# deep, and a snapshot taken midway through - so it holds an older, smaller set
# of the same files - which is what puts the snapshot and ownership paths under
# the comparison rather than plain files alone.
#
# Needs btrfs-progs and root (for the subvolume commands), on a btrfs directory.
set -eu

as_root() {
    if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi
}

command -v btrfs >/dev/null || { echo "btrfs-progs is needed" >&2; exit 1; }
if [ "$(stat -f -c %T .)" != btrfs ]; then
    echo "the current directory is not on btrfs" >&2
    exit 1
fi
if [ -e .scratch/own ]; then
    echo ".scratch/own already exists - refusing to touch it" >&2
    exit 1
fi

mkdir -p .scratch
as_root btrfs subvolume create .scratch/own
as_root chown "$(id -u):$(id -g)" .scratch/own

cd .scratch/own
printf 'hello\n' > a.txt
ln -s a.txt link
head -c 300000 /dev/urandom > b.bin
mkdir -p sub/deeper
printf '01234\n' > sub/c.txt
head -c 120000 /dev/urandom > sub/deeper/d.bin
sync
as_root btrfs subvolume snapshot . snap >/dev/null

# After the snapshot, so the snapshot keeps the older, smaller set: a second
# link to a.txt, one more file, and a longer c.txt.
ln a.txt hardlink
head -c 90000 /dev/urandom > sub/e.bin
printf '0123456\n' > sub/c.txt
sync

echo "created $(pwd) - the gate scans this with both implementations and"
echo "compares the caches byte for byte; do not modify it."
