#!/bin/bash
set -euo pipefail
source "$(dirname "$0")/common.sh"
current=$(mount_source)
if [[ -n $current && $current != "$NFS_SOURCE" ]]; then
    echo "FAIL refusing to replace unrelated mount: $current" >&2
    exit 1
fi
compose up -d --build --wait --wait-timeout 90
bounded 15 python3 "$TASK_DIR/rpc-check.py" 127.0.0.1
if [[ -n $current ]]; then
    if bounded 20 python3 "$TASK_DIR/host-check.py" probe "$MOUNT_POINT"; then
        echo "Already mounted: $NFS_SOURCE on $MOUNT_POINT"
    else
        echo 'Remounting unresponsive export (stop users of the mount first).'
        # Never force-unmount or discard outstanding writes.
        bounded 15 /sbin/umount "$MOUNT_POINT"
        current=
    fi
fi
if [[ -z $current ]]; then
    mkdir -p "$MOUNT_POINT"
    [[ $(cd "$MOUNT_POINT" && pwd -P) == "$MOUNT_POINT" ]] || {
        echo 'FAIL mount point must not contain symlinks' >&2; exit 1;
    }
    [[ -z $(ls -A "$MOUNT_POINT") ]] || {
        echo 'FAIL refusing to hide files in a nonempty mount point' >&2; exit 1;
    }
    # User-owned directory + noresvport permits mounting without sudo here.
    bounded 30 /sbin/mount -t nfs -o "$MOUNT_OPTIONS" "$NFS_SOURCE" "$MOUNT_POINT"
    echo "Mounted: $NFS_SOURCE on $MOUNT_POINT"
fi
bounded 20 python3 "$TASK_DIR/host-check.py" probe "$MOUNT_POINT"
python3 "$TASK_DIR/host-check.py" verify "$MOUNT_POINT" >/dev/null
echo "PASS mount is read/write NFSv4.0 at $MOUNT_POINT"
echo "Mount options: $MOUNT_OPTIONS"
