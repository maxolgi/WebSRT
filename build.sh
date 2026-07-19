#!/usr/bin/env bash
# build.sh — orchestrator for WebSRT WASM, gateway, library, and web builds.
#
# Wraps the snippets in AGENTS.md "Build commands" so the wasm→web/wasm copy
# cycle, the forked srt-protocol two-half rebuild, and the supervisord restart
# after library changes become single commands. Run `./build.sh --help` for
# the menu.
#
# Raw form of every step lives in AGENTS.md; this script is a convenience
# wrapper that does not introduce any new behavior.

set -euo pipefail

# Resolve repo root from this script's location so it works from any CWD.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

# --- Color helpers -----------------------------------------------------------

if [[ -t 2 && "${NO_COLOR:-}" == "" && "${BUILD_SH_NO_COLOR:-}" == "" ]]; then
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

# --- Usage -------------------------------------------------------------------

usage() {
    cat <<'EOF' >&2
build.sh — WebSRT build orchestrator

Usage: ./build.sh [GLOBAL FLAGS] COMMAND [COMMAND FLAGS] [ARGS]

Global flags (must precede the command):
  -h, --help        Show this help
  -v, --verbose     Enable bash tracing (set -x) for full visibility
      --no-color    Disable ANSI colors (also honored via NO_COLOR=1)

Commands:

  setup [--no-npm-install] [--no-wasm]
          First-time bootstrap: mkdir web/wasm/<crate>, build all 3 WASM
          crates, then npm install in web/.

  wasm [crate] [--debug]
          Build + copy one or all WASM crates.
          crate: srt | mpeg2ts | ts-muxer  (default: all three)
          --debug: build with --dev instead of --release

  gateway [--sim-loss] [--debug]
          cargo build the demo gateway binary (release by default).

  lib [--debug]
          cargo build the websrt library crate (release by default).

  web [mode]
          Run the Vite dev/build/preview script.
          mode: dev | build | preview  (default: dev)

  check [--no-cargo] [--no-tsc]
          cargo check --workspace + npx tsc --noEmit.

  test [--no-cargo] [--no-smoke]
          cargo test --workspace + node web/smoke.mjs.
          Note: smoke test reads crates/<crate>/pkg/, so it requires
          `./build.sh wasm` (or setup) to have been run first.

  srt-protocol [--sim-loss] [--debug]
          Rule 1 helper (AGENTS.md "Critical build order"): rebuild BOTH
          the gateway binary AND srt-wasm + copy after editing the forked
          maxolgi/srt-rs crate.

  restart
          sudo supervisorctl restart websrt. Prints the config and log
          paths first so you can Ctrl-C on a machine without supervisord.

  all [--sim-loss] [--debug]
          Full clean rebuild: clean -y → setup → gateway → web build.

  clean [--keep-target] [-y]
          rm -rf web/wasm web/dist. Also rm -rf target unless --keep-target.
          -y: skip the confirmation prompt (for non-interactive use).

Raw form of every step lives in AGENTS.md "Build commands"; this script
is a convenience wrapper that does not introduce any new behavior.
EOF
}

# --- Constants ---------------------------------------------------------------

WASM_CRATES=(srt-wasm mpeg2ts-wasm ts-muxer-wasm)

# --- WASM build helper -------------------------------------------------------

# build_one_wasm <crate-name>  (reads ARG_DEBUG=0|1)
build_one_wasm() {
    local crate="$1"
    local profile_flag=(--release)
    if [[ "${ARG_DEBUG:-0}" == "1" ]]; then
        profile_flag=(--dev)
    fi
    info "wasm-pack build $crate (${profile_flag[*]})"
    (cd "crates/$crate" && wasm-pack build --target web "${profile_flag[@]}")
    mkdir -p "web/wasm/$crate"
    cp -f "crates/$crate/pkg/"* "web/wasm/$crate/"
    ok "wasm $crate → web/wasm/$crate"
}

# --- Subcommands -------------------------------------------------------------

cmd_setup() {
    local do_npm=1
    local do_wasm=1
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --no-npm-install) do_npm=0; shift ;;
            --no-wasm) do_wasm=0; shift ;;
            -h|--help)
                cat <<'EOF' >&2
setup — first-time bootstrap
Usage: ./build.sh setup [--no-npm-install] [--no-wasm]
EOF
                return 0 ;;
            *) err "setup: unknown arg: $1"; return 2 ;;
        esac
    done

    for crate in "${WASM_CRATES[@]}"; do
        mkdir -p "web/wasm/$crate"
    done
    if [[ $do_wasm -eq 1 ]]; then
        for crate in "${WASM_CRATES[@]}"; do
            build_one_wasm "$crate"
        done
    else
        info "skipping WASM builds (--no-wasm)"
    fi
    if [[ $do_npm -eq 1 ]]; then
        info "npm install (web/)"
        (cd web && npm install)
        ok "web deps installed"
    else
        info "skipping npm install (--no-npm-install)"
    fi
    ok "setup complete"
}

