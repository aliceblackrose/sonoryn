#!/usr/bin/env bash
# Sonoryn Linux updater: https://github.com/aliceblackrose/sonoryn
set -Eeuo pipefail

REPO=https://github.com/aliceblackrose/sonoryn.git
APP_DIR="$HOME/.local/lib/sonoryn"
BINARY="$APP_DIR/sonoryn"
SERVICE=sonoryn.service
RESTART=1

log() { printf '\n[sonoryn] %s\n' "$*"; }
die() { printf '[sonoryn] Error: %s\n' "$*" >&2; exit 1; }

case "${1:-}" in
    '') ;;
    --no-restart) RESTART=0 ;;
    --help|-h)
        printf 'Usage: bash update.sh [--no-restart]\n'
        printf '  --no-restart  Update the binary without restarting the service.\n'
        exit 0 ;;
    *) die 'Unknown option. Use --help.' ;;
esac
(( $# <= 1 )) || die 'Too many arguments.'
[[ $(uname -s) == Linux ]] || die 'Linux is required.'
(( EUID != 0 )) || die 'Run as the same normal user who installed Sonoryn, not sudo.'
[[ -x "$BINARY" && -x "$HOME/.local/bin/sonoryn" ]] || die 'Install Sonoryn with install.sh first.'
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$HOME/.local/bin:$PATH"
for tool in git rustup install flock cc c++ pkg-config ffmpeg; do
    command -v "$tool" >/dev/null || die "Missing dependency: $tool. Rerun install.sh."
done

# Serialize concurrent runs of this updater.
exec 9>"$APP_DIR/.update.lock"
flock -n 9 || die 'Another update is already running.'
work_dir=$(mktemp -d)
staged=$(mktemp "$APP_DIR/.sonoryn.new.XXXXXX")
backup_stage="$staged.previous"
cleanup() { rm -rf -- "$work_dir"; rm -f -- "$staged" "$backup_stage"; }
trap cleanup EXIT
trap 'printf "[sonoryn] Update failed at line %s.\n" "$LINENO" >&2' ERR

log 'Checking runtime dependencies...'
ffmpeg -version >/dev/null
encoders=$(ffmpeg -hide_banner -encoders 2>&1)
grep -Eq '[[:space:]]libopus[[:space:]]' <<< "$encoders" || die 'FFmpeg lacks libopus encoding support.'

log 'Fetching the latest main branch...'
git clone --depth 1 --branch main "$REPO" "$work_dir/source"
cd "$work_dir/source"
revision=$(git rev-parse HEAD)
channel=$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([0-9]+\.[0-9]+\.[0-9]+)"[[:space:]]*$/\1/p' rust-toolchain.toml)
[[ -n "$channel" ]] || die 'Cannot determine the pinned Rust toolchain.'
log "Preparing Rust $channel..."
rustup toolchain install "$channel" --profile minimal

log "Building ${revision:0:12}; the running bot is not interrupted..."
build_args=(build --release --target-dir "$work_dir/target")
[[ ! -f Cargo.lock ]] || build_args+=(--locked)
rustup run "$channel" cargo "${build_args[@]}"
[[ -x "$work_dir/target/release/sonoryn" ]] || die 'Build did not produce the executable.'

# Stage on the destination filesystem so the replacement is atomic.
install -m 755 "$work_dir/target/release/sonoryn" "$staged"
install -m 755 "$BINARY" "$backup_stage"
mv -f -- "$backup_stage" "$BINARY.previous"
mv -f -- "$staged" "$BINARY"
log "Installed commit ${revision:0:12}. Previous binary: $BINARY.previous"

# Do not start a deliberately stopped service or touch its unit/configuration.
if (( RESTART )) && command -v systemctl >/dev/null && systemctl --user is-active --quiet "$SERVICE"; then
    log 'Restarting the running Sonoryn service...'
    if ! systemctl --user restart "$SERVICE"; then
        die 'New binary installed, but restart failed. See journalctl --user -u sonoryn and the rollback instructions.'
    fi
    log 'Restart requested successfully. Check the logs to confirm Discord connectivity.'
else
    log 'No automatic restart performed. Restart Sonoryn manually when ready.'
fi
log 'Update complete. Existing launcher, Discord token and service configuration were preserved.'
printf '  Status: systemctl --user status sonoryn\n'
printf '  Logs:   journalctl --user -u sonoryn -f\n'
