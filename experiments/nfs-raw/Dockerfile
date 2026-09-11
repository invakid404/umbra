FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
# Pinned versions so a rebuild is reproducible even when the Debian
# archive rolls forward under this SHA. The versions were captured
# against the pinned image; bump this list in the same commit that
# tests the resulting fixture works end to end.
RUN apt-get update && apt-get install -y --no-install-recommends \
      nfs-ganesha=4.3-2 \
      nfs-ganesha-vfs=4.3-2 \
      rpcbind=1.2.6-6+b1 \
      python3=3.11.2-1+b1 \
    && rm -rf /var/lib/apt/lists/*
COPY ganesha.conf /etc/ganesha/ganesha.conf
COPY container-entrypoint.sh /usr/local/bin/umbra-entrypoint
# The rpc-check probe's only contract is "NFSv4 NULL succeeds on TCP/2049
# inside the container"; kept identical to the mount-based fixture's copy.
COPY rpc-check.py /usr/local/bin/rpc-check.py
ENTRYPOINT ["/bin/sh", "/usr/local/bin/umbra-entrypoint"]
