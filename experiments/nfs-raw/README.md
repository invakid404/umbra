# Raw-transport NFSv4 fixture

Loopback NFS-Ganesha instance for the `umbra-storage-nfs-userspace`
`transport-raw` test suite.

The image is built by this directory's [`Dockerfile`](Dockerfile). The compose
file (`build: .`) then wraps it with the settings the raw-transport tests
expect:

- container name `umbra-m1-transport-raw-ganesha`
  (`UMBRA_NFS_FIXTURE_CONTAINER` in `tests/live_state.rs`)
- loopback listener on `127.0.0.1:12105`
  (`UMBRA_NFS_RAW_FIXTURE` in every raw-transport test)
- compose project `umbra-m1-transport-raw`, whose named volumes
  (`umbra-m1-transport-raw_export-data`,
  `umbra-m1-transport-raw_recovery-data`) preserve the export contents and
  the fs_ng recovery database across `docker stop`/`docker start`

The fixture speaks NFSv4.0 over TCP with AUTH_SYS. Nothing on the host is
mounted — the tests speak raw RPC to Ganesha over the exposed TCP port.

## Bring it up

```
docker compose -f experiments/nfs-raw/docker-compose.yml up -d --build --wait
```

`--wait` blocks until the health check (`rpc-check.py`) succeeds.

## Run the transport-raw suite

The raw suite lives in `crates/umbra-storage-nfs-userspace`. Every test skips
unless the two env vars below name a live fixture; a few restart the fixture
mid-test, which requires the tests to run single-threaded.

```
UMBRA_LIBNFS_SRC=/path/to/pinned/libnfs \
UMBRA_NFS_RAW_FIXTURE=127.0.0.1:12105 \
UMBRA_NFS_FIXTURE_CONTAINER=umbra-m1-transport-raw-ganesha \
cargo test -p umbra-storage-nfs-userspace \
    --features transport-raw \
    --tests \
    -- --test-threads=1 --nocapture \
    --skip the_namespace_mutations_the_hotfix_added_run_end_to_end
```

`--skip the_namespace_mutations_the_hotfix_added_run_end_to_end` matches
what CI does: NFS-Ganesha's VFS FSAL can't report atomic REMOVE
`change_info4` (RFC 7530 §14.2), so the test asserts `Abandoned`
against this fixture — a fixture-capability gap, not a code gap. The
same test runs under the mounted adapter in `crates/umbra-storage-nfs`.

`UMBRA_LIBNFS_SRC` points at a libnfs checkout at the commit named in
`crates/umbra-storage-nfs-userspace/libnfs.pin`. Without it, `build.rs`
falls back to `third_party/libnfs` under the workspace root.

## Bring it down

```
docker compose -f experiments/nfs-raw/docker-compose.yml down -v
```

The `-v` also discards the named volumes, so the next `up` starts from a
clean export and a clean recovery database.
