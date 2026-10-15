#!/usr/bin/env bash
#
# Build piphone on the Raspberry Pi 5 and install it on the Pi Zero 2W.
#
#   scripts/deploy.sh [options]
#
# The Pi Zero 2W cannot compile the rvoip dependency tree, so the release build
# runs on the Pi 5 in /home/fox/piphone-build (see AGENTS.override.md) and the
# resulting binary is copied over. No backup of the previous binary is kept:
# the new file is staged next to the old one and put in place with an atomic
# rename, then the service is restarted. A failed build or a failing sanity
# check aborts before the running system is touched.
#
# Options:
#   --no-restart   Install the binary but leave the running service alone
#   --skip-build   Reinstall the binary already built on the Pi 5
#   --force        Deploy even though a call is in progress
#   -h, --help     Show this help

set -euo pipefail

REPO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
KEY="$REPO_DIR/.ssh/id_ed25519_piphone"
KNOWN_HOSTS="$REPO_DIR/.ssh/known_hosts"

BUILD_HOST="fox@pi5"
BUILD_DIR="/home/fox/piphone-build"
CARGO="/home/fox/.cargo/bin/cargo"

DEPLOY_HOST="fox@piphone"
REMOTE_BIN="/usr/local/bin/piphone"
STAGE_BIN="/usr/local/bin/.piphone.new"
SERVICE="piphone"

RESTART=1
BUILD=1
FORCE=0

usage() { awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"; }

step() { printf '\n== %s\n' "$*"; }
info() { printf '   %s\n' "$*"; }
warn() { printf '   warning: %s\n' "$*" >&2; }
die()  { printf '\nerror: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
	case "$1" in
		--no-restart) RESTART=0 ;;
		--skip-build) BUILD=0 ;;
		--force)      FORCE=1 ;;
		-h|--help)    usage; exit 0 ;;
		*)            die "unknown option '$1' (try --help)" ;;
	esac
	shift
done

SSH=(ssh -i "$KEY" -o UserKnownHostsFile="$KNOWN_HOSTS" -o BatchMode=yes -o ConnectTimeout=10)

[ -r "$KEY" ] || die "no SSH key at $KEY"
command -v rsync >/dev/null || die "rsync is required"

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

# ---------------------------------------------------------------- preflight

step "Preflight"
info "build host:   $BUILD_HOST:$BUILD_DIR"
info "deploy host:  $DEPLOY_HOST:$REMOTE_BIN"
"${SSH[@]}" "$BUILD_HOST" true || die "$BUILD_HOST is not reachable"
"${SSH[@]}" "$DEPLOY_HOST" true || die "$DEPLOY_HOST is not reachable"
if [ "$BUILD" -eq 1 ]; then
	"${SSH[@]}" "$BUILD_HOST" "test -x $CARGO" || die "no cargo at $CARGO on $BUILD_HOST"
fi

