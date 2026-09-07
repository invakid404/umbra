#!/bin/sh
set -eu
mkdir -p /export/umbra /var/lib/nfs/ganesha /run/ganesha /run/rpcbind
# Only the export root is adjusted; preserve ownership of session files.
chown "${UMBRA_UID}:${UMBRA_GID}" /export/umbra
chmod 0755 /export/umbra
rpcbind -w
exec ganesha.nfsd -F -L /dev/stdout -f /etc/ganesha/ganesha.conf