cmd_wasm() {
    local crate=""
    ARG_DEBUG=0
    while [[ $# -gt 0 ]]; do
        case "$1" in
            srt|srt-wasm) crate="srt-wasm"; shift ;;
            mpeg2ts|mpeg2ts-wasm) crate="mpeg2ts-wasm"; shift ;;
            ts-muxer|ts-muxer-wasm) crate="ts-muxer-wasm"; shift ;;
            --debug) ARG_DEBUG=1; shift ;;
            -h|--help)
                cat <<'EOF' >&2
wasm — build + copy WASM crates
Usage: ./build.sh wasm [crate] [--debug]
  crate: srt | mpeg2ts | ts-muxer  (default: all three)
EOF
                return 0 ;;
            *) err "wasm: unknown arg: $1"; return 2 ;;
        esac
    done

    if [[ -z "$crate" ]]; then
        for c in "${WASM_CRATES[@]}"; do build_one_wasm "$c"; done
    else
        build_one_wasm "$crate"
    fi
}

cmd_gateway() {
    local features=()
    local profile_args=(--release)
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --sim-loss) features+=("sim-loss"); shift ;;
            --debug) profile_args=(--profile dev); shift ;;
            -h|--help)
                cat <<'EOF' >&2
gateway — build the demo gateway binary
Usage: ./build.sh gateway [--sim-loss] [--debug]
EOF
                return 0 ;;
            *) err "gateway: unknown arg: $1"; return 2 ;;
        esac
    done
    local feature_args=()
    if [[ ${#features[@]} -gt 0 ]]; then
        feature_args=(--features "${features[*]}")
    fi
    info "cargo build ${profile_args[*]} -p websrt-gateway ${features[*]:-}"
    cargo build "${profile_args[@]}" -p websrt-gateway "${feature_args[@]+"${feature_args[@]}"}"
    ok "gateway built"
}

cmd_lib() {
    local profile_args=(--release)
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --debug) profile_args=(--profile dev); shift ;;
            -h|--help)
                cat <<'EOF' >&2
lib — build the websrt library crate
Usage: ./build.sh lib [--debug]
EOF
                return 0 ;;
            *) err "lib: unknown arg: $1"; return 2 ;;
        esac
    done
    info "cargo build ${profile_args[*]} -p websrt"
    cargo build "${profile_args[@]}" -p websrt
    ok "lib built"
}

cmd_web() {
    local mode="dev"
    while [[ $# -gt 0 ]]; do
        case "$1" in
            dev|build|preview) mode="$1"; shift ;;
            -h|--help)
                cat <<'EOF' >&2
web — run the Vite dev/build/preview script
Usage: ./build.sh web [dev|build|preview]  (default: dev)
EOF
                return 0 ;;
            *) err "web: unknown arg: $1"; return 2 ;;
        esac
    done
    info "npm run $mode (web/)"
    (cd web && npm run "$mode")
}

cmd_check() {
    local do_cargo=1
    local do_tsc=1
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --no-cargo) do_cargo=0; shift ;;
            --no-tsc) do_tsc=0; shift ;;
            -h|--help)
                cat <<'EOF' >&2
check — cargo check + tsc --noEmit
Usage: ./build.sh check [--no-cargo] [--no-tsc]
EOF
                return 0 ;;
            *) err "check: unknown arg: $1"; return 2 ;;
        esac
    done
    if [[ $do_cargo -eq 1 ]]; then
        info "cargo check --workspace"
        cargo check --workspace
        ok "cargo check"
    fi
    if [[ $do_tsc -eq 1 ]]; then
        info "tsc --noEmit (web/)"
        (cd web && npx tsc --noEmit)
        ok "tsc"
    fi
}

cmd_test() {
    local do_cargo=1
    local do_smoke=1
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --no-cargo) do_cargo=0; shift ;;
            --no-smoke) do_smoke=0; shift ;;
            -h|--help)
                cat <<'EOF' >&2
test — cargo test + node smoke
Usage: ./build.sh test [--no-cargo] [--no-smoke]
EOF
                return 0 ;;
            *) err "test: unknown arg: $1"; return 2 ;;
        esac
    done
    if [[ $do_cargo -eq 1 ]]; then
        info "cargo test --workspace"
        cargo test --workspace
        ok "cargo test"
    fi
    if [[ $do_smoke -eq 1 ]]; then
        if [[ ! -d crates/srt-wasm/pkg || ! -d crates/mpeg2ts-wasm/pkg ]]; then
            warn "crates/<wasm>/pkg/ missing — run './build.sh wasm' first; skipping smoke"
        elif [[ ! -f web/smoke.mjs ]]; then
            warn "web/smoke.mjs not found; skipping smoke"
        else
            info "node web/smoke.mjs"
            node web/smoke.mjs
            ok "smoke"
        fi
    fi
}

