#!/bin/bash
set -euo pipefail
source "$(dirname "$0")/common.sh"
current=$(mount_source)
if [[ -n $current ]]; then
    [[ $current == "$NFS_SOURCE" ]] || {
        echo "FAIL refusing to unmount unrelated source: $current" >&2; exit 1;
    }
    # Leave the server running if clients still hold the mount busy.
    bounded 30 /sbin/umount "$MOUNT_POINT"
    echo "Unmounted: $MOUNT_POINT"
else
    echo "Already unmounted: $MOUNT_POINT"
fi
compose down
echo 'PASS container stopped; export and recovery volumes retained'
