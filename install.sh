#!/usr/bin/env bash
# AI Command installer: builds `cmd` from source and installs it for the current user.
#
#   curl -fsSL https://raw.githubusercontent.com/uxeric/ai-command/HEAD/install.sh | bash
#   ./install.sh                 # from a checkout: builds that checkout
#   ./install.sh --force         # rebuild even when the installed cmd is up to date
#   ./install.sh --uninstall     # removes cmd and the zsh helper this script added
#
# Settings (environment variables):
#   CMD_REPO        git URL to clone          (default: https://github.com/uxeric/ai-command.git)
#   CMD_REF         branch or tag to build    (default: the repo's default branch)
#   CMD_PREFIX      install prefix            (default: ~/.local, so cmd goes in ~/.local/bin)
#   CMD_SOURCE_DIR  build this directory instead of cloning
#   CMD_YES=1       answer yes to prompts (e.g. installing Rust)
#
# This script never reads or writes a key. Sign in separately with `grok login`.

set -euo pipefail

REPO="${CMD_REPO:-https://github.com/uxeric/ai-command.git}"
REF="${CMD_REF:-}"
PREFIX="${CMD_PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/cmd"
ZSH_FN="$DATA_DIR/cmd.zsh"
STAMP="$DATA_DIR/installed-rev"
MIN_RUST="1.88"
FORCE=0
ZSH_BEGIN="# >>> cmd >>>"
ZSH_END="# <<< cmd >>>"

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
    ICE=$'\e[38;2;41;240;255m' HOT=$'\e[38;2;255;42;109m' AMBER=$'\e[38;2;246;196;69m'
    LEAF=$'\e[38;2;61;255;154m' GHOST=$'\e[38;2;110;106;130m' BOLD=$'\e[1m' RESET=$'\e[0m'
else
    ICE="" HOT="" AMBER="" LEAF="" GHOST="" BOLD="" RESET=""
fi

step() { printf '%s›%s %s\n' "$HOT" "$RESET" "$*"; }
ok() { printf '  %s✓%s %s\n' "$LEAF" "$RESET" "$*"; }
warn() { printf '  %s!%s %s\n' "$AMBER" "$RESET" "$*" >&2; }
die() { printf '%s✗ %s%s\n' "$HOT" "$*" "$RESET" >&2; exit 1; }

banner() {
    # Full blocks only, so the word stays CMD when a terminal draws box
    # marks on the wrong half of the cell.
    printf '\n%s%s' "$HOT" "$BOLD"
    printf '   ██████  █     █  ██████ \n'
    printf '  █        ██   ██  █     █\n'
    printf '%s  █        █ █ █ █  █     █\n' "$AMBER"
    printf '  █        █  █  █  █     █\n'
    printf '%s  █        █     █  █     █\n' "$ICE"
    printf '   ██████  █     █  ██████ %s\n\n' "$RESET"
    printf '  %sAI Command · the command, placed on the line · installer%s\n\n' "$GHOST" "$RESET"
}

confirm() {
    [[ "${CMD_YES:-}" == 1 ]] && return 0
    local answer=""
    { exec 3< /dev/tty; } 2> /dev/null || return 1
    printf '  %s?%s %s [y/N] ' "$AMBER" "$RESET" "$1" 2> /dev/null > /dev/tty
    read -r answer <&3 || true
    exec 3<&-
    [[ "$answer" == [yY]* ]]
}

is_omarchy() {
    command -v omarchy-install-dev-env > /dev/null || command -v omarchy-version > /dev/null
}

