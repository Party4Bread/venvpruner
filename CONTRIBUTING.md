# Contributing

Bug reports, fixes and new detectors are all welcome. This document covers how
to build and, more importantly, the invariants that make the tool safe to run.

## Build and check

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

All three must be clean before a PR. CI runs the same commands.

Suppressing a warning with `#[allow(...)]` or skipping a test with `#[ignore]`
to make a check pass is not acceptable. If a lint is a genuine false positive,
add the attribute *with a one-line comment saying why*. If a test fails, the
implementation is what needs fixing.

## Invariants

This program calls `remove_dir_all` on directories in a user's home. Several
things are load-bearing, and none of them are obvious from reading a diff in
isolation.

### Sizing must stay hardlink-aware

`reclaim` is the number a user makes a deletion decision on. It must mean
"bytes freed by deleting *this artifact alone*", which requires knowing whether
every hardlink of an inode lives inside the artifact.

- Key inodes by `(st_dev, st_ino)`, never `st_ino` alone. btrfs subvolumes
  report different `st_dev` values on one mount, so a bare inode number is
  ambiguous on exactly the filesystems this tool is most useful on.
- Use `st_blocks * 512`, not `st_size`. Compression, sparse files and tail
  packing all make `st_size` the wrong answer.
- `apparent == reclaim + shared` must always hold.
- The session-wide inode set is what stops "delete these five venvs to reclaim
  40G" when they share 20G with a package cache. Do not remove it for speed.

`src/scan/size.rs` has tests that construct hardlinks across directories and
assert the split directly. Extend those rather than replacing them.

### Deletion is guarded, and the guard runs every time

`delete::check_guard` refuses paths that are relative, contain `..`, have fewer
than three components, are `$HOME` or a bare child of it, are an ancestor of the
current directory, are a symlink, fall outside every scan root, or no longer
match the category they were discovered as.

Two things repeatedly go wrong here:

- **Path containment must be checked on canonical paths, component-wise.** A
  string `starts_with` lets `/home/p4bevil` pass a `/home/p4b` root.
- **`Category::Pycache` stores the *project* directory in `artifact.path`**, and
  its `children` are the `__pycache__` directories. Only the children are ever
  removed. The match in `delete()` is exhaustive on purpose — adding a
  `Category` should be a compile error, not a silent fall-through into
  `remove_dir_all` on somebody's project.

### The walker prunes on match

Once a directory is identified as an artifact, the walk does not descend into
it. This is both the main performance lever (it avoids reading the interior of
an 11 GB venv) and a correctness property: it is what prevents nested
`node_modules` being reported as separate artifacts inside their own parent.

A detector that returns `None` for a directory means the walk *enters* it. That
is what caused a real bug: requiring a sibling `package.json` made global npm
roots fail to match, so the walk descended and reported 108 nested fragments of
one 1.8 GB tree. If you add a detector, decide deliberately what happens to the
subtree.

### The terminal must always be restored

`TerminalGuard` restores raw mode and the alternate screen on every exit path,
including unwind, and a panic hook restores before the default hook prints.
Anything that can panic while the terminal is in raw mode leaves the user with
an unusable shell. Do not add an early `return` that bypasses the guard.

## Adding a detector

Detectors live in `src/scan/detect.rs`. A good one keys on something that is
hard to produce by accident:

- `pyvenv.cfg` for virtualenvs
- `conda-meta/history` for conda environments
- a `CACHEDIR.TAG` whose first bytes carry the standard signature, for cargo
  `target/` directories — a directory merely *named* `target` is not enough

Match on path *components*, not substrings, when location matters. Add tests
for both the positive case and the near-miss that must not match.

## Tests

Filesystem behaviour is tested against real trees built in a `tempfile`
directory, not mocks. That is deliberate: the bugs worth catching here are
about inodes, links and permissions, which mocks do not model.

When you fix a bug, add the test first and confirm it fails against the
unfixed code. Several tests in this repo exist because that step caught a
"fix" that did not actually fix anything.

## Scope

The tool runs on Linux and macOS and uses `std::os::unix::fs::MetadataExt`
throughout. CI runs the checks on both.
Portability patches are welcome but must not weaken the sizing model —
if `st_blocks` or `st_nlink` are unavailable on a target, that target needs a
different accounting story, not a silent fallback to `st_size`.
