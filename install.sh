#!/bin/sh
# Installs traffic-police, the terminal network inspector for Android apps, on macOS and Linux:
#
#   curl -fsSL https://raw.githubusercontent.com/git-krishnabisht/traffic-police/master/install.sh | sh
#
# It downloads the binary for this machine from a GitHub release (the latest unless --version
# names one), checks it against the release's SHA256SUMS.txt, puts it in ~/.local/bin (no sudo)
# and, when that folder is not on PATH yet, adds it to your shell's startup file. Run it again to
# update. adb is not installed: traffic-police uses the one you have (Android Studio's SDK or
# PATH; a second adb of another version would fight Android Studio's over the adb server), and
# the script says where it found it or how to get it.
#
#   sh install.sh [--version v0.4.0] [--dir DIR] [--no-modify-path]
#   curl -fsSL .../install.sh | sh -s -- --version v0.4.0
#
# On Windows, install.ps1 does the same in PowerShell.

set -eu

REPO="git-krishnabisht/traffic-police"
SOURCE="https://github.com/$REPO#install"

version="${TRAFFIC_POLICE_VERSION:-latest}"
dir="${TRAFFIC_POLICE_INSTALL_DIR:-$HOME/.local/bin}"
modify_path=1

# --- what the installer shows -------------------------------------------------------------------
# In a terminal: one line per step, a spinner while it runs and a mark when it is done, and a
# progress bar for the download. Elsewhere (CI, a log): the same lines, plain. NO_COLOR or
# TERM=dumb leave out the colors; a terminal without UTF-8 gets ASCII marks.

fancy=0
if [ -t 1 ] && [ "${TERM:-dumb}" != dumb ]; then fancy=1; fi
if [ "$fancy" = 1 ] && [ -z "${NO_COLOR:-}" ]; then
    esc=$(printf '\033')
    green="${esc}[32m" yellow="${esc}[33m" red="${esc}[31m" cyan="${esc}[36m" dim="${esc}[2m" bold="${esc}[1m"
    reset="${esc}[0m"
else
    esc="" green="" yellow="" red="" cyan="" dim="" bold="" reset=""
fi
case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
    *UTF-8* | *utf-8* | *UTF8* | *utf8*)
        mark_ok="✓" mark_warn="!" mark_bad="✗" bar_full="━" bar_empty="─"
        frames="⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏"
        ;;
    *)
        mark_ok="ok" mark_warn="!" mark_bad="x" bar_full="#" bar_empty="-"
        frames="| / - \\"
        ;;
esac

# a step's line: its mark, its name in a column, and what it found
line() {
    printf '  %s%s%s %s%-10s%s %s\n' "$2" "$1" "$reset" "$bold" "$3" "$reset" "$4"
}
ok() { line "$mark_ok" "$green" "$1" "$2"; }
warn() { line "$mark_warn" "$yellow" "$1" "$2"; }
note() { printf '               %s%s%s\n' "$dim" "$*" "$reset"; }

# the line being drawn over (a step still running) is cleared before anything else is written
redraw() {
    if [ "$fancy" = 1 ]; then printf '\r%s[K' "$esc"; fi
}
cursor() {
    if [ "$fancy" = 1 ] && [ -n "$esc" ]; then printf '%s[?25%s' "$esc" "$1"; fi
}

fail() {
    redraw
    cursor h
    printf '  %s%s%s %s\n' "$red" "$mark_bad" "$reset" "$*" >&2
    exit 1
}

# `spin NAME DETAIL COMMAND...`: runs the command, with a spinner beside its name meanwhile.
# Its output goes to a log, shown if it fails.
bg=""
spin() {
    name="$1" detail="$2"
    shift 2
    if [ "$fancy" = 0 ]; then
        "$@" >"$tmp/step.log" 2>&1
        return
    fi
    "$@" >"$tmp/step.log" 2>&1 &
    bg=$!
    cursor l
    while kill -0 "$bg" 2>/dev/null; do
        for f in $frames; do
            printf '\r  %s%s%s %s%-10s%s %s' "$cyan" "$f" "$reset" "$bold" "$name" "$reset" "$detail"
            sleep 0.1
            kill -0 "$bg" 2>/dev/null || break
        done
    done
    status=0
    wait "$bg" || status=$?
    bg=""
    redraw
    cursor h
    return "$status"
}

