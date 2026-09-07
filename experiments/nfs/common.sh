#!/bin/bash
# Sourced by the host scripts; macOS ships Bash 3.2.
set -euo pipefail
TASK_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
MOUNT_POINT=${UMBRA_MOUNT_POINT:-$TASK_DIR/mnt/umbra-nfs}
NFS_SOURCE=127.0.0.1:/umbra
MOUNT_OPTIONS=rw,vers=4.0,tcp,port=2049,noresvport,hard,intr,actimeo=0,nonegnamecache,nocallback,noatime,nobrowse,retrycnt=0
export UMBRA_UID=${UMBRA_UID:-$(id -u)}
export UMBRA_GID=${UMBRA_GID:-$(id -g)}
[[ $(uname -s) == Darwin ]] || { echo 'FAIL macOS host required' >&2; exit 1; }
[[ $MOUNT_POINT == /* ]] || { echo 'FAIL mount point must be absolute' >&2; exit 1; }
command -v python3 >/dev/null
command -v docker >/dev/null
compose() { docker compose --project-directory "$TASK_DIR" -f "$TASK_DIR/docker-compose.yml" "$@"; }
mount_source() { python3 "$TASK_DIR/host-check.py" source "$MOUNT_POINT"; }
bounded() { python3 "$TASK_DIR/host-check.py" timeout "$@"; }
