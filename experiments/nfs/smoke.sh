#!/bin/bash
set -euo pipefail
source "$(dirname "$0")/common.sh"
bounded 60 python3 "$TASK_DIR/smoke.py" "$MOUNT_POINT" "$TASK_DIR"
