#!/bin/sh
# Ephemeral venvpruner runner: fetch a release binary into a private temp dir,
# run it, then delete it. Nothing is installed and nothing is left behind.
#
#   curl -fsSL https://raw.githubusercontent.com/Party4Bread/venvpruner/main/run.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/Party4Bread/venvpruner/main/run.sh | sh -s -- --no-tui ~
#
# Environment:
#   VENVPRUNER_REPO     owner/repo to fetch from   (default Party4Bread/venvpruner)
#   VENVPRUNER_VERSION  release tag, e.g. v0.1.0   (default latest)
#   VENVPRUNER_KEEP     set to 1 to keep the binary and print its path
#   VENVPRUNER_BASE_URL override the asset base URL (mirrors, GH Enterprise)

set -eu

REPO="${VENVPRUNER_REPO:-Party4Bread/venvpruner}"
VERSION="${VENVPRUNER_VERSION:-latest}"
KEEP="${VENVPRUNER_KEEP:-0}"

die() {
    printf 'venvpruner: %s\n' "$*" >&2
    exit 1
}

note() {
    printf 'venvpruner: %s\n' "$*" >&2
}

WORKDIR=""
cleanup() {
    if [ "$KEEP" = 1 ]; then
        return 0
    fi
    if [ -n "$WORKDIR" ] && [ -d "$WORKDIR" ]; then
        rm -rf "$WORKDIR"
    fi
}

# EXIT covers the normal path and `set -e` aborts; the signal traps make sure a
# Ctrl-C part-way through a scan still takes the binary with it.
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
trap 'cleanup; exit 129' HUP

# --- platform -----------------------------------------------------------------

os=$(uname -s)
[ "$os" = Linux ] || die "only Linux is supported (detected $os); build from source instead"

arch=$(uname -m)
case "$arch" in
    x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
    aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
    *) die "no prebuilt binary for $arch; build from source with 'cargo build --release'" ;;
esac

asset="venvpruner-$target"

# --- download -----------------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" -- "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q --https-only -O "$2" -- "$1"; }
else
    die "need curl or wget to download the release"
fi

if [ -n "${VENVPRUNER_BASE_URL:-}" ]; then
    base="$VENVPRUNER_BASE_URL"
elif [ "$VERSION" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$VERSION"
fi

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/venvpruner.XXXXXXXX") || die "could not create a temp dir"
chmod 700 "$WORKDIR"
bin="$WORKDIR/venvpruner"
sums="$WORKDIR/SHA256SUMS"

note "fetching $asset ($VERSION) from $REPO"
fetch "$base/$asset" "$bin" ||
    die "download failed: $base/$asset
       (no release for this platform yet? check https://github.com/$REPO/releases)"

# --- verify -------------------------------------------------------------------

# The checksums ship from the same release, so this catches a truncated or
# corrupted download -- not a compromised release. Pin VENVPRUNER_VERSION and
# check the tag yourself if you need more than that.
if fetch "$base/SHA256SUMS" "$sums" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$bin" | cut -d' ' -f1)
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$bin" | cut -d' ' -f1)
    else
        actual=""
        note "no sha256sum or shasum available; skipping checksum verification"
    fi

    if [ -n "$actual" ]; then
        expected=$(awk -v f="$asset" '{ sub(/^\*/, "", $2); if ($2 == f) print $1 }' "$sums")
        [ -n "$expected" ] || die "$asset is not listed in SHA256SUMS"
        [ "$actual" = "$expected" ] || die "checksum mismatch for $asset
       expected $expected
       got      $actual"
    fi
else
    note "release has no SHA256SUMS; skipping checksum verification"
fi

chmod 700 "$bin"

# --- terminal -----------------------------------------------------------------

# Under `curl ... | sh` the shell is still reading this script from stdin, so we
# must not touch fd 0 for the shell itself -- only for the binary. Redirecting
# with `exec` here would eat the rest of the script.
# Headless invocations never open a terminal, so don't demand one -- that would
# break `curl ... | sh -s -- --json ~ > out.json` in CI for no reason.
needs_tty=1
for a in "$@"; do
    case "$a" in
        --no-tui | --json | --dry-run | --help | -h | --version | -V) needs_tty=0 ;;
    esac
done

tty_in=""
if [ "$needs_tty" = 1 ] && [ ! -t 0 ]; then
    # The probe runs in a subshell on purpose: a redirection failure on a POSIX
    # special builtin (`:`) exits the shell outright, even inside an `if`, so
    # probing directly would kill this script instead of reporting the problem.
    if [ -r /dev/tty ] && (exec </dev/tty) 2>/dev/null; then
        tty_in=/dev/tty
    else
        die "no terminal available on stdin and /dev/tty is not readable.
       The TUI needs one. Download run.sh and run it directly, or use the
       binary's headless flags from a real terminal (--no-tui, --json, --dry-run)."
    fi
fi

# --- run ----------------------------------------------------------------------

set +e
if [ -n "$tty_in" ]; then
    "$bin" "$@" <"$tty_in"
else
    "$bin" "$@"
fi
status=$?
set -e

if [ "$KEEP" = 1 ]; then
    note "kept binary at $bin"
fi

exit "$status"
