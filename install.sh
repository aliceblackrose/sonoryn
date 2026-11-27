#!/usr/bin/env bash
# Sonoryn Linux installer: https://github.com/aliceblackrose/sonoryn
set -Eeuo pipefail

REPO=https://github.com/aliceblackrose/sonoryn.git
BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/lib/sonoryn"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/sonoryn"
CONFIG_FILE="$CONFIG_DIR/.env"
SKIP_DEPS=0

log() { printf '\n[sonoryn] %s\n' "$*"; }
die() { printf '[sonoryn] Error: %s\n' "$*" >&2; exit 1; }
has() { command -v "$1" >/dev/null 2>&1; }
usage() {
    printf 'Usage: bash install.sh [--skip-deps]\n'
    printf '  --skip-deps  Use already-installed system dependencies.\n'
}

case "${1:-}" in
    '') ;;
    --skip-deps) SKIP_DEPS=1 ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
esac
(( $# <= 1 )) || die 'Too many arguments.'
[[ $(uname -s) == Linux ]] || die 'This installer is for Linux only.'
(( EUID != 0 )) || die 'Run as a normal user, not with sudo.'

trap 'printf "[sonoryn] Installation failed at line %s.\n" "$LINENO" >&2' ERR
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$BIN_DIR:$PATH"

if (( ! SKIP_DEPS )); then
    has sudo || die 'sudo is required for system packages; or use --skip-deps.'
    log 'Installing system dependencies...'
    if has apt-get; then
        sudo apt-get update
        sudo apt-get install -y git curl ca-certificates build-essential \
            pkg-config cmake libssl-dev ffmpeg yt-dlp
    elif has dnf; then
        sudo dnf install -y git curl ca-certificates gcc gcc-c++ make \
            pkgconf-pkg-config cmake openssl-devel ffmpeg-free yt-dlp
    elif has pacman; then
        sudo pacman -S --needed base-devel git curl ca-certificates \
            pkgconf cmake openssl ffmpeg yt-dlp
    else
        die 'Unsupported package manager. Install git, curl, a C/C++ toolchain, pkg-config, cmake, OpenSSL headers, FFmpeg with libopus, and yt-dlp; then rerun with --skip-deps.'
    fi
fi

for tool in git curl cc c++ pkg-config ffmpeg yt-dlp install; do
    has "$tool" || die "Missing dependency: $tool"
done

log 'Checking audio runtime dependencies...'
yt-dlp --version
ffmpeg -version >/dev/null
encoders=$(ffmpeg -hide_banner -encoders 2>&1)
if ! grep -Eq '[[:space:]]libopus[[:space:]]' <<< "$encoders"; then
    die 'FFmpeg lacks the libopus encoder. Install a build with libopus and rerun with --skip-deps.'
fi

work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT

if ! has rustup; then
    log 'Installing Rustup...'
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
        -o "$work_dir/rustup-init.sh"
    sh "$work_dir/rustup-init.sh" -y --profile minimal \
        --default-toolchain none --no-modify-path
fi
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
has rustup || die 'Rustup was not found after installation.'

log 'Downloading Sonoryn source...'
git clone --depth 1 "$REPO" "$work_dir/source"
cd "$work_dir/source"

# Read the repository's pinned numeric Rust release rather than overriding it.
channel=$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([0-9]+\.[0-9]+\.[0-9]+)"[[:space:]]*$/\1/p' rust-toolchain.toml)
[[ -n "$channel" ]] || die 'Unable to read the pinned Rust release from rust-toolchain.toml.'
log "Installing Rust $channel..."
rustup toolchain install "$channel" --profile minimal

log 'Building Sonoryn (release mode)...'
# The repository currently has no Cargo.lock; use it if one is added later.
build_args=(build --release --target-dir "$work_dir/target")
[[ ! -f Cargo.lock ]] || build_args+=(--locked)
rustup run "$channel" cargo "${build_args[@]}"
[[ -x "$work_dir/target/release/sonoryn" ]] || die 'Compiled executable not found.'

log 'Installing Sonoryn for the current user...'
install -d "$BIN_DIR" "$APP_DIR"
install -d -m 700 "$CONFIG_DIR"

# Use a separate launcher because Sonoryn reads environment variables, not .env files.
cat > "$work_dir/launcher" <<'LAUNCHER'
#!/usr/bin/env bash
set -euo pipefail
config="${XDG_CONFIG_HOME:-$HOME/.config}/sonoryn/.env"
export PATH="$HOME/.local/bin:$PATH"
if [[ -f "$config" ]]; then
    # This is a trusted, user-owned shell configuration file.
    set -a
    source "$config"
    set +a
fi
if [[ -z "${DISCORD_TOKEN:-}" ]]; then
    printf 'Sonoryn: set DISCORD_TOKEN in %s before starting.\n' "$config" >&2
    exit 1
fi
exec "$HOME/.local/lib/sonoryn/sonoryn" "$@"
LAUNCHER

if [[ ! -e "$CONFIG_FILE" && ! -L "$CONFIG_FILE" ]]; then
    (umask 077; set -o noclobber; cat > "$CONFIG_FILE" <<'CONFIG'
# Sonoryn configuration (shell syntax). Keep this file private.
DISCORD_TOKEN="${DISCORD_TOKEN:-}"
# SONORYN_DEV_GUILD_ID="123456789012345678"
RUST_LOG="sonoryn=info,gloamwire=info,gloam_commands=info"
CONFIG
    )
    log "Created configuration: $CONFIG_FILE"
else
    log "Preserving existing configuration: $CONFIG_FILE"
fi

# Replace via rename so a currently running executable is not overwritten in place.
install -m 755 "$work_dir/target/release/sonoryn" "$APP_DIR/.sonoryn.new.$$"
mv -f -- "$APP_DIR/.sonoryn.new.$$" "$APP_DIR/sonoryn"
install -m 755 "$work_dir/launcher" "$BIN_DIR/.sonoryn.new.$$"
mv -f -- "$BIN_DIR/.sonoryn.new.$$" "$BIN_DIR/sonoryn"

log 'Installation complete.'
printf '  Edit your configuration: %q\n' "$CONFIG_FILE"
printf '  Start the bot: %q\n' "$BIN_DIR/sonoryn"
printf '  If sonoryn is not found, add ~/.local/bin to your shell PATH.\n'
printf '  To update, rerun this installer.\n'
