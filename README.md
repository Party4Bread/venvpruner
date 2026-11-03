# venvpruner

A terminal tool for finding and removing the directories that quietly eat a
developer's disk: Python virtualenvs, uv tools, pipx venvs, conda envs,
`node_modules`, cargo `target/` directories, `__pycache__` trees and package
manager caches.

It scans, sizes everything hardlink-aware, judges how safe each candidate is to
delete, and lets you mark and remove them from an interactive list — or prints
the results and exits so you can pipe them into something else.

## Run without installing

`run.sh` fetches the release binary for your platform into a private temp
directory, runs it, and deletes it on the way out — including on Ctrl-C, on
error, and on a failed checksum. Nothing is installed and nothing is left
behind.

```sh
curl -fsSL https://raw.githubusercontent.com/Party4Bread/venvpruner/main/run.sh | sh

# arguments go after `-s --`
curl -fsSL https://raw.githubusercontent.com/Party4Bread/venvpruner/main/run.sh | sh -s -- --no-tui ~
```

The downloaded binary is checked against the `SHA256SUMS` published with the
release. That catches a truncated or corrupted download; it is not a signature,
since the checksums ship from the same release as the binary. Pin
`VENVPRUNER_VERSION` and inspect the tag yourself if you need more than that.

| Variable | Default | Meaning |
| --- | --- | --- |
| `VENVPRUNER_REPO` | `Party4Bread/venvpruner` | Repository to fetch from |
| `VENVPRUNER_VERSION` | `latest` | Release tag, e.g. `v0.1.0` |
| `VENVPRUNER_KEEP` | `0` | Set to `1` to keep the binary and print its path |
| `VENVPRUNER_BASE_URL` | — | Override the asset base URL (mirrors, GH Enterprise) |

Under `curl … | sh` the shell's stdin is the pipe, not your terminal, so the
script hands the binary `/dev/tty` instead. Headless invocations (`--no-tui`,
`--json`, `--dry-run`, `--help`, `--version`) skip that requirement and stay
pipeable in CI.

Prebuilt binaries cover `x86_64` and `aarch64` on Linux (static musl builds)
and macOS. On Apple silicon the native binary is picked even from a shell
running under Rosetta. On any other platform, build from source.

The macOS binaries are ad-hoc signed, not notarized. `curl` does not mark what
it downloads as quarantined, so Gatekeeper does not stop them under `run.sh`; a
copy fetched with a browser needs `xattr -d com.apple.quarantine` first.

## Install

Requires a recent Rust toolchain (edition 2024) and Linux or macOS.

```sh
git clone https://github.com/Party4Bread/venvpruner
cd venvpruner
cargo build --release
install -m755 target/release/venvpruner ~/.local/bin/
```

Or install straight from a checkout:

```sh
cargo install --path .
```

## Usage

```sh
venvpruner                    # interactive TUI over $HOME
venvpruner ~/code ~/work      # scan specific roots
```

Nothing is deleted unless you mark it and confirm. Deletion is guarded: a path
must lie under one of the scanned roots and must still match the category it was
discovered as, or the delete is refused.

### CLI examples

```sh
# Everything at least 100 MiB, as a plain table
venvpruner --no-tui --min-size 100M

# What a cleanup would remove, without touching anything
venvpruner --dry-run --older-than 6mo

# Only Python environments, machine-readable
venvpruner --json --category venv,uv-tool,pipx > artifacts.json

# Total reclaimable bytes for stale node_modules
venvpruner --json --category node --older-than 90d \
  | jq '[.artifacts[].sizes.reclaim] | add'

# Cross filesystem boundaries (off by default) and treat 30 days as stale
venvpruner --all-filesystems --stale-after 30d

# Keep the scan out of the way of a build: two threads per stage instead of all of them
venvpruner -j2
```

### Options

| Flag | Meaning |
| --- | --- |
| `[ROOT]...` | Directories to scan. Defaults to `$HOME`. |
| `--min-size <SIZE>` | Ignore anything smaller. `500K`, `100M`, `1.5G`, or plain bytes. Binary units. |
| `--older-than <DUR>` | Only artifacts untouched for at least this long. |
| `--category <LIST>` | Comma-separated: `venv`, `uv-tool`, `pipx`, `conda`, `node`, `cargo`, `pycache`, `cache`. |
| `--stale-after <DUR>` | How long without activity counts as stale for badging. Default `90d`. |
| `--json` | JSON to stdout. Implies `--no-tui`. |
| `--no-tui` | Scan, print a table, exit. |
| `--dry-run` | Print what a cleanup would remove, delete nothing, exit. |
| `--all-filesystems` | Follow the walk across mount points. Off by default. |
| `--jobs`, `-j <N>` | Worker threads for the walk and for sizing. Defaults to the machine's parallelism (capped at 32); `-j1` scans serially. |

