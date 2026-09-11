FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
RUN apt-get update && apt-get install -y --no-install-recommends \
      nfs-ganesha nfs-ganesha-vfs rpcbind python3 \
    && rm -rf /var/lib/apt/lists/*
COPY ganesha.conf /etc/ganesha/ganesha.conf
COPY container-entrypoint.sh /usr/local/bin/umbra-entrypoint
# The rpc-check probe's only contract is "NFSv4 NULL succeeds on TCP/2049
# inside the container"; kept identical to the mount-based fixture's copy.
COPY rpc-check.py /usr/local/bin/rpc-check.py
ENTRYPOINT ["/bin/sh", "/usr/local/bin/umbra-entrypoint"]
