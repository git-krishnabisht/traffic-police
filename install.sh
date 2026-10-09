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

say() { printf '%s\n' "$*"; }
fail() {
    printf 'traffic-police installer: %s\n' "$*" >&2
    exit 1
}

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
            arm64) asset=traffic-police-macos-arm64 ;;
            *) fail "there is no build for Intel Macs yet; build it from source: $SOURCE" ;;
        esac
        ;;
    Linux)
        case "$arch" in
            x86_64 | amd64) asset=traffic-police-linux-x86_64 ;;
            *) fail "there is no build for Linux on $arch yet; build it from source: $SOURCE" ;;
        esac
        check_glibc
        ;;
    MINGW* | MSYS* | CYGWIN*)
        fail "on Windows, run this in PowerShell: irm https://raw.githubusercontent.com/$REPO/master/install.ps1 | iex"
        ;;
    *) fail "there is no build for $os; build it from source: $SOURCE" ;;
esac

# a download that stalls (under 1 KB a second for two minutes) is given up, and tried again
download() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --speed-limit 1024 --speed-time 120 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --https-only --timeout=120 --tries=3 -O "$2" "$1"
    else
        fail "needs curl or wget to download"
    fi
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

if [ "$version" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
    what="the latest release"
else
    base="https://github.com/$REPO/releases/download/$version"
    what="release $version"
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t traffic-police)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM

say "Downloading $asset from $what..."
download "$base/$asset" "$tmp/$asset" || fail "could not download $base/$asset (is $version a release of $REPO?)"
download "$base/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" || fail "could not download $base/SHA256SUMS.txt"
want=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS.txt")
[ -n "$want" ] || fail "SHA256SUMS.txt of $what has no line for $asset"
got=$(sha256 "$tmp/$asset")
[ "$got" = "$want" ] || fail "the download does not match its checksum (expected $want, got $got); nothing was installed"

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
say "Installed $installed in $target"

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
    say "adb: $adb_path"
else
    say "adb was not found. traffic-police needs it (Android's platform-tools) to reach a device:"
    if [ "$os" = Darwin ]; then
        say "  Android Studio has it, or: brew install --cask android-platform-tools"
    else
        say "  Android Studio has it, or: sudo apt install adb (Debian, Ubuntu), sudo dnf install android-tools (Fedora)"
    fi
    say "  or the platform-tools from https://developer.android.com/tools/releases/platform-tools"
    say "  (traffic-police demo works without it)"
fi

# PATH: the folder, written with $HOME when it is under it
case ":$PATH:" in
    *":$dir:"* | *":$dir/:"*)
        say "Try it: traffic-police demo"
        exit 0
        ;;
esac
case "$dir" in
    "$HOME"/*) shown="\$HOME/${dir#"$HOME"/}" ;;
    *) shown="$dir" ;;
esac
if [ "$modify_path" = 0 ]; then
    say "$dir is not on your PATH. Add it, e.g. in your shell's startup file: export PATH=\"$shown:\$PATH\""
    exit 0
fi
case "$(basename "${SHELL:-sh}")" in
    zsh)
        rc="${ZDOTDIR:-$HOME}/.zshrc"
        line="export PATH=\"$shown:\$PATH\""
        ;;
    bash)
        # macOS opens login shells (.bash_profile); Linux terminals read .bashrc
        if [ "$os" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi
        line="export PATH=\"$shown:\$PATH\""
        ;;
    fish)
        rc="$HOME/.config/fish/conf.d/traffic-police.fish"
        line="fish_add_path -g \"$shown\""
        ;;
    *)
        rc="$HOME/.profile"
        line="export PATH=\"$shown:\$PATH\""
        ;;
esac
if ! grep -qsF "$line" "$rc"; then
    mkdir -p "$(dirname "$rc")"
    printf '\n# added by the traffic-police installer\n%s\n' "$line" >>"$rc" || fail "cannot write to $rc; add $dir to PATH yourself"
    say "Added $dir to PATH in $rc."
fi
say "Open a new terminal (or run: export PATH=\"$dir:\$PATH\"), then try it: traffic-police demo"
