# Local NFS for umbra M0

NFS-Ganesha 4.3 (Debian package `4.3-2`, native Linux arm64) runs in
OrbStack and exports a persistent Docker volume at `/export/umbra`.
Its NFSv4 pseudo-path is `/umbra`, reached from macOS at
`127.0.0.1:2049` over TCP. NFSv3 is disabled globally and for the export.

## Bring up, test, stop

Run as your regular macOS user with OrbStack running, Docker Compose v2,
and `python3` available:

```sh
cd /Users/inva/umbra-scratch/nfs
./up.sh
./smoke.sh
./down.sh
```

`up.sh` builds the image, waits up to 90 seconds for a successful NFSv4 RPC
probe that also verifies NFSv3 rejection, checks the host endpoint, and mounts
the export. Re-running it retains a responsive existing mount and checks its
actual negotiated options. An unresponsive mount is unmounted and remounted
if normal unmount succeeds; a busy mount causes failure. It never force-unmounts.
An unrelated mount or a nonempty underlying directory is rejected.

`down.sh` unmounts before stopping/removing the container and network; it
retains both named volumes. If unmount fails, the server remains running.
It is safe to re-run. Stop processes using the export and leave any shell
working directories inside it before teardown. Do not use `docker compose
down -v`: that deletes session and recovery data.

No sudo was needed, including for unmount. This Mac permits an ordinary user
to mount NFS on a directory they own with `noresvport`. Run the scripts from
a normal terminal or an execution environment permitted to access the Docker
socket, network, and mount syscalls. The agent sandbox blocked Docker socket
access and `nfsstat` initially; execution outside that sandbox worked as UID
501 without elevating to root. No host system configuration was changed.

## Mount point for Track A

**Use `/Users/inva/umbra-scratch/nfs/mnt/umbra-nfs`.**

The requested `/mnt/umbra-nfs` could not be used within the constraints:
`/mnt` is absent and `/` is the sealed, read-only system volume on this
macOS 26.5.1 installation. Creating a synthetic root entry would require a
system configuration change and reboot. `/System/Volumes/Data/mnt` exists
but is root-owned and does not provide `/mnt`. The project-local mount
avoids those changes and requires no sudo.

Point the tracer's physical redirect target at this path, for example
`/Users/inva/umbra-scratch/nfs/mnt/umbra-nfs/fsvirt/runs/<run-id>/root/`.
This setup does not edit the umbra repo. Persistent run metadata should keep
logical paths independent of this physical mount location, as specified in
handoff §4. Set `UMBRA_MOUNT_POINT` to another absolute, user-owned, empty
directory if needed; use the same value for all three scripts. Do not switch
the value without bringing the old mount down first. Symlink paths are rejected.

## Mount options

```text
rw,vers=4.0,tcp,port=2049,noresvport,hard,intr,actimeo=0,nonegnamecache,nocallback,noatime,nobrowse,retrycnt=0
```

- `vers=4.0,tcp,port=2049` pins NFSv4.0 without protocol fallback or host
  rpcbind/mountd discovery. Ganesha supports minor versions 0 and 1 here;
  the tested Mac mount uses 0.
- `rw,hard,intr` allows writes and retries server outages without introducing
  soft-mount write errors; blocked I/O is interruptible. There is no automatic
  dead timeout or forced unmount that would discard pending writes.
- `actimeo=0` disables regular-file, directory, and root-directory attribute
  caching; `nonegnamecache` disables negative lookup caching. `nocallback` and
  server-side `Delegations=false` prevent delegation-based caching. Server
  export attribute expiration is also zero.
- TCP, ordinary buffered I/O, and default pipelining/readahead permit
  throughput without synchronous I/O on every write. Observed negotiated
  values are `rsize=32768,wsize=32768,readahead=16`. No unsafe `async` option
  is enabled. Small-file throughput is reduced by disabling metadata caches;
  no performance target or benchmark is claimed.
- `noresvport` enables the unprivileged mount. `noatime` avoids access-time
  traffic; `nobrowse` keeps Finder from advertising the volume. macOS also
  applies `nodev,nosuid` to this user mount.