version_at_least() {
    [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" == "$2" ]]
}

ensure_rust() {
    if ! command -v cargo > /dev/null && [[ -x "$HOME/.cargo/bin/cargo" ]]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
    if command -v cargo > /dev/null; then
        local have
        have="$(rustc --version | awk '{print $2}')"
        version_at_least "$have" "$MIN_RUST" || die "cmd needs Rust $MIN_RUST or newer (found $have). Run: rustup update"
        ok "Rust $have"
        return
    fi
    warn "Rust is not installed."
    if is_omarchy && command -v omarchy-install-dev-env > /dev/null; then
        confirm "Install Rust the Omarchy way (omarchy-install-dev-env rust)?" \
            || die "Install Rust ($MIN_RUST+) and run this again."
        omarchy-install-dev-env rust
        export PATH="$HOME/.cargo/bin:$PATH"
    elif command -v pacman > /dev/null; then
        confirm "Install Rust with pacman (sudo pacman -S --needed rust)?" \
            || die "Install Rust ($MIN_RUST+) and run this again."
        sudo pacman -S --needed --noconfirm rust
    else
        confirm "Install Rust with rustup (https://rustup.rs)?" \
            || die "Install Rust ($MIN_RUST+) and run this again."
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
    command -v cargo > /dev/null || die "Rust was installed but cargo is not on PATH. Open a new shell and run this again."
    local have
    have="$(rustc --version | awk '{print $2}')"
    version_at_least "$have" "$MIN_RUST" || die "cmd needs Rust $MIN_RUST or newer (found $have). Run: rustup update"
    ok "Rust $have"
}

ensure_cc() {
    if command -v cc > /dev/null || command -v gcc > /dev/null; then
        return
    fi
    die "cmd needs a C compiler to build TLS. Install one with: sudo pacman -S base-devel"
}

is_checkout() {
    [[ -f "$1/Cargo.toml" && -f "$1/cmd.zsh" ]] && grep -q '^name = "cmd"' "$1/Cargo.toml"
}

find_source() {
    if [[ -n "${CMD_SOURCE_DIR:-}" ]]; then
        is_checkout "$CMD_SOURCE_DIR" || die "CMD_SOURCE_DIR=$CMD_SOURCE_DIR is not an cmd checkout."
        SOURCE="$CMD_SOURCE_DIR"
        ok "building $SOURCE"
        return
    fi
    local here=""
    if [[ -n "${BASH_SOURCE[0]:-}" && -f "${BASH_SOURCE[0]}" ]]; then
        here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    fi
    if [[ -n "$here" ]] && is_checkout "$here"; then
        SOURCE="$here"
        ok "building this checkout ($SOURCE)"
        return
    fi
    command -v git > /dev/null || die "git is needed to download cmd. Install git and run this again."
    SOURCE="$DATA_DIR/src"
    if [[ -d "$SOURCE/.git" ]]; then
        git -C "$SOURCE" fetch --quiet --depth 1 origin "${REF:-HEAD}" || die "Could not update $SOURCE from $REPO."
        git -C "$SOURCE" checkout --quiet --force FETCH_HEAD
        ok "updated $SOURCE (${REF:-default branch})"
    else
        mkdir -p "$DATA_DIR"
        local branch=()
        [[ -n "$REF" ]] && branch=(--branch "$REF")
        git clone --quiet --depth 1 "${branch[@]}" "$REPO" "$SOURCE" \
            || die "Could not clone $REPO${REF:+ ($REF)}. Set CMD_REPO (or CMD_REF) to the right value."
        ok "downloaded $REPO (${REF:-default branch})"
    fi
}

source_rev() {
    git -C "$SOURCE" rev-parse --is-inside-work-tree > /dev/null 2>&1 || return 0
    [[ -z "$(git -C "$SOURCE" status --porcelain --untracked-files=no 2> /dev/null)" ]] || return 0
    git -C "$SOURCE" rev-parse --short HEAD 2> /dev/null || true
}

install_zsh_fn() {
    [[ -f "$SOURCE/cmd.zsh" ]] || die "cmd.zsh is missing from $SOURCE."
    install -Dm644 "$SOURCE/cmd.zsh" "$ZSH_FN"
    ok "installed $ZSH_FN"
}

wire_zsh() {
    local rc="$HOME/.zshrc"
    if [[ -f "$rc" ]] && grep -q 'cmd\.zsh' "$rc"; then
        ok "zsh already loads cmd"
        return
    fi
    touch "$rc"
    {
        printf '\n%s\n' "$ZSH_BEGIN"
        printf '%s\n' '# Puts the chosen command on the line. Does not run it.'
        printf '[[ -f %q ]] && source %q\n' "$ZSH_FN" "$ZSH_FN"
        printf '%s\n' "$ZSH_END"
    } >> "$rc"
    ok "added cmd to ~/.zshrc (open a new shell)"
}

unwire_zsh() {
    local rc="$HOME/.zshrc"
    [[ -f "$rc" ]] || return 0
    grep -qF "$ZSH_BEGIN" "$rc" || return 0
    local tmp
    tmp="$(mktemp)"
    awk -v b="$ZSH_BEGIN" -v e="$ZSH_END" '
        $0 == b { skip = 1; next }
        $0 == e { skip = 0; next }
        !skip { print }
    ' "$rc" > "$tmp"
    mv "$tmp" "$rc"
    ok "removed the cmd block from ~/.zshrc"
}

check_path() {
    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *)
            warn "$BIN_DIR is not on your PATH. Add this to your shell's rc file:"
            printf '      export PATH="%s:$PATH"\n' "$BIN_DIR" >&2
            ;;
    esac
}

