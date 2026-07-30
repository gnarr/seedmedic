#!/bin/sh
# Installed as /usr/local/bin/seedmedic-entrypoint.
#
# Starts as root, makes the state directories owned by PUID:PGID, drops to
# PUID:PGID, and execs. If the container was started as a non-root user
# (`--user`, or `user:` in compose) it can chown nothing and must not pretend
# otherwise: it execs unchanged. That is a supported way to run.
#
# See docs/todos/0020-a-container-that-just-runs.md.
set -eu

DATA_DIR=/data
# Mounted at the same path on both sides, because SeedMedic hands it to the
# download client verbatim (see the chown block below). Passed in so this
# script knows what to take ownership of; SeedMedic itself never reads it —
# the staging root is a setting, and it is set at /settings.
STAGING_DIR=${STAGING_PATH:-/srv/seedmedic/staging}

log() { printf 'seedmedic-entrypoint: %s\n' "$*"; }
warn() { printf 'seedmedic-entrypoint: %s\n' "$*" >&2; }

# CMD is ["seedmedic"], so "$@" normally already names the binary. This makes
# `docker run IMAGE --check-config` work as well as the explicit
# `docker run IMAGE seedmedic --check-config`, and leaves `docker run IMAGE sh`
# alone.
if [ "$#" -eq 0 ]; then
    set -- seedmedic
fi
case $1 in
-*) set -- seedmedic "$@" ;;
esac

if [ "$(id -u)" -ne 0 ]; then
    # No CAP_CHOWN and no CAP_SETGID here. setpriv would fail with exit 127,
    # which reads as "command not found" and sends people hunting for a
    # missing binary that is present.
    if [ -n "${PUID:-}${PGID:-}" ]; then
        warn "started as uid $(id -u):$(id -g); PUID/PGID ignored"
    fi
    exec "$@"
fi

PUID=${PUID:-1000}
PGID=${PGID:-1000}

case "$PUID:$PGID" in
*[!0-9:]*)
    warn "PUID and PGID must be numeric (got $PUID:$PGID)"
    exit 1
    ;;
esac

if [ "$PUID" -eq 0 ]; then
    warn "PUID=0: running as root. Nothing below this line protects the library."
    exec "$@"
fi

# Guarded on the top-level owner, so the pass happens on the first start and
# after a deliberate PUID change, and never again.
#
# A chown failure is never fatal. A bind mount can sit on a filesystem with no
# real ownership (Docker Desktop's virtiofs, CIFS, NFS with root_squash) or be
# mounted read-only. In each case the mount either already works, or fails
# later with an error that names the real problem; exiting here would replace
# that message with a worse one.
own() {
    target=$1
    recurse=$2 # "-R" or ""

    current=$(stat -c '%u:%g' "$target" 2>/dev/null) || {
        warn "cannot stat $target"
        return 0
    }
    if [ "$current" = "$PUID:$PGID" ]; then
        return 0
    fi

    log "chown ${recurse:+$recurse }$PUID:$PGID $target (was $current)"
    # Never -L: that follows symlinks out of the tree.
    # shellcheck disable=SC2086 # $recurse is a deliberate unquoted flag or empty
    chown $recurse "$PUID:$PGID" "$target" 2>/dev/null ||
        warn "could not chown $target; leaving it $current"
    return 0
}

# Config and the database. Entirely ours and small — config.toml, config.toml.bak,
# seedmedic.db and its -wal/-shm — so recursing is correct and cheap.
own "$DATA_DIR" -R

# Staging: TOP LEVEL ONLY. Never -R.
#
# staging::adapters::local materialises library content by hard link, so a
# staged file is the *same inode* as the library file. `chown -R` here would
# rewrite the owner of files inside the media library, which AGENTS.md's first
# rule forbids unconditionally. Owning the directory is sufficient: the process
# creates, links and unlinks inside it and never writes into an existing staged
# file.
#
# Only when it already exists — which, for a bind mount, it always does. This
# script must never create it: an unmounted staging path has to stay absent so
# that `/` staying unwritable refuses a mistyped staging.root inline, rather
# than quietly staging into the container's own filesystem and losing the lot
# on the next `docker compose up`.
if [ -d "$STAGING_DIR" ]; then
    own "$STAGING_DIR" ""
else
    log "no directory at $STAGING_DIR; mount one there before setting staging.root"
fi

# The media mount is never touched. It is read-only and not ours.

# setpriv is util-linux, which is `Essential: yes` in Debian — already present
# in debian:bookworm-slim and not removable. It execs the target in place
# rather than forking, so seedmedic becomes PID 1 and receives SIGTERM from
# `docker stop` directly.
#
# --clear-groups rather than --init-groups: --init-groups needs an /etc/passwd
# entry for the uid, which a runtime-chosen PUID does not have. Not creating
# one is the point — nothing in the image is mutated, so this behaves
# identically on a --read-only rootfs and there is no uid/gid collision
# handling to get wrong.
if [ -n "${SUPPLEMENTARY_GIDS:-}" ]; then
    set -- setpriv --reuid "$PUID" --regid "$PGID" --groups "$SUPPLEMENTARY_GIDS" \
        --inh-caps=-all --no-new-privs -- "$@"
else
    set -- setpriv --reuid "$PUID" --regid "$PGID" --clear-groups \
        --inh-caps=-all --no-new-privs -- "$@"
fi

log "config ${SEEDMEDIC_CONFIG:-$DATA_DIR/config.toml}, staging $STAGING_DIR, library mount /srv/media (read-only)"
log "starting as $PUID:$PGID"
exec "$@"