cmd_srt_protocol() {
    local pass_args=()
    local want_debug=0
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --sim-loss) pass_args+=("$1"); shift ;;
            --debug) pass_args+=("$1"); want_debug=1; shift ;;
            -h|--help)
                cat <<'EOF' >&2
srt-protocol — rebuild gateway + srt-wasm after editing forked srt-rs
Usage: ./build.sh srt-protocol [--sim-loss] [--debug]
Forwards flags to the gateway build; --debug also affects srt-wasm.
Implements AGENTS.md "Critical build order" rule 1.
EOF
                return 0 ;;
            *) err "srt-protocol: unknown arg: $1"; return 2 ;;
        esac
    done
    info "srt-protocol rule: rebuilding gateway + srt-wasm"
    cmd_gateway "${pass_args[@]+"${pass_args[@]}"}"
    ARG_DEBUG=$want_debug
    build_one_wasm "srt-wasm"
    ok "srt-protocol rebuild complete"
}

cmd_restart() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help)
                cat <<'EOF' >&2
restart — sudo supervisorctl restart websrt
Usage: ./build.sh restart
EOF
                return 0 ;;
            *) err "restart: unknown arg: $1"; return 2 ;;
        esac
    done
    if [[ ! -f websrt.conf ]]; then
        err "websrt.conf not found at repo root — is this the production host?"
        return 2
    fi
    info "config: $(pwd)/websrt.conf"
    info "logs:   logs/gateway.out.log, logs/gateway.err.log"
    info "running: sudo supervisorctl restart websrt"
    sudo supervisorctl restart websrt
    ok "websrt restarted"
}

cmd_clean() {
    local keep_target=0
    local assume_yes=0
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --keep-target) keep_target=1; shift ;;
            -y|--yes) assume_yes=1; shift ;;
            -h|--help)
                cat <<'EOF' >&2
clean — remove build outputs
Usage: ./build.sh clean [--keep-target] [-y]
Always removes web/wasm/ and web/dist/.
Removes target/ unless --keep-target is set.
EOF
                return 0 ;;
            *) err "clean: unknown arg: $1"; return 2 ;;
        esac
    done

    local targets=("web/wasm" "web/dist")
    if [[ $keep_target -eq 0 ]]; then
        targets+=("target")
    fi

    if [[ $assume_yes -eq 0 ]]; then
        warn "about to remove: ${targets[*]}"
        read -r -p "proceed? [y/N] " reply
        case "$reply" in
            y|Y|yes|YES) ;;
            *) info "aborted"; return 1 ;;
        esac
    fi

    for t in "${targets[@]}"; do
        if [[ -e "$t" ]]; then
            info "rm -rf $t"
            rm -rf "$t"
        fi
    done
    ok "clean"
}

cmd_all() {
    local all_args=()
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --sim-loss|--debug) all_args+=("$1"); shift ;;
            -h|--help)
                cat <<'EOF' >&2
all — full clean rebuild
Usage: ./build.sh all [--sim-loss] [--debug]
Runs: clean -y → setup → gateway → web build
EOF
                return 0 ;;
            *) err "all: unknown arg: $1"; return 2 ;;
        esac
    done

    cmd_clean -y
    cmd_setup
    cmd_gateway "${all_args[@]+"${all_args[@]}"}"
    cmd_web build
    ok "all complete"
}

# --- Global flag parsing -----------------------------------------------------

# Parse leading global flags until we hit the subcommand.
while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        -v|--verbose) shift; set -x ;;
        --no-color) export BUILD_SH_NO_COLOR=1; shift ;;
        *) break ;;
    esac
done

if [[ $# -eq 0 ]]; then
    usage
    exit 1
fi

CMD="$1"
shift

# --- Dispatcher --------------------------------------------------------------

case "$CMD" in
    setup)         cmd_setup "$@" ;;
    wasm)          cmd_wasm "$@" ;;
    gateway)       cmd_gateway "$@" ;;
    lib)           cmd_lib "$@" ;;
    web)           cmd_web "$@" ;;
    check)         cmd_check "$@" ;;
    test)          cmd_test "$@" ;;
    srt-protocol)  cmd_srt_protocol "$@" ;;
    restart)       cmd_restart "$@" ;;
    clean)         cmd_clean "$@" ;;
    all)           cmd_all "$@" ;;
    *)
        err "unknown command: $CMD"
        usage
        exit 1
        ;;
esac