remind_login() {
    local auth="${GROK_HOME:-$HOME/.grok}/auth.json"
    if [[ -f "$auth" ]]; then
        ok "Grok login is in place ($auth)"
        return
    fi
    warn "No Grok login yet. cmd has no key of its own. In a terminal, run:"
    printf '      %sgrok login%s\n' "$ICE" "$RESET" >&2
    printf '      %shttps://x.ai/cli  ·  curl -fsSL https://x.ai/cli/install.sh | bash%s\n' "$GHOST" "$RESET" >&2
}

finish() {
    check_path
    remind_login
    printf '\n  %s%s%s%s On a request, run %s%scmd%s. The line waits for Enter.\n\n' \
        "$ICE" "$BOLD" "$1" "$RESET" "$HOT" "$BOLD" "$RESET"
}

install_cmd() {
    banner
    step "checking the toolchain"
    ensure_rust
    step "getting the source"
    find_source
    local rev old=""
    rev="$(source_rev)"
    if [[ -x "$BIN_DIR/cmd" && -f "$ZSH_FN" ]]; then
        [[ -f "$STAMP" ]] && old="$(< "$STAMP")"
        if [[ -n "$rev" && "$rev" == "$old" && "$FORCE" != 1 ]]; then
            ok "cmd $rev is already installed and up to date (--force rebuilds it anyway)"
            finish "already jacked in."
            return
        fi
        step "updating cmd ${old:-(earlier build)} → ${rev:-local build}"
    else
        step "installing cmd ${rev:-(local build)}"
    fi
    step "compiling (this takes a minute the first time)"
    ensure_cc
    (cd "$SOURCE" && cargo build --release --locked --quiet) || die "The build failed. The compiler output above says why."
    ok "built target/release/cmd"
    step "installing"
    install -Dm755 "$SOURCE/target/release/cmd" "$BIN_DIR/cmd"
    ok "installed $BIN_DIR/cmd"
    install_zsh_fn
    wire_zsh
    mkdir -p "$DATA_DIR"
    if [[ -n "$rev" ]]; then
        printf '%s\n' "$rev" > "$STAMP"
    else
        rm -f "$STAMP"
    fi
    finish "jacked in."
}

uninstall_cmd() {
    step "uninstalling"
    if [[ -e "$BIN_DIR/cmd" ]]; then
        rm -f "$BIN_DIR/cmd"
        ok "removed $BIN_DIR/cmd"
    else
        ok "cmd was not installed in $BIN_DIR"
    fi
    unwire_zsh
    if [[ -d "$DATA_DIR" ]]; then
        rm -rf "$DATA_DIR"
        ok "removed $DATA_DIR (zsh helper, downloaded source, install record)"
    fi
    ok "left ~/.grok/auth.json alone"
}

main() {
    local mode=install
    for arg in "$@"; do
        case "$arg" in
            --uninstall) mode=uninstall ;;
            --force) FORCE=1 ;;
            -h | --help)
                sed -n '2,16p' "${BASH_SOURCE[0]:-/dev/null}" 2> /dev/null | sed 's/^# \{0,1\}//'
                exit 0
                ;;
            *) die "unknown option: $arg (try --help)" ;;
        esac
    done
    if [[ "$mode" == uninstall ]]; then
        uninstall_cmd
    else
        install_cmd
    fi
}

if [[ -n ${BASH_SOURCE[0]:-} && ${BASH_SOURCE[0]} != "$0" ]]; then
    :
else
    main "$@"
fi
