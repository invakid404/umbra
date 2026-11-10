//! Runtime ownership of a stopped filesystem metadata query.
use umbra_core::{FilesystemBinding, FilesystemQueryToken, FilesystemReply, IoBuffer, TaskId};

/// One query retains its binding and output descriptor independently of fd reuse.
#[derive(Debug)]
pub(crate) struct PendingFilesystem {
    pub token: FilesystemQueryToken,
    pub binding: FilesystemBinding,
    pub output: IoBuffer,
}

impl PendingFilesystem {
    pub fn accepts(&self, reply: &FilesystemReply, image_generation: u64) -> bool {
        reply.token == self.token
            && reply.metadata.binding == self.binding
            && image_generation == self.token.image_generation
            && self
                .binding
                .metadata()
                .is_ok_and(|expected| reply.metadata == expected)
    }
}

#[derive(Default)]
pub(crate) struct FilesystemQueries {
    pending: Option<PendingFilesystem>,
}

impl FilesystemQueries {
    pub fn begin(&mut self, pending: PendingFilesystem) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.pending = Some(pending);
        true
    }

    pub fn cancel_task(&mut self, task: TaskId) {
        if self.pending.as_ref().is_some_and(|p| p.token.task == task) {
            self.clear();
        }
    }

    pub fn clear(&mut self) {
        self.pending = None;
    }

    /// Consume even a mismatched completion: no stale output can be reused.
    pub fn finish(&mut self, reply: &FilesystemReply, image_generation: u64) -> Option<IoBuffer> {
        let pending = self.pending.take()?;
        pending
            .accepts(reply, image_generation)
            .then_some(pending.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::{RunId, ThreadId};
    use uuid::Uuid;

    fn pending(request: u64, binding: FilesystemBinding) -> PendingFilesystem {
        PendingFilesystem {
            token: FilesystemQueryToken {
                request,
                task: TaskId(umbra_core::TaskIdentity {
                    native_id: 1,
                    generation: 1,
                }),
                thread: ThreadId(umbra_core::TaskIdentity {
                    native_id: 2,
                    generation: 1,
                }),
                image_generation: 3,
            },
            binding,
            output: IoBuffer {
                address: 0x1000,
                length: 2168,
            },
        }
    }
    fn binding(id: u128) -> FilesystemBinding {
        FilesystemBinding::overlay(RunId(Uuid::from_u128(id)))
    }
    fn reply(pending: &PendingFilesystem) -> FilesystemReply {
        FilesystemReply {
            token: pending.token.clone(),
            metadata: pending.binding.metadata().unwrap(),
        }
    }

    #[test]
    fn f214_pending_close_reuse() {
        let mut queries = FilesystemQueries::default();
        let mut descriptors = std::collections::BTreeMap::from([(4096, binding(1))]);
        let first = pending(1, descriptors[&4096].clone());
        let response = reply(&first);
        assert!(queries.begin(first));
        descriptors.remove(&4096);
        descriptors.insert(4096, binding(2));
        assert!(queries.finish(&response, 3).is_some());
        let second = pending(2, descriptors[&4096].clone());
        let second_response = reply(&second);
        assert_ne!(response.metadata.binding, second_response.metadata.binding);
        assert!(queries.begin(second));
        assert!(queries.finish(&second_response, 3).is_some());
    }

    #[test]
    fn f214_pending_cancel_exec() {
        let mut queries = FilesystemQueries::default();
        let query = pending(1, binding(1));
        let response = reply(&query);
        assert!(queries.begin(query));
        queries.cancel_task(TaskId(umbra_core::TaskIdentity {
            native_id: 1,
            generation: 1,
        }));
        assert!(queries.finish(&response, 4).is_none());
        // Independently catch image-generation validation being removed.
        let query = pending(2, binding(1));
        let response = reply(&query);
        assert!(queries.begin(query));
        assert!(queries.finish(&response, 4).is_none());
        assert!(queries.finish(&response, 3).is_none());
    }

    #[test]
    fn f214_pending_wrong_binding_and_request() {
        for fault in 0..6 {
            let mut queries = FilesystemQueries::default();
            let query = pending(1, binding(1));
            let mut response = reply(&query);
            match fault {
                0 => response.metadata.binding = binding(2),
                1 => response.token.request += 1,
                2 => {
                    response.token.thread = ThreadId(umbra_core::TaskIdentity {
                        native_id: 3,
                        generation: 1,
                    })
                }
                3 => response.token.task.0.generation += 1,
                4 => response.metadata.type_name = "apfs".into(),
                _ => response.metadata.counts[0] = umbra_core::FilesystemStatistic::Known(0),
            }
            assert!(queries.begin(query));
            assert!(queries.finish(&response, 3).is_none());
        }
    }
}
