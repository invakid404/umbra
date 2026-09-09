//! A [`MarkerStore`] that lives on the server, reached through the frozen
//! transport facade.
//!
//! # Why admission needs its own store rather than an operations call
//!
//! The failure model's split-brain row requires "server-atomic admission" — the
//! contender must be denied by the server, not by a read-then-write that two
//! hosts can interleave. That atomicity is an authority requirement, so the one
//! operation it depends on is expressed here rather than borrowed from the
//! operations node: `OPEN` with `GUARDED4`, which answers `NFS4ERR_EXIST` when
//! the name is already there.
//!
//! `GUARDED4` and not `EXCLUSIVE4` on purpose. An exclusive create is *designed*
//! to let a replay with the same verifier succeed a second time, which is right
//! for a retried create and wrong for admission: a second session must be told
//! the name exists, not handed a success. Umbra's own retry safety for the
//! marker comes from the marker's contents, which record who holds it.
//!
//! This is deliberately the whole of it. No path resolution, no anchoring, no
//! CRUD surface, no capability: three calls over the frozen facade, on one name,
//! for one purpose. Binding any of this into `Storage` is `m1_integrate`'s seam.

use crate::error::{AuthorityError, FacadeError, FacadeResult, Nfs4Status};
use crate::handle::FileHandle;
use crate::state::open_owner::{close, CloseOutcome, OpenOutcome, OpenOwnerRegistry, OpenRequest};
use crate::transport::{
    ComponentName, Deadline, OpenHow, RawTransport, ShareAccess, ShareDeny, Stability,
};

use super::marker::{ExclusiveCreate, MarkerStore, EXTENDED_MARKER_BYTES};

/// Mode the marker is created with, matching the run layout the golden fixtures
/// pin for `.provider/writer.lock`.
const MARKER_MODE: u32 = 0o600;

/// The admission marker as a file on the export, driven over a [`RawTransport`].
pub struct ServerMarkerStore<'a> {
    transport: &'a mut dyn RawTransport,
    owners: &'a mut OpenOwnerRegistry,
    parent: FileHandle,
    name: ComponentName,
    deadline: Deadline,
}

impl std::fmt::Debug for ServerMarkerStore<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerMarkerStore")
            .field("name", &self.name)
            .field("connection", &self.transport.connection())
            .finish()
    }
}

impl<'a> ServerMarkerStore<'a> {
    /// A store for `name` inside the directory `parent`.
    ///
    /// `owners` mints the open owners the three calls sequence through; it is
    /// borrowed rather than owned so the marker's opens share the session's
    /// client id and seqid discipline instead of inventing a parallel one.
    pub fn new(
        transport: &'a mut dyn RawTransport,
        owners: &'a mut OpenOwnerRegistry,
        parent: FileHandle,
        name: ComponentName,
        deadline: Deadline,
    ) -> Self {
        Self {
            transport,
            owners,
            parent,
            name,
            deadline,
        }
    }

    fn request(&self, how: OpenHow, share_access: ShareAccess) -> OpenRequest {
        OpenRequest {
            parent: self.parent.clone(),
            name: self.name.clone(),
            how,
            share_access,
            // Umbra arbitrates writers itself. An NFS share reservation is not
            // the admission mechanism and must not be mistaken for one.
            share_deny: ShareDeny::NONE,
        }
    }

    /// Open the marker, run `body` against it, and close it either way.
    fn with_open<T>(
        &mut self,
        how: OpenHow,
        share_access: ShareAccess,
        body: impl FnOnce(
            &mut dyn RawTransport,
            &FileHandle,
            crate::handle::Stateid,
            Deadline,
        ) -> FacadeResult<T>,
    ) -> FacadeResult<T> {
        let lease = self.owners.allocate()?;
        let request = self.request(how, share_access);
        let file = match self
            .owners
            .open(lease, &mut *self.transport, &request, self.deadline)
        {
            OpenOutcome::Opened(file) => file,
            OpenOutcome::Unconfirmed { error, .. } | OpenOutcome::Rejected { error, .. } => {
                return Err(error)
            }
            OpenOutcome::Abandoned { error } => return Err(error),
        };
        let stateid = file.stateid()?;
        let handle = file.handle().clone();
        let result = body(&mut *self.transport, &handle, stateid, self.deadline);
        // The open is closed whichever way the body went: leaving marker state
        // open across a failure would hold server-side state this session cannot
        // account for.
        match close(file, &mut *self.transport, self.deadline) {
            CloseOutcome::Closed(_) => result,
            CloseOutcome::Rejected { error, .. } | CloseOutcome::Abandoned { error } => {
                // A body failure is the more informative answer; a close failure
                // only wins when the body succeeded.
                result.and(Err(error))
            }
        }
    }

    fn short_write(wrote: u32, wanted: usize) -> FacadeError {
        FacadeError::Authority(AuthorityError::IdentityUnproven(format!(
            "admission marker write was short: {wrote} of {wanted} bytes, so the record on the \
             server is not a complete marker"
        )))
    }
}

impl MarkerStore for ServerMarkerStore<'_> {
    fn create_exclusive(&mut self, bytes: &[u8]) -> FacadeResult<ExclusiveCreate> {
        let payload = bytes.to_vec();
        let wanted = payload.len();
        let outcome = self.with_open(
            OpenHow::Guarded { mode: MARKER_MODE },
            ShareAccess::WRITE,
            move |transport, handle, stateid, deadline| {
                // FILE_SYNC4: the marker is ownership evidence, so it is durable
                // before this call returns or it is not evidence at all.
                let reply =
                    transport.write(handle, stateid, 0, Stability::FileSync, payload, deadline)?;
                if reply.count as usize != wanted {
                    return Err(Self::short_write(reply.count, wanted));
                }
                Ok(())
            },
        );
        match outcome {
            Ok(()) => Ok(ExclusiveCreate::Created),
            // GUARDED4 answering EXIST is the server-atomic denial. It is the one
            // status here that is not a failure.
            Err(error) if error.status() == Some(Nfs4Status::EXIST) => Ok(ExclusiveCreate::Exists),
            Err(error) => Err(error),
        }
    }

    fn read(&mut self) -> FacadeResult<Option<Vec<u8>>> {
        let outcome = self.with_open(
            OpenHow::NoCreate,
            ShareAccess::READ,
            |transport, handle, stateid, deadline| {
                let reply =
                    transport.read(handle, stateid, 0, EXTENDED_MARKER_BYTES as u32, deadline)?;
                Ok(reply.data.clone())
            },
        );
        match outcome {
            Ok(bytes) => Ok(Some(bytes)),
            // No marker means no run has ever been admitted here. That is an
            // answer, not a failure.
            Err(error) if error.status() == Some(Nfs4Status::NOENT) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn overwrite(&mut self, bytes: &[u8]) -> FacadeResult<()> {
        let payload = bytes.to_vec();
        let wanted = payload.len();
        self.with_open(
            OpenHow::NoCreate,
            ShareAccess::WRITE,
            move |transport, handle, stateid, deadline| {
                let reply =
                    transport.write(handle, stateid, 0, Stability::FileSync, payload, deadline)?;
                if reply.count as usize != wanted {
                    return Err(Self::short_write(reply.count, wanted));
                }
                Ok(())
            },
        )
    }
}
