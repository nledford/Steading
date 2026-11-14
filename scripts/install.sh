#!/usr/bin/env bash
# Installs Ladderback from source, and updates an installation made this way:
#
#   curl -fsSL https://raw.githubusercontent.com/nledford/Ladderback/main/scripts/install.sh | bash
#
# It checks the tools the build needs, installs the ones that need no
# administrator rights (rustup, just, trunk) after asking once, clones
# Ladderback's source into a folder of its own or updates it there, checks out
# the newest release and installs it with `just install`. It never runs sudo:
# a missing C compiler, git or Node.js is named with the command that
# installs it, and nothing is changed. Decision 79
# (docs/design/decisions/79-a-from-source-install-script.md) says why;
# `features/cli/install_script.feature` specifies it. `--help` lists the
# options.
#
# The whole script is a set of functions, and the last line calls `main`: a
# download cut short defines nothing it then runs.
#
# LADDERBACK_INSTALL_TTY names the terminal the one question is asked on
# (default /dev/tty, as standard input is the script itself when it is piped
# into bash). The scenarios point it at a file, or at nothing.

set -euo pipefail

readonly LADDERBACK_URL="https://github.com/nledford/Ladderback.git"
readonly SCRIPT_URL="https://raw.githubusercontent.com/nledford/Ladderback/main/scripts/install.sh"
readonly BY_HAND_URL="https://github.com/nledford/Ladderback/blob/main/docs/guide/install.md#by-hand"
readonly FULL_DISK_ACCESS_URL="https://github.com/nledford/Ladderback/blob/main/docs/guide/install.md#macos-full-disk-access"
readonly RUSTUP_URL="https://sh.rustup.rs"
readonly TRUNK_RELEASES_URL="https://github.com/trunk-rs/trunk/releases"
# The trunk installed where it is missing: from its release (trunk_release),
# or built from source where trunk publishes none.
readonly TRUNK_VERSION=0.22.1