mb() { awk -v b="$1" 'BEGIN { printf "%.1f", b / 1048576 }'; }
rate() { awk -v b="$1" -v s="$2" 'BEGIN { if (s < 1) s = 1; r = b / s; if (r >= 1048576) printf "%.1f MB/s", r / 1048576; else printf "%.0f KB/s", r / 1024 }'; }
took() {
    if [ "$1" -ge 60 ]; then printf '%dm %02ds' $(($1 / 60)) $(($1 % 60)); else printf '%ds' "$1"; fi
}

say() { printf '%s\n' "$*"; }
# for showing paths under the home folder the short way
tilde="~"

usage() {
    cat <<EOF
Installs traffic-police from its GitHub releases (https://github.com/$REPO).

Usage: install.sh [--version vX.Y.Z] [--dir DIR] [--no-modify-path]

  --version vX.Y.Z   the release to install (default: the latest)
  --dir DIR          where the binary goes (default: ~/.local/bin)
  --no-modify-path   leave PATH alone (by default DIR is added to your shell's startup file
                     when it is not on PATH yet)

TRAFFIC_POLICE_VERSION and TRAFFIC_POLICE_INSTALL_DIR do the same as --version and --dir.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || fail "--version needs a release, like v0.4.0"
            version="$2"
            shift 2
            ;;
        --version=*)
            version="${1#*=}"
            shift
            ;;
        --dir)
            [ $# -ge 2 ] || fail "--dir needs a folder"
            dir="$2"
            shift 2
            ;;
        --dir=*)
            dir="${1#*=}"
            shift
            ;;
        --no-modify-path)
            modify_path=0
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) fail "unknown option $1 (see --help)" ;;
    esac
done

# the release: "latest", or a tag with or without its v
case "$version" in
    "" | latest) version=latest ;;
    v[0-9]*.[0-9]*.[0-9]*) ;;
    [0-9]*.[0-9]*.[0-9]*) version="v$version" ;;
    *) fail "--version takes a release like v0.4.0, not \"$version\"" ;;
esac
[ -n "$dir" ] || fail "--dir needs a folder"

# Linux builds need glibc 2.34 or newer (they are built on Ubuntu 22.04)
check_glibc() {
    found=$(getconf GNU_LIBC_VERSION 2>/dev/null | sed -n 's/^glibc \([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\1 \2/p')
    [ -n "$found" ] || fail "the Linux build needs glibc 2.34 or newer, and this system has no glibc (musl, as on Alpine?); build it from source: $SOURCE"
    major=${found% *}
    minor=${found#* }
    if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 34 ]; }; then
        fail "the Linux build needs glibc 2.34 or newer, and this system has $major.$minor; build it from source: $SOURCE"
    fi
}


os=$(uname -s)
arch=$(uname -m)
case "$os" in
    Darwin)
        # a shell under Rosetta says x86_64 on Apple silicon
        if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
            arch=arm64
        fi
        case "$arch" in
            arm64) asset=traffic-police-macos-arm64 system="macOS on Apple silicon" ;;
            *) fail "there is no build for Intel Macs yet; build it from source: $SOURCE" ;;
        esac
        ;;
    Linux)
        case "$arch" in
            x86_64 | amd64) asset=traffic-police-linux-x86_64 system="Linux on x86_64" ;;
            *) fail "there is no build for Linux on $arch yet; build it from source: $SOURCE" ;;
        esac
        check_glibc
        system="$system, glibc $major.$minor"
        ;;
    MINGW* | MSYS* | CYGWIN*)
        fail "on Windows, run this in PowerShell: irm https://raw.githubusercontent.com/$REPO/master/install.ps1 | iex"
        ;;
    *) fail "there is no build for $os; build it from source: $SOURCE" ;;
esac

if command -v curl >/dev/null 2>&1; then
    downloader=curl
elif command -v wget >/dev/null 2>&1; then
    downloader=wget
else
    fail "needs curl or wget to download"
fi