# Refuse to restart into an ongoing conversation unless asked to.
call_state() {
	"${SSH[@]}" "$DEPLOY_HOST" 'bash -s' <<-'REMOTE'
		set -eu
		cfg=/usr/local/etc/piphone/config.toml
		[ -r "$cfg" ] || exit 2
		sec=$(sed -n '/^\[web\]/,/^\[/p' "$cfg")
		port=$(printf '%s\n' "$sec" | sed -n 's/^[[:space:]]*port[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -1)
		user=$(printf '%s\n' "$sec" | sed -n 's/^[[:space:]]*user[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
		pass=$(printf '%s\n' "$sec" | sed -n 's/^[[:space:]]*pass[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
		[ -n "$port" ] && [ "$port" != "0" ] || exit 2
		set -- curl -s -m 5
		[ -n "$user" ] && set -- "$@" -u "$user:$pass"
		exec "$@" "http://127.0.0.1:$port/api/state"
	REMOTE
}

if state="$(call_state)" && [ -n "$state" ]; then
	if printf '%s' "$state" | grep -q '"call_active":true'; then
		[ "$FORCE" -eq 1 ] || die "a call is in progress on $DEPLOY_HOST; try again later or with --force"
		warn "a call is in progress, continuing because of --force"
	else
		info "no call in progress"
	fi
else
	warn "could not read the call state from $DEPLOY_HOST (web interface off or unreachable)"
fi

# --------------------------------------------------------------------- sync

step "Syncing sources to $BUILD_HOST"
rsync -az --delete \
	--exclude '/target/' --exclude '/node_modules/' --exclude '/.git/' \
	--exclude '.DS_Store' --exclude 'build.log' \
	-e "ssh -i $KEY -o UserKnownHostsFile=$KNOWN_HOSTS -o BatchMode=yes" \
	"$REPO_DIR/" "$BUILD_HOST:$BUILD_DIR/"
info "done"

# -------------------------------------------------------------------- build

BINARY="target/release/piphone"

if [ "$BUILD" -eq 1 ]; then
	step "Building release binary on $BUILD_HOST"
	"${SSH[@]}" "$BUILD_HOST" \
		"cd '$BUILD_DIR' && $CARGO build --release 2>&1 | tee build.log; exit \${PIPESTATUS[0]}" \
		|| die "cargo build --release failed on $BUILD_HOST (see $BUILD_DIR/build.log)"
else
	step "Skipping build, using the existing binary on $BUILD_HOST"
fi

step "Sanity check on $BUILD_HOST"
"${SSH[@]}" "$BUILD_HOST" "cd '$BUILD_DIR' && ./$BINARY --help >/dev/null" \
	|| die "the built binary does not run on $BUILD_HOST"
CHECKSUM="$("${SSH[@]}" "$BUILD_HOST" "sha256sum '$BUILD_DIR/$BINARY' | cut -d' ' -f1")"
info "sha256 $CHECKSUM"

# ------------------------------------------------------------------ transfer

step "Copying to $DEPLOY_HOST"
scp -i "$KEY" -o UserKnownHostsFile="$KNOWN_HOSTS" -o BatchMode=yes -q \
	"$BUILD_HOST:$BUILD_DIR/$BINARY" "$WORK_DIR/piphone" \
	|| die "could not fetch the binary from $BUILD_HOST"
info "fetched $(wc -c < "$WORK_DIR/piphone" | tr -d ' ') bytes"
chmod 0755 "$WORK_DIR/piphone"

# The binary is Linux aarch64 and cannot be run on the macOS build machine, so
# it is checked on the Pi 5 above and by checksum after the copy.

scp -i "$KEY" -o UserKnownHostsFile="$KNOWN_HOSTS" -o BatchMode=yes -q \
	"$WORK_DIR/piphone" "$DEPLOY_HOST:/tmp/piphone.new" || die "could not upload to $DEPLOY_HOST"

# Stage on the same filesystem first: install there, then rename into place.
# A plain mv from /tmp would copy over the running executable and fail with
# ETXTBSY, and the rename leaves the old process alive on its own inode until
# the restart below.
"${SSH[@]}" "$DEPLOY_HOST" \
	"sudo -n install -o root -g root -m 0755 /tmp/piphone.new '$STAGE_BIN' && rm -f /tmp/piphone.new" \
	|| die "could not stage the new binary on $DEPLOY_HOST"

GOT="$("${SSH[@]}" "$DEPLOY_HOST" "sudo -n sha256sum '$STAGE_BIN' | cut -d' ' -f1")"
[ "$GOT" = "$CHECKSUM" ] || die "checksum mismatch on $DEPLOY_HOST ($GOT != $CHECKSUM)"
info "checksum verified"

"${SSH[@]}" "$DEPLOY_HOST" "sudo -n mv -f '$STAGE_BIN' '$REMOTE_BIN'" \
	|| die "could not put the new binary in place"
info "installed $REMOTE_BIN (no backup kept)"

# ------------------------------------------------------------------- restart

if [ "$RESTART" -eq 0 ]; then
	step "Done, service $SERVICE not restarted"
	info "activate with: ssh -i $KEY $DEPLOY_HOST sudo systemctl restart $SERVICE"
	exit 0
fi

step "Restarting $SERVICE on $DEPLOY_HOST"
"${SSH[@]}" "$DEPLOY_HOST" "sudo -n systemctl restart $SERVICE" || die "could not restart $SERVICE"

for _ in 1 2 3 4 5 6 7 8 9 10; do
	if new_state="$(call_state)" && [ -n "$new_state" ]; then break; fi
	sleep 2
done

step "Result"
"${SSH[@]}" "$DEPLOY_HOST" "systemctl is-active $SERVICE; sudo -n ls -l '$REMOTE_BIN'; sudo -n journalctl -u $SERVICE -n 12 --no-pager"

if [ -n "${new_state:-}" ]; then
	printf '   state: %s\n' "$new_state"
	printf '   uptime: %ss\n' "$(printf '%s' "$new_state" | sed -n 's/.*"uptime":\([0-9]*\).*/\1/p')"
else
	warn "$SERVICE did not answer on its web interface yet; check journalctl -u $SERVICE"
fi