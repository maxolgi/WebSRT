#!/usr/bin/env bash
# install-prereqs.sh — install WebSRT's toolchain prerequisites.
#
# Installs: build tools (build-essential/cmake/pkg-config), Rust (via rustup,
# user-local), the wasm32-unknown-unknown target, wasm-pack, Node.js >= 18,
# and ffmpeg. Idempotent: every component is checked before install, so
# re-runs are fast and safe.
#
# Supports: Debian/Ubuntu, Fedora/RHEL, Arch, macOS (Homebrew).
# After running this, run ./build.sh setup to build the WASM modules + npm install.
#
# Can be curl'd directly:
#   curl -sSf https://raw.githubusercontent.com/maxolgi/WebSRT/master/install-prereqs.sh | bash
# (when piped, the script will not clone the repo for you — do that next)

set -euo pipefail

# --- Color helpers -----------------------------------------------------------

if [[ -t 2 && "${NO_COLOR:-}" == "" && "${INSTALL_PREREQS_NO_COLOR:-}" == "" ]]; then
    C_INFO=$'\033[1;34m'   # blue
    C_OK=$'\033[1;32m'     # green
    C_WARN=$'\033[1;33m'   # yellow
    C_ERR=$'\033[1;31m'    # red
    C_RST=$'\033[0m'
else
    C_INFO="" C_OK="" C_WARN="" C_ERR="" C_RST=""
fi

info() { printf '%s==%s %s\n' "$C_INFO" "$C_RST" "$*" >&2; }
ok()   { printf '%s✓%s %s\n'  "$C_OK"   "$C_RST" "$*" >&2; }
warn() { printf '%s!%s %s\n'  "$C_WARN" "$C_RST" "$*" >&2; }
err()  { printf '%s✗%s %s\n'  "$C_ERR"  "$C_RST" "$*" >&2; }

# --- Constants ---------------------------------------------------------------

MIN_NODE_MAJOR=18
MIN_RUST_MAJOR=1
MIN_RUST_MINOR=75
NODESOURCE_MAJOR=20   # Node.js major version installed via NodeSource on Linux

# --- Usage -------------------------------------------------------------------

usage() {
    cat <<'EOF' >&2
install-prereqs.sh — install WebSRT toolchain

Usage: ./install-prereqs.sh [--check] [--no-color] [-y]

  --check     Verify prerequisites without installing anything. Exits 0 if
              everything is present and recent enough; exits 1 otherwise.
  -y, --yes   Assume "yes" for any confirmation prompt (e.g. for apt).
  --no-color  Disable ANSI colors (also honored via NO_COLOR=1).
  -h, --help  Show this help.

What gets installed:
  - build-essential / cmake / pkg-config / curl / git  (system)
  - ffmpeg                                             (system)
  - rustup + stable Rust toolchain >= 1.75              (user-local, ~/.cargo)
  - wasm32-unknown-unknown rustc target
  - wasm-pack                                          (user-local, ~/.cargo)
  - Node.js >= 18                                      (system)

Supported platforms: Debian/Ubuntu, Fedora/RHEL, Arch, macOS (Homebrew).
EOF
}

# --- Argument parsing --------------------------------------------------------

CHECK_ONLY=0
ASSUME_YES=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)    CHECK_ONLY=1; shift ;;
        -y|--yes)   ASSUME_YES=1; shift ;;
        --no-color) export INSTALL_PREREQS_NO_COLOR=1; shift ;;
        -h|--help)  usage; exit 0 ;;
        *) err "unknown arg: $1"; usage; exit 2 ;;
    esac
done

# --- Helpers ----------------------------------------------------------------#

have() { command -v "$1" >/dev/null 2>&1; }

# Extract the leading "<major>.<minor>" from a version string.
normalize_ver() {
    # strip non-digits, take first two dot-separated fields
    echo "$1" | sed -E 's/^[^0-9]*//; s/[^0-9.].*$//' | cut -d. -f1-2
}
ver_major() { echo "$1" | cut -d. -f1; }
ver_minor() { echo "$1" | cut -d. -f2; }

# Return 0 if $1 (major.minor) >= $2 (major.minor), else 1.
ver_ge() {
    local a="$1" b="$2"
    local am aj bm bj
    am=$(ver_major "$a"); aj=$(ver_minor "$a")
    bm=$(ver_major "$b"); bj=$(ver_minor "$b")
    am=${am:-0}; aj=${aj:-0}; bm=${bm:-0}; bj=${bj:-0}
    if [[ $am -gt $bm ]]; then return 0; fi
    if [[ $am -lt $bm ]]; then return 1; fi
    [[ $aj -ge $bj ]]
}

sudo_maybe() {
    if [[ $EUID -eq 0 ]]; then "$@"; else sudo "$@"; fi
}

# Run a command. In --check mode, return 1 instead of running it.
maybe_install() {
    if [[ $CHECK_ONLY -eq 1 ]]; then
        return 1
    fi
    "$@"
}

