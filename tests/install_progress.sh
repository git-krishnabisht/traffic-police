#!/bin/sh
# install.sh's count of the bytes downloaded so far (got_bytes), which its progress bar calls ten
# times a second in a terminal, as the bar calls it: got=$(got_bytes), under set -eu.
#
#   sh tests/install_progress.sh [INSTALL_SH] [CALLS]
#
# First CALLS calls (1,500 by default) while another process keeps writing the eight parts and
# removing them, as the end of the download did until 2026-10-13: a part removed between the count
# finding it and reading its size stopped the install ("n + : syntax error"). That got_bytes (git
# show 11bb9ac:install.sh) stops here within 82 calls, often the first few, in sh, dash, bash and
# ksh. Then what it counts: nothing yet, the parts, the file once it is there. CI (install.yml)
# runs this in sh, dash and bash, on Linux and on macOS, where sh is bash.

set -eu

src=${1:-$(dirname "$0")/../install.sh}
calls=${2:-1500}

say() { printf '%s\n' "$*"; }
fail() {
    say "install_progress: $*" >&2
    exit 1
}

[ -f "$src" ] || fail "no $src"
# got_bytes as install.sh has it; it reads $tmp and $asset
got_bytes=$(sed -n '/^got_bytes() {/,/^}/p' "$src")
[ -n "$got_bytes" ] || fail "$src has no got_bytes"
eval "$got_bytes"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/install-progress.XXXXXX")
asset=traffic-police-test
churn=""
i=0
stop_churn() {
    if [ -n "$churn" ]; then
        kill "$churn" 2>/dev/null || true
        wait "$churn" 2>/dev/null || true
        churn=""
    fi
}
# a count that stops the script (set -e ends it in the loop below) says how far it got
on_exit() {
    status=$?
    if [ "$status" != 0 ] && [ -n "$churn" ]; then
        say "install_progress: got_bytes stopped the script after $i of $calls calls" >&2
    fi
    stop_churn
    rm -rf "$tmp"
    exit "$status"
}
trap on_exit EXIT

# the parts come and go
(
    while :; do
        for k in 0 1 2 3 4 5 6 7; do printf 'xxxxxxxxxx' >"$tmp/$asset.part$k"; done
        rm -f "$tmp/$asset".part*
    done
) &
churn=$!
while [ "$i" -lt "$calls" ]; do
    got=$(got_bytes)
    case "$got" in
        "" | *[!0-9]*) fail "call $((i + 1)): got_bytes says \"$got\", not a number" ;;
    esac
    [ "$got" -le 80 ] || fail "call $((i + 1)): got_bytes says $got of eight parts of 10 bytes"
    i=$((i + 1))
done
stop_churn
say "install_progress: $calls counts while the parts came and went, each a number"

# what it counts (another name: a part removed late by the process above cannot be one of these)
asset=traffic-police-counts
counts() {
    got=$(got_bytes)
    [ "$got" = "$1" ] || fail "$2: got_bytes says $got, not $1"
}
counts 0 "before anything came"
printf 'aaa' >"$tmp/$asset.part0"
printf 'bbbbb' >"$tmp/$asset.part1"
counts 8 "two parts of 3 and 5 bytes"
printf 'abcdefghij' >"$tmp/$asset"
counts 10 "the file of 10 bytes, its parts still there"
say "install_progress: what it counts is right"