# A download that stalls (under 1 KB a second for two minutes) is given up, and tried again. An
# address that does not answer is left after 10 s for the next one: GitHub serves release files
# from four, and where one of them is unreachable curl otherwise waits for the system's own
# timeout (75 s on macOS) on every connection, so the install seemed to hang for minutes.
download() {
    if [ "$downloader" = curl ]; then
        curl --proto '=https' --tlsv1.2 -fsSL --connect-timeout 10 --retry 3 \
            --speed-limit 1024 --speed-time 120 -o "$2" "$1"
    else
        wget -q --https-only --connect-timeout=10 --timeout=120 --tries=3 -O "$2" "$1"
    fi
}

# A large file comes in parts, each over its own connection: at times GitHub's release servers
# give each connection from a network about 100 KB/s while the same network takes 20 MB/s from
# elsewhere, and then eight parts at once arrive about six times as fast (measured on 2026-10-11:
# 116 KB/s over one connection, 682 KB/s over eight; when one connection is fast, parts cost
# nothing). Each part's size is checked, and the checksum after checks the whole; when a part
# fails, or the server does not send parts, the file comes over one connection instead.
PARTS=8
download_parts() {
    url=$1 out=$2 size=$3
    chunk=$(((size + PARTS - 1) / PARTS))
    part_pids=""
    k=0
    while [ "$k" -lt "$PARTS" ]; do
        from=$((k * chunk))
        to=$((from + chunk - 1))
        if [ "$to" -ge "$size" ]; then to=$((size - 1)); fi
        curl --proto '=https' --tlsv1.2 -fsSL --connect-timeout 10 --retry 3 \
            --speed-limit 1024 --speed-time 120 -r "$from-$to" -o "$out.part$k" "$url" &
        part_pids="$part_pids $!"
        k=$((k + 1))
    done
    whole=1
    for p in $part_pids; do
        wait "$p" || whole=0
    done
    part_pids=""
    k=0
    while [ "$k" -lt "$PARTS" ]; do
        from=$((k * chunk))
        to=$((from + chunk - 1))
        if [ "$to" -ge "$size" ]; then to=$((size - 1)); fi
        have=0
        if [ -f "$out.part$k" ]; then have=$(wc -c <"$out.part$k" | tr -d ' '); fi
        if [ "$have" != $((to - from + 1)) ]; then whole=0; fi
        k=$((k + 1))
    done
    if [ "$whole" = 1 ]; then
        : >"$out.joined"
        k=0
        while [ "$k" -lt "$PARTS" ]; do
            cat "$out.part$k" >>"$out.joined"
            k=$((k + 1))
        done
        rm -f "$out".part*
        mv "$out.joined" "$out"
        : >"$out.in-parts"
        return 0
    fi
    rm -f "$out".part* "$out.joined"
    download "$url" "$out"
}

# the binary: in parts when the server sends them and the file is large enough to gain from it
fetch() {
    if [ "$in_parts" = 1 ]; then
        download_parts "$base/$asset" "$tmp/$asset" "$total"
    else
        download "$base/$asset" "$tmp/$asset"
    fi
}

# bytes of the binary so far: its parts, or the file
got_bytes() {
    n=0
    for f in "$tmp/$asset" "$tmp/$asset".part*; do
        if [ -f "$f" ]; then n=$((n + $(wc -c <"$f" | tr -d ' '))); fi
    done
    say "$n"
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "needs sha256sum or shasum to check the download"
    fi
}

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t traffic-police)
# on any exit: a step still running stops (a background job ignores Ctrl+C, and its curl is a
# child of it; without a terminal, the parts' curls are the script's own background jobs), the
# scratch folder goes, and the cursor comes back
part_pids=""
stop_bg() {
    if [ -n "$bg" ]; then
        pkill -P "$bg" 2>/dev/null || true
        kill "$bg" 2>/dev/null || true
        wait "$bg" 2>/dev/null || true
        bg=""
    fi
    if [ -n "$part_pids" ]; then
        # shellcheck disable=SC2086 # one word per process
        kill $part_pids 2>/dev/null || true
        part_pids=""
    fi
}
# (the folder first: after a closed terminal, writing the cursor's code fails)
trap 'stop_bg; rm -rf "$tmp"; cursor h 2>/dev/null || true' EXIT
trap 'stop_bg; redraw; printf "  stopped\n"; exit 130' INT
trap 'stop_bg; exit 1' HUP TERM

say ""
say "  ${bold}traffic-police${reset} installer"
say ""
ok System "$system"