usage() {
    cat <<EOF
Usage: install.sh [--yes] [--dir <folder>] [-- <options for just install>]

Builds the newest release of Ladderback from source and installs it with
\`just install\`. Running it again updates to the newest release.

Options:
  -y, --yes         install missing tools (rustup, just, trunk) without asking
      --dir <folder>
                    where to keep Ladderback's source (default:
                    \${XDG_DATA_HOME:-~/.local/share}/ladderback/source)
  -h, --help        show this help
  --                what follows goes to \`just install\`, which passes it to
                    \`cargo install\`: \`-- --root ~/.local\` installs into
                    ~/.local/bin

Piped into bash, options follow \`bash -s --\`:
  curl -fsSL $SCRIPT_URL | bash -s -- --yes
EOF
}

say() {
    printf '%s\n' "$*"
}

warn() {
    printf '%s\n' "$*" >&2
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

# Whether version $1 is at least $2: dot-separated numbers, compared from the
# left; anything after a `-` or `+` is ignored.
version_at_least() {
    local have=${1%%[-+]*} want=${2%%[-+]*}
    local h w
    while [ -n "$want" ]; do
        h=${have%%.*}
        w=${want%%.*}
        [[ $h =~ ^[0-9]+$ ]] || h=0
        [[ $w =~ ^[0-9]+$ ]] || w=0
        if ((10#$h > 10#$w)); then return 0; fi
        if ((10#$h < 10#$w)); then return 1; fi
        if [ "$have" = "${have#*.}" ]; then have=''; else have=${have#*.}; fi
        if [ "$want" = "${want#*.}" ]; then want=''; else want=${want#*.}; fi
    done
    return 0
}

# The version a tool reports: the first word of its `--version` that starts
# with a digit, after an optional `v`.
tool_version() {
    local out word words=()
    out=$("$@" 2>/dev/null) || true
    read -r -a words <<<"${out%%$'\n'*}" || true
    for word in ${words[@]+"${words[@]}"}; do
        word=${word#v}
        if [[ $word =~ ^[0-9] ]]; then
            printf '%s\n' "$word"
            return 0
        fi
    done
    return 0
}

has() {
    command -v "$1" >/dev/null 2>&1
}

# The package manager whose command is printed for a missing tool.
package_manager() {
    if [ "$os" = Darwin ]; then
        if has brew; then say brew; else say macos; fi
    elif has apt-get; then
        say apt
    elif has dnf; then
        say dnf
    elif has pacman; then
        say pacman
    else
        say other
    fi
}

# How to install what this script never installs, for this system.
how_cc() {
    case $1 in
        brew | macos) say "xcode-select --install" ;;
        apt) say "sudo apt install build-essential" ;;
        dnf) say "sudo dnf install gcc" ;;
        pacman) say "sudo pacman -S --needed base-devel" ;;
        *) say "install gcc or clang with your system's package manager" ;;
    esac
}

how_git() {
    case $1 in
        brew | macos) say "xcode-select --install" ;;
        apt) say "sudo apt install git" ;;
        dnf) say "sudo dnf install git" ;;
        pacman) say "sudo pacman -S --needed git" ;;
        *) say "install git with your system's package manager" ;;
    esac
}

how_node() {
    case $1 in
        brew) say "brew install node" ;;
        apt) say "sudo apt install nodejs npm (if your release's Node.js is older than 20, take it from https://nodejs.org)" ;;
        dnf) say "sudo dnf install nodejs npm" ;;
        pacman) say "sudo pacman -S --needed nodejs npm" ;;
        *) say "take it from https://nodejs.org" ;;
    esac
}

# Whether the terminal the question is asked on can be opened.
have_terminal() {
    (: <"$tty") 2>/dev/null
}

parse_args() {
    while [ $# -gt 0 ]; do
        case $1 in
            -y | --yes) yes=1 ;;
            --dir)
                if [ $# -lt 2 ] || [ -z "$2" ]; then
                    warn "error: --dir needs a folder"
                    usage >&2
                    exit 2
                fi
                dir=$2
                shift
                ;;
            --dir=*)
                dir=${1#--dir=}
                if [ -z "$dir" ]; then
                    warn "error: --dir needs a folder"
                    usage >&2
                    exit 2
                fi
                ;;
            -h | --help)
                usage
                exit 0
                ;;
            --)
                shift
                forward=("$@")
                return 0
                ;;
            *)
                warn "error: unknown option: $1"
                usage >&2
                exit 2
                ;;
        esac
        shift
    done
}

# Checks the tools the build needs. What needs administrator rights, or can't
# be installed without rustup, is named with how to install it, and the script
# stops before changing anything. What it can install goes into `install_rust`,
# `install_tools` (built with cargo), and `fetch_trunk` or `build_trunk`.
check_tools() {
    local pm missing=() version
    pm=$(package_manager)

    if [ "$os" = Darwin ] && ! xcode-select -p >/dev/null 2>&1; then
        # Every Mac has /usr/bin/cc and /usr/bin/git, which without the
        # Command Line Tools only offer to install them.
        missing+=("Apple's Command Line Tools, with a C compiler and git: xcode-select --install")
    else
        if ! has git; then
            missing+=("git: $(how_git "$pm")")
        fi
        if ! has cc; then
            missing+=("a C compiler: $(how_cc "$pm")")
        fi
    fi
    if ! has node || ! has npm; then
        missing+=("Node.js 20 or newer, with npm: $(how_node "$pm")")
    else
        version=$(tool_version node --version)
        if ! version_at_least "${version:-0}" 20; then
            missing+=("Node.js 20 or newer, with npm (found ${version:-an unknown version}): $(how_node "$pm")")
        fi
    fi

    if has rustup; then
        rustup=1
    elif has cargo && has rustc; then
        rustup=0
    else
        rustup=1
        install_rust=1
    fi

    version=''
    if has just; then
        version=$(tool_version just --version)
    fi
    if [ -z "$version" ] || ! version_at_least "$version" 1.40; then
        if [ "$rustup" = 1 ]; then
            install_tools+=(just)
        else
            missing+=("just 1.40 or newer: take a prebuilt release from https://github.com/casey/just/releases, or install Rust with rustup (https://rustup.rs) and run this again")
        fi
    fi
    if ! has trunk; then
        trunk_release
        if [ -n "$trunk_target" ]; then
            fetch_trunk=1
        elif [ "$rustup" = 1 ]; then
            build_trunk=1
            trunk_jobs=$(trunk_build_jobs)
        else
            missing+=("trunk: it publishes no release for $os on $(uname -m), and building it needs rustup: install Rust with rustup (https://rustup.rs) and run this again")
        fi
    fi
    if ! has curl && { [ "$install_rust" = 1 ] || [ "$fetch_trunk" = 1 ]; }; then
        missing+=("curl, to download rustup's installer and trunk's release: install it with your system's package manager")
    fi

    if [ "${#missing[@]}" -gt 0 ]; then
        warn "error: Ladderback's build needs these, which this script doesn't install:"
        local line
        for line in "${missing[@]}"; do
            warn "  - $line"
        done
        warn "Install them, then run this again. Nothing was changed."
        exit 1
    fi
}

# trunk's release archive for this system, at TRUNK_VERSION, and its SHA-256:
# sets trunk_target and trunk_sha, or leaves them empty where trunk publishes
# none. On Linux it is the static (musl) build, as trunk's gnu builds need
# glibc 2.39 and OpenSSL 3, which Debian 12 and Ubuntu 22.04 lack. To move to
# another trunk, change TRUNK_VERSION and the four hashes, taking each from
# the release's `<archive>.sha256` asset after checking it against the
# downloaded archive.
trunk_release() {
    trunk_target=''
    trunk_sha=''
    case "$os-$(uname -m)" in
        Linux-x86_64) trunk_target=x86_64-unknown-linux-musl trunk_sha=a1fe5cd6b053f5a76f66b598d296022a7f06f56def6013cc16cd6897d53f5363 ;;
        Linux-aarch64 | Linux-arm64) trunk_target=aarch64-unknown-linux-musl trunk_sha=8c84a939aaf48bbdac2d78195c932adb319e610fb2cbf791bae07b2660cf3e46 ;;
        Darwin-x86_64) trunk_target=x86_64-apple-darwin trunk_sha=55b5cf2d6a23201f555cc561cc07157989581cc720a260505f8e98aae67abcf7 ;;
        Darwin-arm64) trunk_target=aarch64-apple-darwin trunk_sha=18e381936c848700fefb9ee3f64f362419fab700200eefee323ebc0d00a8e9db ;;
    esac
}

# How many jobs build trunk from source where it publishes no release: one
# per 2 GB of the memory free now, at least one, and no more than the
# processors online. Where getconf can't say, one. With cargo's default of a
# job per processor, the build took all of a 4 GB Raspberry Pi's memory and
# swap until it reset (#538).
trunk_build_jobs() {
    local pages page_size processors jobs=0
    pages=$(getconf _AVPHYS_PAGES 2>/dev/null || true)
    page_size=$(getconf PAGESIZE 2>/dev/null || true)
    if [[ $pages =~ ^[0-9]{1,12}$ ]] && [[ $page_size =~ ^[0-9]{1,7}$ ]]; then
        jobs=$((10#$pages * 10#$page_size / 2147483648))
    fi
    processors=$(getconf _NPROCESSORS_ONLN 2>/dev/null || true)
    if [[ $processors =~ ^[0-9]{1,6}$ ]] && ((10#$processors > 0 && jobs > 10#$processors)); then
        jobs=$((10#$processors))
    fi
    if ((jobs < 1)); then
        jobs=1
    fi
    say "$jobs"
}

# Downloads trunk's release archive for this system and checks it against the
# SHA-256 pinned in trunk_release, before anything is installed: a mismatch
# stops the script with nothing changed. The archive holds `trunk` alone,
# which is unpacked into the scratch folder.
download_trunk() {
    local archive="trunk-$trunk_target.tar.gz" actual
    say "Downloading trunk $TRUNK_VERSION for $trunk_target"
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location --retry 3 \
        -o "$scratch/$archive" "$TRUNK_RELEASES_URL/download/v$TRUNK_VERSION/$archive" ||
        die "trunk's release couldn't be downloaded. Nothing was installed; run this again."
    if has sha256sum; then
        actual=$(sha256sum "$scratch/$archive") || die "sha256sum couldn't read trunk's release. Nothing was installed."
    elif has shasum; then
        actual=$(shasum -a 256 "$scratch/$archive") || die "shasum couldn't read trunk's release. Nothing was installed."
    else
        die "neither sha256sum nor shasum is installed, to check trunk's release. Nothing was installed."
    fi
    actual=${actual%% *}
    if [ "$actual" != "$trunk_sha" ]; then
        die "trunk's release $archive doesn't match the SHA-256 this script pins: it has $actual, not $trunk_sha. Nothing was installed. Run this again; if it says the same, report it: https://github.com/nledford/Ladderback/issues"
    fi
    tar -xzf "$scratch/$archive" -C "$scratch" trunk
}

# Refuses a source folder this script must not touch: one that holds
# something other than a clone of Ladderback, or a clone with changes.
check_source_folder() {
    if [ ! -e "$dir" ]; then
        return 0
    fi
    if [ ! -d "$dir" ]; then
        die "$dir isn't a folder. Nothing was changed; choose another with --dir."
    fi
    if is_empty_folder "$dir"; then
        return 0
    fi
    local top url
    top=$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null || true)
    url=$(git -C "$dir" config --get remote.origin.url 2>/dev/null || true)
    if [ "$top" != "$(cd "$dir" && pwd -P)" ] || ! is_ladderback_url "$url"; then
        die "$dir isn't a clone of Ladderback's repository. Nothing in it was changed; choose another folder with --dir."
    fi
    if [ -n "$(git -C "$dir" status --porcelain --untracked-files=no)" ]; then
        die "$dir has changes that aren't committed. Nothing was changed; commit or stash them (git -C '$dir' stash), then run this again."
    fi
    update=1
}

# Whether folder $1 holds nothing, hidden files included.
is_empty_folder() {
    (
        shopt -s nullglob dotglob
        entries=("$1"/*)
        [ "${#entries[@]}" -eq 0 ]
    )
}

# Whether the clone's `origin` is Ladderback's repository, by any of the
# addresses GitHub gives it.
is_ladderback_url() {
    local url=$1 matched=1
    shopt -s nocasematch
    case $url in
        https://github.com/nledford/ladderback | https://github.com/nledford/ladderback.git | \
            git@github.com:nledford/ladderback | git@github.com:nledford/ladderback.git | \
            ssh://git@github.com/nledford/ladderback | ssh://git@github.com/nledford/ladderback.git)
            matched=0
            ;;
    esac
    shopt -u nocasematch
    return "$matched"
}

# Installs rustup, just and trunk where they are missing, after asking once.
# trunk's release is downloaded and checked first, so an archive that fails
# its check leaves nothing installed.
install_missing_tools() {
    if [ "$install_rust" = 0 ] && [ "${#install_tools[@]}" -eq 0 ] &&
        [ "$fetch_trunk" = 0 ] && [ "$build_trunk" = 0 ]; then
        return 0
    fi
    warn "Ladderback's build needs tools that aren't installed. This script will install:"
    if [ "$install_rust" = 1 ]; then
        warn "  - Rust, with rustup's installer from $RUSTUP_URL$([ "$yes" = 1 ] && printf ' (with -y: its defaults, no questions)' || printf ' (it asks its own questions)')"
    fi
    if [ "${#install_tools[@]}" -gt 0 ]; then
        warn "  - ${install_tools[*]}, built from source with: cargo +stable install --locked ${install_tools[*]}"
    fi
    if [ "$fetch_trunk" = 1 ]; then
        warn "  - trunk $TRUNK_VERSION, from trunk's release for $trunk_target, checked against the SHA-256 this script pins"
    elif [ "$build_trunk" = 1 ]; then
        warn "  - trunk $TRUNK_VERSION, built from source, as it publishes no release for this system, with: cargo +stable install --locked -j $trunk_jobs trunk@$TRUNK_VERSION"
    fi
    if [ "$yes" = 0 ]; then
        if ! have_terminal; then
            warn "error: there is no terminal to ask on. Nothing was installed. To install them without asking, run:"
            warn "  curl -fsSL $SCRIPT_URL | bash -s -- --yes"
            exit 1
        fi
        local answer=''
        printf 'Install them? [y/N] ' >&2
        read -r answer <"$tty" || answer=''
        case $answer in
            [yY] | [yY][eE][sS]) ;;
            *)
                warn "Nothing was installed."
                exit 1
                ;;
        esac
    fi

    local cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
    if [ "$fetch_trunk" = 1 ]; then
        download_trunk
    fi
    if [ "$install_rust" = 1 ]; then
        say "Installing Rust with rustup's installer"
        curl --proto '=https' --tlsv1.2 -sSf "$RUSTUP_URL" -o "$scratch/rustup-init.sh"
        if [ "$yes" = 1 ]; then
            sh "$scratch/rustup-init.sh" -y
        else
            sh "$scratch/rustup-init.sh" <"$tty"
        fi
    fi
    # What rustup and cargo install goes here; first on the PATH, so this run
    # finds what it just installed.
    PATH="$cargo_bin:$PATH"
    if [ "${#install_tools[@]}" -gt 0 ]; then
        say "Installing ${install_tools[*]}"
        cargo +stable install --locked "${install_tools[@]}"
    fi
    if [ "$fetch_trunk" = 1 ]; then
        mkdir -p "$cargo_bin"
        install -m 0755 "$scratch/trunk" "$cargo_bin/trunk"
        say "Installed trunk $TRUNK_VERSION into $cargo_bin"
    elif [ "$build_trunk" = 1 ]; then
        say "trunk publishes no release for this system: building it from source with $trunk_jobs build job(s), one per 2 GB of free memory"
        cargo +stable install --locked -j "$trunk_jobs" "trunk@$TRUNK_VERSION"
    fi
    local version
    version=$(tool_version just --version)
    if ! version_at_least "${version:-0}" 1.40 || ! has trunk; then
        die "just 1.40 or newer and trunk were installed into $cargo_bin, but this run doesn't find them there: put $cargo_bin first on your PATH and run this again"
    fi
}

# Clones Ladderback, or fetches its new releases, and checks out the newest
# release, leaving out release candidates (vX.Y.Z-rc.N) as the install guide does.
checkout_newest_release() {
    if [ "$update" = 1 ]; then
        say "Fetching Ladderback's releases into $dir"
        git -C "$dir" fetch --tags
    else
        say "Cloning Ladderback into $dir"
        mkdir -p "${dir%/*}/"
        git clone --quiet -- "$LADDERBACK_URL" "$dir"
    fi
    local tags tag
    tags=$(git -C "$dir" tag --list 'v*' --sort=-v:refname)
    release=''
    while IFS= read -r tag; do
        case $tag in
            '' | *-*) ;;
            *)
                release=$tag
                break
                ;;
        esac
    done <<<"$tags"
    if [ -z "$release" ]; then
        die "Ladderback has no release to install yet (no vX.Y.Z tag in $dir)."
    fi
    say "Checking out $release"
    git -C "$dir" -c advice.detachedHead=false checkout --quiet "$release"
}

# Without rustup nothing installs the Rust the checkout names, so the one
# on the PATH must be new enough and have the WebAssembly target.
check_rust_without_rustup() {
    local msrv='' line version sysroot
    local pattern='^rust-version *= *"([0-9.]+)"'
    while IFS= read -r line || [ -n "$line" ]; do
        if [[ $line =~ $pattern ]]; then
            msrv=${BASH_REMATCH[1]}
            break
        fi
    done <"$dir/Cargo.toml"
    version=$(tool_version rustc --version)
    if [ -n "$msrv" ] && ! version_at_least "${version:-0}" "$msrv"; then
        die "Rust ${version:-of an unknown version} is older than $msrv, the oldest $release builds with. Install Rust with rustup (https://rustup.rs), or a newer Rust, and run this again."
    fi
    sysroot=$(rustc --print sysroot)
    if [ ! -d "$sysroot/lib/rustlib/wasm32-unknown-unknown" ]; then
        die "this Rust has no wasm32-unknown-unknown target, which the web UI is built for. Install the target, or Rust with rustup (https://rustup.rs), and run this again."
    fi
}

# With rustup, the release builds with the Rust its rust-toolchain.toml pins
# (decision 78), whatever the environment names instead: RUSTUP_TOOLCHAIN
# outranks the file, and RUSTC or CARGO_BUILD_RUSTC has cargo run another
# compiler. They are unset for the rest of this run, which is a process of
# its own: the shell it was started from keeps them. The pinned Rust, with
# the components and targets the file lists, is installed here when rustup
# lacks it, not left to rustup's automatic install, which rustup has
# deprecated (rust-lang/rustup#4836). The version is passed by name, as
# `rustup toolchain install` reads the file itself only from rustup 1.28 on,
# and `--no-self-update` keeps it from updating rustup itself.
use_pinned_rust() {
    local line channel='' components='' targets='' list
    local channel_pattern='^channel *= *"([^"]+)"'
    local list_pattern='^(components|targets) *= *\[(.*)\]'
    if [ -f "$dir/rust-toolchain.toml" ]; then
        while IFS= read -r line || [ -n "$line" ]; do
            if [[ $line =~ $channel_pattern ]]; then
                channel=${BASH_REMATCH[1]}
            elif [[ $line =~ $list_pattern ]]; then
                # ["rustfmt", "clippy"] becomes rustfmt,clippy.
                list=${BASH_REMATCH[2]//\"/}
                list=${list// /}
                list=${list%,}
                if [ "${BASH_REMATCH[1]}" = components ]; then
                    components=$list
                else
                    targets=$list
                fi
            fi
        done <"$dir/rust-toolchain.toml"
    fi
    if [ -z "$channel" ]; then
        die "$release's rust-toolchain.toml names no Rust version to build it with. Build it by hand: $BY_HAND_URL"
    fi

    local name value
    for name in RUSTUP_TOOLCHAIN RUSTC CARGO_BUILD_RUSTC; do
        value=${!name:-}
        if [ -n "$value" ]; then
            say "Ignoring $name=$value: Ladderback $release builds with the Rust it pins, $channel."
        fi
        unset "$name"
    done

    if ! has_rust "$channel" "$components" "$targets"; then
        say "Installing Rust $channel, which Ladderback $release builds with"
        local install=("$channel" --no-self-update)
        if [ -n "$components" ]; then install+=(--component "$components"); fi
        if [ -n "$targets" ]; then install+=(--target "$targets"); fi
        rustup toolchain install "${install[@]}"
    fi
}

# Whether rustup has Rust $1 with components $2 and targets $3, each a
# comma-separated list. It asks rustup only what is installed, so it neither
# downloads nor installs anything.
has_rust() {
    local installed item items=()
    installed=$'\n'$(rustup toolchain list 2>/dev/null)
    case $installed in
        *$'\n'"$1"-*) ;;
        *) return 1 ;;
    esac
    installed=$'\n'$(rustup component list --installed --toolchain "$1" 2>/dev/null)$'\n' || return 1
    IFS=, read -r -a items <<<"$2"
    for item in ${items[@]+"${items[@]}"}; do
        case $installed in
            *$'\n'"$item"-*) ;;
            *) return 1 ;;
        esac
    done
    IFS=, read -r -a items <<<"$3"
    for item in ${items[@]+"${items[@]}"}; do
        case $installed in
            *$'\n'"rust-std-$item"$'\n'*) ;;
            *) return 1 ;;
        esac
    done
}

# On 64-bit ARM Linux with a glibc older than 2.38, the wasm-bindgen trunk
# downloads doesn't run; the version the checkout pins is built instead, in
# the checkout so that it is built with the Rust the checkout pins, and trunk
# finds it on the PATH (docs/guide/install.md, "By hand").
build_wasm_bindgen_if_needed() {
    local machine glibc
    machine=$(uname -m)
    if [ "$os" != Linux ] || { [ "$machine" != aarch64 ] && [ "$machine" != arm64 ]; }; then
        return 0
    fi
    glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
    glibc=${glibc#glibc }
    if [ -z "$glibc" ] || version_at_least "$glibc" 2.38; then
        return 0
    fi
    local version
    version=$(cd "$dir" && just _wasm-bindgen-version)
    say "Building wasm-bindgen $version: the one trunk downloads doesn't run with glibc $glibc"
    (cd "$dir" && cargo install --locked wasm-bindgen-cli --version "$version")
}

# The web UI's build regenerates its stylesheet, `crates/web/tw-out.css`.
# Releases up to v0.2.0 track it, so there it is put back, and the next run
# doesn't refuse the source folder as changed. Later releases ignore it
# (#381), and the one the build wrote stays.
restore_stylesheet() {
    local css=crates/web/tw-out.css
    if git -C "$dir" ls-files --error-unmatch -- "$css" >/dev/null 2>&1; then
        git -C "$dir" checkout --quiet -- "$css"
    fi
}

# Where `cargo install` puts the binary: `--root` among the options, else
# CARGO_INSTALL_ROOT, else CARGO_HOME, else ~/.cargo. A relative root is
# relative to the source folder, where `just install` runs.
install_root() {
    local root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}" next=0 arg
    for arg in ${forward[@]+"${forward[@]}"}; do
        if [ "$next" = 1 ]; then
            root=$arg
            next=0
            continue
        fi
        case $arg in
            --root) next=1 ;;
            --root=*) root=${arg#--root=} ;;
        esac
    done
    case $root in
        /*) ;;
        *) root="$dir/$root" ;;
    esac
    say "$root"
}

# Where command $1 is on the PATH this was started with, if it is.
on_start_path() {
    local PATH=$start_path
    command -v "$1" 2>/dev/null || true
}

report() {
    local bin_dir binary version
    bin_dir="$(install_root)/bin"
    binary="$bin_dir/ladderback"
    if [ ! -x "$binary" ] && has ladderback; then
        binary=$(command -v ladderback)
        bin_dir=${binary%/*}
    fi
    say ""
    if [ -x "$binary" ]; then
        version=$("$binary" --version)
        say "Installed $version as $binary."
    else
        say "Installed $release; cargo's configuration put it in a folder this script didn't find."
    fi
    if [ "$(on_start_path ladderback)" != "$binary" ]; then
        if [ "$install_rust" = 1 ] && [ "$bin_dir" = "${CARGO_HOME:-$HOME/.cargo}/bin" ]; then
            say "Open a new terminal before you go on: rustup's installer put $bin_dir on the PATH of new ones."
        else
            say "Your PATH doesn't lead to it: put $bin_dir first on your PATH, or run it by its full path."
        fi
    fi
    say ""
    say "Next:"
    say "  ladderback serve              starts it; open the address it prints"
    say "  ladderback install-service    runs it in the background, from each login on"
    if [ "$os" = Darwin ]; then
        say ""
        say "On macOS, grant $binary Full Disk Access in System Settings → Privacy & Security,"
        say "and again after each update: $FULL_DISK_ACCESS_URL"
    fi
    say ""
    say "To update, run the same command again."
}

main() {
    # The PATH this was started with: the one the person's shell has.
    start_path=$PATH

    # Standard input is the script itself when it is piped into bash: nothing
    # this runs may read it.
    exec </dev/null

    yes=0
    dir=''
    forward=()
    rustup=1
    install_rust=0
    install_tools=()
    fetch_trunk=0
    build_trunk=0
    trunk_jobs=1
    trunk_target=''
    trunk_sha=''
    update=0
    release=''
    tty=${LADDERBACK_INSTALL_TTY:-/dev/tty}

    parse_args "$@"

    os=$(uname -s)
    case $os in
        Linux | Darwin) ;;
        *) die "Ladderback builds on Linux and macOS; this is $os." ;;
    esac

    if [ -z "$dir" ]; then
        dir="${XDG_DATA_HOME:-$HOME/.local/share}/ladderback/source"
    fi
    case $dir in
        \~) dir=$HOME ;;
        \~/*) dir="$HOME/${dir:2}" ;;
    esac
    case $dir in
        /*) ;;
        *) dir="$PWD/$dir" ;;
    esac
    while [ "${#dir}" -gt 1 ] && [ "${dir%/}" != "$dir" ]; do
        dir=${dir%/}
    done

    # rustup's folder comes first, as its installer puts it in new shells: its
    # cargo before one the system's packages installed, and Rust that rustup
    # installed before found in a shell that hasn't picked up its PATH yet.
    local cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
    if [ -d "$cargo_bin" ]; then
        PATH="$cargo_bin:$PATH"
    fi

    check_tools
    check_source_folder

    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' EXIT

    install_missing_tools
    checkout_newest_release
    if [ "$rustup" = 1 ]; then
        use_pinned_rust
    else
        check_rust_without_rustup
    fi
    build_wasm_bindgen_if_needed

    say "Building and installing Ladderback $release (several minutes)"
    local status=0
    (cd "$dir" && just install ${forward[@]+"${forward[@]}"}) || status=$?
    restore_stylesheet
    if [ "$status" != 0 ]; then
        die "\`just install\` failed in $dir. Fix what it names, then run this again."
    fi

    report
}

main "$@"
