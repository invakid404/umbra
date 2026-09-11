#!/bin/sh
set -eu

# Ganesha state directories plus the two exports the raw-transport tests walk.
mkdir -p /export/probe /export/pagedir /umbra/runs \
         /var/lib/nfs/ganesha /run/ganesha /run/rpcbind

# fault_matrix.rs and live_state.rs open /export/probe/{read.txt,write.txt}
# with OpenHow::NoCreate, so both files must exist before the first client.
# `an_anchored_open_confirms_when_the_server_demands_it` reads read.txt and
# asserts the exact 20 bytes below. write.txt is created empty because the
# tests write to it and read the same bytes back.
if [ ! -s /export/probe/read.txt ]; then
    printf 'hello-umbra-raw-rpc\n' > /export/probe/read.txt
fi
touch /export/probe/write.txt

# live_state.rs::bounded_readdir_paging_keeps_cookieverf_continuity walks
# /export/pagedir with a deliberately small READDIR budget and asserts at
# least 120 entries across more than one page. Seed exactly that many.
i=1
while [ $i -le 120 ]; do
    name=$(printf 'entry-%03d.bin' $i)
    [ -e "/export/pagedir/$name" ] || : > "/export/pagedir/$name"
    i=$((i + 1))
done

# The tests attach as whatever uid the runner has; wide-open mode keeps every
# path writable without any threat model beyond "one client on loopback".
chmod -R 0777 /export /umbra

rpcbind -w
exec ganesha.nfsd -F -L /dev/stdout -f /etc/ganesha/ganesha.conf