# the release: "latest" is resolved to its tag first (GitHub's redirect names it), so every file
# comes from the same release; its answer also gives the binary's size for the progress bar
if [ "$version" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$version"
fi
total=""
in_parts=0
if [ "$downloader" = curl ]; then
    if ! spin Release "looking up ${version}" curl --proto '=https' --tlsv1.2 -sSIL --connect-timeout 10 \
        --retry 3 -o "$tmp/headers" "$base/$asset"; then
        fail "could not reach GitHub ($(tail -n 1 "$tmp/step.log"))"
    fi
    tr -d '\r' <"$tmp/headers" >"$tmp/headers.txt"
    final=$(awk 'tolower($1) ~ /^http/ { code = $2 } END { print code }' "$tmp/headers.txt")
    if [ "$final" != 200 ]; then
        fail "release $version of $REPO has no $asset (GitHub answered $final)"
    fi
    found=$(sed -n 's#^[Ll]ocation: .*/releases/download/\([^/]*\)/.*#\1#p' "$tmp/headers.txt" | head -n 1)
    total=$(awk 'tolower($1) == "content-length:" { n = $2 } END { print n }' "$tmp/headers.txt")
    ranges=$(awk 'tolower($1) == "accept-ranges:" { r = tolower($2) } END { print r }' "$tmp/headers.txt")
    if [ "$ranges" = bytes ] && [ -n "$total" ] && [ "$total" -ge 1048576 ]; then in_parts=1; fi
    if [ "$version" = latest ] && [ -n "$found" ]; then
        version="$found"
        ok Release "$version (the latest)"
    else
        ok Release "$version"
    fi
    base="https://github.com/$REPO/releases/download/$version"
else
    ok Release "$version"
fi

# the binary, with a progress bar in a terminal: its size so far against the total, polled
start=$(date +%s)
if [ "$fancy" = 1 ]; then
    fetch >"$tmp/step.log" 2>&1 &
    bg=$!
    cursor l
    # shellcheck disable=SC2086 # the frames are words on purpose
    set -- $frames
    n=$#
    i=0
    while kill -0 "$bg" 2>/dev/null; do
        i=$((i % n + 1))
        eval "f=\${$i}"
        got=$(got_bytes)
        secs=$(($(date +%s) - start))
        if [ -n "$total" ] && [ "$total" -gt 0 ]; then
            pct=$((got * 100 / total))
            filled=$((pct / 5))
            bar=""
            k=0
            while [ $k -lt 20 ]; do
                if [ $k -lt $filled ]; then bar="$bar$bar_full"; else bar="$bar$bar_empty"; fi
                k=$((k + 1))
            done
            printf '\r  %s%s%s %s%-10s%s %s%s%s %3d%%  %s / %s MB  %s' "$cyan" "$f" "$reset" "$bold" Download \
                "$reset" "$cyan" "$bar" "$reset" "$pct" "$(mb "$got")" "$(mb "$total")" "$(rate "$got" "$secs")"
        else
            printf '\r  %s%s%s %s%-10s%s %s MB  %s' "$cyan" "$f" "$reset" "$bold" Download "$reset" \
                "$(mb "$got")" "$(rate "$got" "$secs")"
        fi
        printf '%s' "${esc:+${esc}[K}"
        sleep 0.1
    done
    status=0
    wait "$bg" || status=$?
    bg=""
    redraw
    cursor h
else
    status=0
    fetch >"$tmp/step.log" 2>&1 || status=$?
fi
if [ "$status" != 0 ]; then
    fail "could not download $base/$asset ($(tail -n 1 "$tmp/step.log" 2>/dev/null))"
fi
size=$(wc -c <"$tmp/$asset" | tr -d ' ')
over=""
if [ -f "$tmp/$asset.in-parts" ]; then over=" over $PARTS connections"; fi
ok Download "$asset · $(mb "$size") MB in $(took $(($(date +%s) - start)))$over"

spin Checksum "comparing with SHA256SUMS.txt" download "$base/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" ||
    fail "could not download $base/SHA256SUMS.txt"
want=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS.txt")
[ -n "$want" ] || fail "SHA256SUMS.txt of $version has no line for $asset"
sum=$(sha256 "$tmp/$asset")
[ "$sum" = "$want" ] || fail "the download does not match its checksum (expected $want, got $sum); nothing was installed"
ok Checksum "matches SHA256SUMS.txt"

# into the folder under a temporary name, then renamed over the old one: a traffic-police that
# is running keeps its file, and a failed copy leaves the old one in place
mkdir -p "$dir" || fail "cannot create $dir"
target="$dir/traffic-police"
cp "$tmp/$asset" "$target.new.$$" || fail "cannot write to $dir"
chmod 755 "$target.new.$$"
mv -f "$target.new.$$" "$target" || {
    rm -f "$target.new.$$"
    fail "cannot replace $target"
}
if [ "$os" = Darwin ]; then
    # files from a browser get this mark and macOS then refuses an unsigned binary; curl sets
    # none, but a copy from elsewhere may carry it
    xattr -d com.apple.quarantine "$target" 2>/dev/null || true
fi
installed=$("$target" --version 2>&1) || fail "$target was installed but does not run here: $installed"
case "$dir" in
    "$HOME"/*) shown="\$HOME/${dir#"$HOME"/}" pretty="$tilde/${dir#"$HOME"/}" ;;
    *) shown="$dir" pretty="$dir" ;;
esac
ok Installed "$installed in $pretty"

# adb: where traffic-police looks for it (the SDK's platform-tools first, then PATH)
find_adb() {
    for sdk in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Library/Android/sdk" "$HOME/Android/Sdk"; do
        if [ -n "$sdk" ] && [ -x "$sdk/platform-tools/adb" ]; then
            say "$sdk/platform-tools/adb"
            return 0
        fi
    done
    command -v adb 2>/dev/null
}
adb_path=$(find_adb) || adb_path=""
if [ -n "$adb_path" ]; then
    case "$adb_path" in
        "$HOME"/*) ok adb "$tilde/${adb_path#"$HOME"/}" ;;
        *) ok adb "$adb_path" ;;
    esac
else
    warn adb "not found: traffic-police needs it (Android's platform-tools) to reach a device"
    if [ "$os" = Darwin ]; then
        note "Android Studio has it, or: brew install --cask android-platform-tools"
    else
        note "Android Studio has it, or: sudo apt install adb (Debian, Ubuntu), sudo dnf install android-tools (Fedora)"
    fi
    note "or the platform-tools from https://developer.android.com/tools/releases/platform-tools"
    note "(traffic-police demo works without it)"
fi

# PATH: the folder, written with $HOME when it is under it
next="Try it: ${bold}traffic-police demo${reset}"
later=""
case ":$PATH:" in
    *":$dir:"* | *":$dir/:"*)
        ok PATH "$pretty is on it"
        ;;
    *)
        if [ "$modify_path" = 0 ]; then
            warn PATH "$pretty is not on it; add it, e.g. in your shell's startup file:"
            note "export PATH=\"$shown:\$PATH\""
        else
            case "$(basename "${SHELL:-sh}")" in
                zsh)
                    rc="${ZDOTDIR:-$HOME}/.zshrc"
                    entry="export PATH=\"$shown:\$PATH\""
                    ;;
                bash)
                    # macOS opens login shells (.bash_profile); Linux terminals read .bashrc
                    if [ "$os" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi
                    entry="export PATH=\"$shown:\$PATH\""
                    ;;
                fish)
                    rc="$HOME/.config/fish/conf.d/traffic-police.fish"
                    entry="fish_add_path -g \"$shown\""
                    ;;
                *)
                    rc="$HOME/.profile"
                    entry="export PATH=\"$shown:\$PATH\""
                    ;;
            esac
            case "$rc" in
                "$HOME"/*) rc_pretty="$tilde/${rc#"$HOME"/}" ;;
                *) rc_pretty="$rc" ;;
            esac
            if grep -qsF "$entry" "$rc"; then
                ok PATH "already in $rc_pretty"
            else
                mkdir -p "$(dirname "$rc")"
                printf '\n# added by the traffic-police installer\n%s\n' "$entry" >>"$rc" ||
                    fail "cannot write to $rc; add $dir to PATH yourself"
                ok PATH "added $pretty to $rc_pretty"
            fi
            next="Open a new terminal, then try it: ${bold}traffic-police demo${reset}"
            later="(or, in this terminal: export PATH=\"$shown:\$PATH\")"
        fi
        ;;
esac
say ""
say "  ${green}Done.${reset} $next"
if [ -n "$later" ]; then say "  ${dim}$later${reset}"; fi
say ""