# --- Platform detection ------------------------------------------------------

detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux)  os="linux" ;;
        Darwin) os="macos" ;;
        *) err "unsupported OS: $os (supported: Linux, macOS)"; exit 1 ;;
    esac
    case "$arch" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) err "unsupported arch: $arch"; exit 1 ;;
    esac
    PLATFORM_OS="$os"
    PLATFORM_ARCH="$arch"
    DISTRO=""

    if [[ "$os" == "linux" ]]; then
        if [[ -f /etc/debian_version ]]; then
            DISTRO="debian"
        elif [[ -f /etc/fedora-release ]]; then
            DISTRO="fedora"
        elif [[ -f /etc/centos-release ]] || [[ -f /etc/rhel-release ]]; then
            DISTRO="fedora"   # treat CentOS/RHEL as fedora-family
        elif [[ -f /etc/arch-release ]]; then
            DISTRO="arch"
        else
            err "unsupported Linux distro: no /etc/{debian_version,fedora-release,arch-release}"
            err "supported: Debian/Ubuntu, Fedora/CentOS/RHEL, Arch"
            exit 1
        fi
    fi
}

# --- Component checkers / installers -----------------------------------------

check_system_pkgs() {
    # Returns 0 if all needed system binaries are present.
    local missing=0
    for tool in cc c++ make cmake pkg-config git curl ffmpeg; do
        if ! have "$tool"; then
            warn "missing: $tool"
            missing=1
        fi
    done
    return $missing
}

install_system_pkgs() {
    info "installing system build tools + ffmpeg"
    case "$PLATFORM_OS-$DISTRO" in
        linux-debian)
            sudo_maybe apt-get update
            sudo_maybe apt-get install -y \
                build-essential cmake pkg-config curl git ffmpeg
            ;;
        linux-fedora)
            sudo_maybe dnf install -y \
                gcc gcc-c++ make cmake pkgconf-pkg-config curl git ffmpeg
            ;;
        linux-arch)
            sudo_maybe pacman -Sy --noconfirm \
                base-devel cmake pkgconf curl git ffmpeg
            ;;
        macos-)
            if ! have brew; then
                err "Homebrew not installed. Install it from https://brew.sh, then rerun."
                exit 1
            fi
            brew install cmake pkg-config ffmpeg
            ;;
        *)
            err "don't know how to install system packages on $PLATFORM_OS/$DISTRO"
            return 1
            ;;
    esac
    ok "system packages installed"
}

# Source cargo env if it exists (so rustc/cargo are on PATH after fresh rustup install).
source_cargo_env() {
    if [[ -f "$HOME/.cargo/env" ]]; then
        # shellcheck disable=SC1091
        source "$HOME/.cargo/env"
    fi
}

check_rust() {
    if ! have rustc; then return 1; fi
    local ver; ver=$(rustc --version 2>/dev/null | awk '{print $2}')
    ver=$(normalize_ver "$ver")
    ver_ge "$ver" "$MIN_RUST_MAJOR.$MIN_RUST_MINOR"
}

install_rust() {
    if have rustc && ! have rustup; then
        warn "rustc is installed but rustup is not — cannot manage toolchain."
        warn "if you installed rustc via a package manager, uninstall it first,"
        warn "or install rustup manually from https://rustup.rs and rerun."
        return 1
    fi
    if have rustc; then
        local ver; ver=$(rustc --version | awk '{print $2}')
        ver=$(normalize_ver "$ver")
        if ver_ge "$ver" "$MIN_RUST_MAJOR.$MIN_RUST_MINOR"; then
            ok "rustc $ver already installed"
            return 0
        fi
        warn "rustc $ver is older than $MIN_RUST_MAJOR.$MIN_RUST_MINOR; upgrading via rustup"
        rustup update stable
        return 0
    fi
    info "installing rustup + stable Rust toolchain (user-local, ~/.cargo)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal
    source_cargo_env
    if ! have rustc; then
        err "rustup install finished but rustc is not on PATH"
        err "open a new shell (or 'source ~/.cargo/env') and rerun"
        return 1
    fi
    ok "rust $(rustc --version | awk '{print $2}') installed"
}

check_wasm_target() {
    rustup target list --installed 2>/dev/null | grep -q '^wasm32-unknown-unknown$'
}

install_wasm_target() {
    info "adding wasm32-unknown-unknown rustc target"
    rustup target add wasm32-unknown-unknown
    ok "wasm32 target installed"
}

check_wasm_pack() {
    have wasm-pack
}

install_wasm_pack() {
    if check_wasm_pack; then
        ok "wasm-pack $(wasm-pack --version 2>/dev/null | awk '{print $2}') already installed"
        return 0
    fi
    info "installing wasm-pack (user-local, ~/.cargo/bin)"
    curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
    source_cargo_env
    if ! check_wasm_pack; then
        err "wasm-pack install finished but it's not on PATH"
        err "open a new shell (or 'source ~/.cargo/env') and rerun"
        return 1
    fi
    ok "wasm-pack installed"
}