Durations accept `s`, `min`, `h`, `d`, `w`, `mo` (30 days) and `y` (365 days).
A bare `m` is rejected on purpose — it reads as both minutes and months, so
write `min` or `mo` and mean it.

In headless modes the progress line goes to stderr and the results go to
stdout, so piping stays clean.

## Badges

Every artifact gets a verdict from the evidence around it — the artifact's own
mtime, recent activity in the surrounding project, `.git/refs/heads` mtimes,
whether a lockfile or manifest exists to rebuild from, and whether the venv's
base interpreter still exists.

| Badge | Meaning |
| --- | --- |
| `DEAD` | The recorded base interpreter is gone. The environment cannot be activated at all. |
| `SAFE` | Stale, reinstallable from a lockfile or manifest, no recent project activity. |
| `REVIEW` | Not clearly safe. Look before you delete. |
| `ACTIVE` | The surrounding project was touched recently. Leave it alone. |

`--dry-run` lists only `DEAD` and `SAFE` artifacts.

## Key bindings

| Key | Action |
| --- | --- |
| `↑` `↓` / `k` `j` | Move the selection |
| `PgUp` `PgDn` | Page up / down |
| `g` `G` / `Home` `End` | Jump to top / bottom |
| `Space` | Mark or unmark the highlighted artifact |
| `a` | Mark everything currently listed |
| `n` | Clear every mark |
| `/` | Filter by path (fuzzy subsequence). `Enter` accepts, `Esc` clears |
| `s` | Cycle the sort key (reclaim, size, age, path, category, badge) |
| `r` | Reverse the sort order |
| `1`–`8` | Toggle a category on or off |
| `d` | Delete marked artifacts, after typing `DELETE` to confirm |
| `?` | Help overlay |
| `q` / `Esc` | Quit |

Details for the highlighted artifact are always shown in the side pane; there is
no key to open them. The footer lists the live bindings and `?` is the
authoritative reference.

## Notes

- Linux and macOS. Sizing uses `st_dev`/`st_ino`/`st_nlink`/`st_blocks`.
- On macOS the walk skips `/System`, which holds the sealed system volume and,
  under `/System/Volumes/Data`, a second view of every file on the data volume.
  Scanning folders such as `~/Documents` or `~/Desktop` triggers the usual
  privacy prompt for your terminal; whatever it is refused shows up as a scan
  error rather than as a missing artifact.
- Symlinks are never followed, anywhere — not during the walk, not during
  sizing, not during deletion.
- The walk stays on one filesystem by default, so network mounts and external
  drives do not silently join the scan.
- The scan is parallel by default: `--jobs` threads walk directories while
  `--jobs` more measure the artifacts they find. Both stages are IO-bound, so
  the useful number tracks the disk more than the CPU — lower it with `-j` on a
  spinning disk or a network mount.
- pnpm's `.pnpm` virtual store counts as a `node_modules`: one row, not one per
  installed package. Under `$PNPM_HOME/global` it is reported with no owning
  project, like a uv tool or a pipx venv — those are the CLI tools on your
  `$PATH`, so they are shown but never badged `SAFE` and never listed by
  `--dry-run`.
- Caches are reported one row per cache directory, never as their contents.
  Anything found under `$XDG_CACHE_HOME` (default `~/.cache`) collapses to the
  top-level directory that owns it: `~/.cache/pnpm/dlx/<hash>/pkg/node_modules`
  is one `pnpm dlx` run out of dozens and `~/.cache/node/corepack/…/node_modules`
  one downloaded pnpm out of a dozen more — the directory worth acting on is
  `~/.cache/pnpm` or `~/.cache/node`. `XDG_CACHE_HOME` is honoured when set to
  an absolute path. On macOS `~/Library/Caches` is a second cache home and
  collapses the same way, since pip, poetry, pdm and pnpm keep their caches
  there while uv and huggingface stay in `~/.cache`. Naming a root inside a
  cache (`venvpruner ~/.cache/node/corepack`) still lists what is in there.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs exactly these. Tests build real directory trees, hardlinks and symlinks
under `$TMPDIR` rather than mocking the filesystem — the bugs worth catching
here are about inodes and link counts, which mocks do not model.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the invariants that keep deletion
safe. Read it before changing anything in `src/scan/size.rs` or
`src/delete.rs`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