- `retrycnt=0` bounds initial mount attempts. It does not make mounted I/O
  soft. Script readiness and mount probes have their own time limits.

These are the macOS options from the installed `mount_nfs(8)` manual, not
Linux-only options such as `lookupcache=none`. `nfsstat -m` records the
negotiated values in `logs/nfsstat.log`.

## Storage, identity, and restart

`umbra-nfs_export-data` is a Docker named volume on OrbStack's Linux storage,
not a macOS bind mount or the container's disposable root filesystem.
`umbra-nfs_recovery-data` persists Ganesha's NFSv4 recovery records. The export
ID, hostname, and export path are stable. The only added container capability
is `DAC_READ_SEARCH`, needed by the VFS backend to open Linux file handles;
the container does not run privileged or mount kernel nfsd.

The entrypoint sets only the export root owner to the invoking Mac user's
UID/GID (`501:20` here; overridable with `UMBRA_UID`/`UMBRA_GID`). It preserves
ownership of existing files. AUTH_SYS and root squashing are enabled.
Docker publishes only TCP/2049 on IPv4 loopback; rpcbind stays inside the
container. Other containers with access to the Compose network may reach
the server. This is a trusted local prototype, without Kerberos or encryption.

```sh
docker compose restart nfs
./up.sh
./smoke.sh
```

Restart recovery uses a 10-second lease and grace period for this local
experiment; an operation may pause during grace. `restart: unless-stopped`
restarts the service with OrbStack unless it was explicitly stopped.
After a Mac/OrbStack restart, run `up.sh` again to restore the host mount.
There is no host boot service or automount configuration.

## Verification and caveats

Tested on macOS 26.5.1 build 25F80, Apple Silicon, Docker 29.4.0, OrbStack
Docker context, with SIP left enabled. `logs/` contains actual execution output.

- Smoke verifies the mount and negotiated options, NFSv4 acceptance/NFSv3
  rejection, host binary write plus `fsync`, byte-exact host and container
  readback, a container update visible immediately on the host, and host
  unlink visible in the container. Every assertion prints PASS/FAIL; failure
  exits nonzero. Temporary smoke files are removed.
- Restart testing verified persistent bytes and an already-open host file
  descriptor across `docker compose restart nfs` plus `up.sh`.
- Teardown/recreation testing verified persistent data after `down.sh` then
  `up.sh`, idempotence of both scripts, and that smoke refuses an unmounted
  directory without creating local files. The final server is left mounted.
- Ganesha logs a warning about OrbStack's `btfs` filesystem and possible
  unsupported subvolumes. The plain export volume passed the above checks;
  nested subvolumes/snapshots are not qualified.
- Ganesha also logs missing D-Bus and Kerberos credentials at startup. Those
  optional facilities are not configured; the tested AUTH_SYS export works.
- Attribute-cache suppression does not eliminate client RAM/data caches or
  prove arbitrary concurrent-writer ordering. Use explicit `fsync`, quiesce
  writers, and implement run leases/fencing and journal checkpoints per
  handoff §9. SQLite, locking, xattrs/ACLs, crash/power-loss durability,
  multi-client access, and machine-to-machine handoff remain unqualified.
- This is locally backed storage inside OrbStack's VM disk. It provides the
  real NFS protocol needed for M0; it is not off-machine durability or a backup.
- The Debian base image digest is pinned. Package versions are selected from
  Debian Bookworm repositories at build time, so a future uncached rebuild
  can receive package updates and should be requalified with the smoke test.

Configuration references: upstream [Ganesha core configuration](https://github.com/nfs-ganesha/nfs-ganesha/blob/next/src/doc/man/ganesha-core-config.rst),
[export configuration](https://github.com/nfs-ganesha/nfs-ganesha/blob/next/src/doc/man/ganesha-export-config.rst),
and [VFS backend requirements](https://github.com/nfs-ganesha/nfs-ganesha/wiki/VFS).
The installed macOS `man mount_nfs` and `man mount` are the client option reference.
