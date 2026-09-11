FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
# Pinning the base image's digest freezes what's already installed, but
# not what `apt-get install` resolves next — a rebuild would still pull
# whatever's current in the live archive. Point APT at a dated
# snapshot.debian.org archive (the day the pinned base image was built)
# so the entire dependency closure resolves the same way on every build.
# `check-valid-until=no` is required because snapshot archives are past
# their advertised expiry. Plain HTTP is fine here: `bookworm-slim`
# ships no CA bundle, and every .deb is signed and verified by apt's
# own trusted keys under `/etc/apt/trusted.gpg.d/` regardless of
# transport.
ARG DEBIAN_SNAPSHOT=20260824T000000Z
RUN set -eu; \
    rm -f /etc/apt/sources.list.d/debian.sources; \
    printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/%s/ bookworm main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list
# Top-level pins keep the explicit-intent hash in review-visible history;
# under the snapshot archive above they are also the exact versions
# apt-get resolves for the whole dependency closure.
RUN apt-get -o Acquire::Retries=5 update \
    && apt-get install -y --no-install-recommends \
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