check_node() {
    if ! have node; then return 1; fi
    local major; major=$(node --version | sed 's/^v//' | cut -d. -f1)
    [[ ${major:-0} -ge $MIN_NODE_MAJOR ]]
}

install_node() {
    if check_node; then
        ok "node $(node --version) already installed"
        return 0
    fi

    if have node; then
        warn "node $(node --version) is older than v$MIN_NODE_MAJOR; upgrading"
    fi

    case "$PLATFORM_OS-$DISTRO" in
        linux-debian)
            info "installing Node.js v$NODESOURCE_MAJOR via NodeSource (apt)"
            curl -fsSL "https://deb.nodesource.com/setup_${NODESOURCE_MAJOR}.x" \
                | sudo_maybe -E bash -
            sudo_maybe apt-get install -y nodejs
            ;;
        linux-fedora)
            info "installing Node.js v$NODESOURCE_MAJOR via NodeSource (dnf)"
            curl -fsSL "https://rpm.nodesource.com/setup_${NODESOURCE_MAJOR}.x" \
                | sudo_maybe -E bash -
            sudo_maybe dnf install -y nodejs
            ;;
        linux-arch)
            info "installing Node.js (pacman)"
            sudo_maybe pacman -Sy --noconfirm nodejs npm
            ;;
        macos-)
            if ! have brew; then
                err "Homebrew not installed. Install it from https://brew.sh, then rerun."
                return 1
            fi
            brew install node
            ;;
        *)
            err "don't know how to install Node.js on $PLATFORM_OS/$DISTRO"
            return 1
            ;;
    esac
    ok "node $(node --version) installed"
}

# --- Main --------------------------------------------------------------------

print_summary() {
    echo "" >&2
    info "summary:"
    local fail_count=0

    if have cc && have cmake && have pkg-config && have ffmpeg; then
        ok "system build tools + ffmpeg"
    else
        err "system build tools + ffmpeg"
        fail_count=$((fail_count + 1))
    fi

    if check_rust; then
        ok "rust     $(rustc --version | awk '{print $2}')"
    else
        err "rust     missing or too old (need >= $MIN_RUST_MAJOR.$MIN_RUST_MINOR)"
        fail_count=$((fail_count + 1))
    fi

    if check_wasm_target; then
        ok "wasm32-unknown-unknown target"
    else
        err "wasm32-unknown-unknown target missing"
        fail_count=$((fail_count + 1))
    fi

    if check_wasm_pack; then
        ok "wasm-pack $(wasm-pack --version | awk '{print $2}')"
    else
        err "wasm-pack missing"
        fail_count=$((fail_count + 1))
    fi

    if check_node; then
        ok "node     $(node --version)"
    else
        err "node     missing or too old (need >= v$MIN_NODE_MAJOR)"
        fail_count=$((fail_count + 1))
    fi

    echo "" >&2
    if [[ $fail_count -eq 0 ]]; then
        ok "all prerequisites satisfied."
        if [[ $CHECK_ONLY -eq 0 ]]; then
            echo "  next steps:" >&2
            echo "    ./build.sh setup    # build WASM modules + npm install" >&2
            echo "    ./build.sh gateway  # build the demo gateway binary" >&2
            echo "    ./build.sh web      # start the Vite dev server" >&2
        fi
        return 0
    else
        if [[ $CHECK_ONLY -eq 1 ]]; then
            err "one or more prerequisites missing or too old (re-run without --check to install)"
        else
            err "one or more prerequisites could not be installed automatically"
        fi
        return 1
    fi
}

main() {
    detect_platform
    info "platform: $PLATFORM_OS/$PLATFORM_ARCH${DISTRO:+ ($DISTRO)}"

    if [[ $CHECK_ONLY -eq 1 ]]; then
        info "check-only mode — no installations will be performed"
        source_cargo_env  # pick up any existing ~/.cargo/env
        print_summary
        return $?
    fi

    # 1. System packages (build tools + ffmpeg)
    if ! check_system_pkgs; then
        install_system_pkgs
    else
        ok "system build tools + ffmpeg already installed"
    fi

    # 2. Rust toolchain (rustup is the source of truth for rustc/cargo)
    source_cargo_env
    if ! check_rust; then
        install_rust
    else
        ok "rustc $(rustc --version | awk '{print $2}') already installed"
    fi

    # 3. wasm32 target
    if ! check_wasm_target; then
        install_wasm_target
    else
        ok "wasm32-unknown-unknown target already installed"
    fi

    # 4. wasm-pack
    if ! check_wasm_pack; then
        install_wasm_pack
    else
        ok "wasm-pack $(wasm-pack --version | awk '{print $2}') already installed"
    fi

    # 5. Node.js
    if ! check_node; then
        install_node
    fi

    print_summary
}

main "$@"
