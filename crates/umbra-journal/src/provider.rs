//! Journal protocol uses a session-scoped cursor with one bounded record per page.
use super::*;
use serde::{Deserialize, Serialize};
use umbra_core::provider::{
    self as wire, protocol_error, Client, Connection, Frame, ProviderDescriptor,
};

/// Owned journal method requests for version 1.
#[derive(Serialize, Deserialize)]
pub enum Request {
    /// Open.
    Open(JournalOpenRequest),
    /// Append.
    Append(JournalRecord),
    /// Flush.
    Flush(Sequence),
    /// Replay.
    Replay(Sequence),
    /// Next.
    Next {
        /// Opaque continuation identity owned by this provider session.
        cursor: u64,
    },
    /// Release.
    Release {
        /// Opaque continuation identity owned by this provider session.
        cursor: u64,
    },
    /// Checkpoint.
    Checkpoint(Checkpoint),
    /// Close.
    Close,
}
/// Method-tagged responses; Page(None) is EOF, errors are never EOF.
#[derive(Serialize, Deserialize)]
pub enum Response {
    /// Open.
    Open(Box<RecoveryState>),
    /// Append.
    Append(Sequence),
    /// Flush.
    Flush(DurableSequence),
    /// Cursor.
    Cursor(u64),
    /// Page.
    Page(Option<JournalRecord>),
    /// Checkpoint.
    Checkpoint(CheckpointId),
    /// Unit.
    Unit,
}
/// Owns the journal provider connection. Replay borrows it exclusively.
pub struct Proxy {
    client: Client,
}
impl Proxy {
    /// Validate the descriptor and handshake before exposing a journal contract.
    pub fn connect(descriptor: &ProviderDescriptor, timeout_ms: u64) -> Result<Self> {
        descriptor.validate("journal")?;
        Ok(Self {
            client: Client::connect(descriptor, timeout_ms)?,
        })
    }
    fn call(&mut self, request: &Request) -> Result<Response> {
        self.client.call(request)
    }
}
impl Journal for Proxy {
    fn open(&mut self, request: &JournalOpenRequest) -> Result<RecoveryState> {
        match self.call(&Request::Open(request.clone()))? {
            Response::Open(v) => Ok(*v),
            _ => Err(protocol_error("journal.open response")),
        }
    }
    fn append(&mut self, record: &JournalRecord) -> Result<Sequence> {
        match self.call(&Request::Append(record.clone()))? {
            Response::Append(v) => Ok(v),
            _ => Err(protocol_error("journal.append response")),
        }
    }
    fn flush(&mut self, through: Sequence) -> Result<DurableSequence> {
        match self.call(&Request::Flush(through))? {
            Response::Flush(v) => Ok(v),
            _ => Err(protocol_error("journal.flush response")),
        }
    }
    fn replay(
        &mut self,
        after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
        let cursor = match self.call(&Request::Replay(after))? {
            Response::Cursor(v) => v,
            _ => return Err(protocol_error("journal.replay response")),
        };
        Ok(Box::new(Replay {
            proxy: self,
            cursor,
            done: false,
        }))
    }
    fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId> {
        match self.call(&Request::Checkpoint(checkpoint.clone()))? {
            Response::Checkpoint(v) => Ok(v),
            _ => Err(protocol_error("journal.checkpoint response")),
        }
    }
    fn close(&mut self) -> Result<()> {
        match self.call(&Request::Close)? {
            Response::Unit => Ok(()),
            _ => Err(protocol_error("journal.close response")),
        }
    }
}
struct Replay<'a> {
    proxy: &'a mut Proxy,
    cursor: u64,
    done: bool,
}
impl Iterator for Replay<'_> {
    type Item = Result<JournalRecord>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.proxy.call(&Request::Next {
            cursor: self.cursor,
        }) {
            Ok(Response::Page(Some(record))) => Some(Ok(record)),
            Ok(Response::Page(None)) => {
                self.done = true;
                None
            }
            Ok(_) => {
                self.done = true;
                Some(Err(protocol_error("journal.page response")))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
impl Drop for Replay<'_> {
    fn drop(&mut self) {
        // Always release, including early drop, EOF and iterator errors.
        if !matches!(
            self.proxy.call(&Request::Release {
                cursor: self.cursor
            }),
            Ok(Response::Unit)
        ) {
            self.proxy.client.connection.invalidate();
        }
    }
}
fn receive(connection: &mut Connection, previous: &mut u64) -> Result<(u64, Request)> {
    connection.begin();
    let Frame::Request { id, payload } = connection.receive_request()? else {
        return Err(protocol_error("expected journal request"));
    };
    if id <= *previous {
        return Err(protocol_error("unordered journal request"));
    }
    *previous = id;
    Ok((id, wire::decode(&payload)?))
}
fn reply(connection: &mut Connection, id: u64, result: Result<Response>) -> Result<()> {
    connection.send(&Frame::Response {
        id,
        result: result.and_then(|r| wire::encode(&r)),
    })
}
/// Serve one journal with exclusive, bounded replay cursors; disconnect drops the iterator.
pub fn serve_provider<B: Journal>(
    id: &str,
    factory: impl FnOnce(&[u8]) -> Result<B>,
) -> Result<()> {
    let (connection, backend) = wire::accept(id, "journal", |options| {
        Ok((factory(options)?, Default::default()))
    })?;
    serve_session(connection, backend)
}

fn serve_session<B: Journal>(mut connection: Connection, mut backend: B) -> Result<()> {
    let mut previous = 0;
    loop {
        let (id, request) = receive(&mut connection, &mut previous)?;
        let result = match request {
            Request::Open(request) => backend
                .open(&request)
                .map(|state| Response::Open(Box::new(state))),
            Request::Append(record) => backend.append(&record).map(Response::Append),
            Request::Flush(through) => backend.flush(through).map(Response::Flush),
            Request::Checkpoint(checkpoint) => backend
                .write_checkpoint(&checkpoint)
                .map(Response::Checkpoint),
            Request::Close => backend.close().map(|()| Response::Unit),
            Request::Replay(after) => {
                match backend.replay(after) {
                    Err(e) => reply(&mut connection, id, Err(e))?,
                    Ok(mut records) => {
                        let cursor = id;
                        reply(&mut connection, id, Ok(Response::Cursor(cursor)))?;
                        let mut finished = false;
                        loop {
                            let (id, request) = receive(&mut connection, &mut previous)?;
                            match request {
                                Request::Next { cursor: got } if got == cursor => {
                                    let result = if finished {
                                        Ok(Response::Page(None))
                                    } else {
                                        match records.next() {
                                            Some(Ok(record)) => Ok(Response::Page(Some(record))),
                                            Some(Err(e)) => {
                                                finished = true;
                                                Err(e)
                                            }
                                            None => {
                                                finished = true;
                                                Ok(Response::Page(None))
                                            }
                                        }
                                    };
                                    reply(&mut connection, id, result)?;
                                }
                                Request::Release { cursor: got } if got == cursor => {
                                    reply(&mut connection, id, Ok(Response::Unit))?;
                                    break;
                                }
                                _ => {
                                    return Err(protocol_error(
                                        "invalid cursor or interleaved journal operation",
                                    ))
                                }
                            }
                        }
                    }
                }
                continue;
            }
            Request::Next { .. } | Request::Release { .. } => {
                Err(protocol_error("no active replay cursor"))
            }
        };
        reply(&mut connection, id, result)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixStream, time::Duration};
    use umbra_core::UmbraError;
    struct FakeJournal;
    impl Journal for FakeJournal {
        fn open(&mut self, _: &JournalOpenRequest) -> Result<RecoveryState> {
            Err(UmbraError::not_implemented("fake.open"))
        }
        fn append(&mut self, _: &JournalRecord) -> Result<Sequence> {
            Err(UmbraError::not_implemented("fake.append"))
        }
        fn flush(&mut self, _: Sequence) -> Result<DurableSequence> {
            Err(UmbraError::not_implemented("fake.flush"))
        }
        fn replay(
            &mut self,
            _: Sequence,
        ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
            Ok(Box::new(std::iter::once(Err(UmbraError::new(
                umbra_core::ErrorKind::CorruptJournal,
                "fake.replay",
                "bad frame",
            )))))
        }
        fn write_checkpoint(&mut self, _: &Checkpoint) -> Result<CheckpointId> {
            Err(UmbraError::not_implemented("fake.checkpoint"))
        }
        fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn replay_propagates_streaming_errors_and_releases_on_early_drop() {
        let (left, right) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            serve_session(Connection::new(right, Duration::from_secs(2)), FakeJournal)
        });
        let mut proxy = Proxy {
            client: Client::from_connection(
                Connection::new(left, Duration::from_secs(2)),
                wire::Welcome {
                    id: "fake".into(),
                    role: "journal".into(),
                    version: wire::PROTOCOL_VERSION,
                    capabilities: Default::default(),
                },
            ),
        };
        {
            let _unread = proxy.replay(Sequence(0)).unwrap();
        }
        {
            let mut replay = proxy.replay(Sequence(0)).unwrap();
            assert_eq!(
                replay.next().unwrap().unwrap_err().kind,
                umbra_core::ErrorKind::CorruptJournal
            );
            assert!(replay.next().is_none());
        }
        proxy.close().unwrap(); // Both cursors were released; ordinary calls work again.
        drop(proxy);
        assert!(worker.join().unwrap().is_err());
    }
}
