//! Test-only observer around the real live-NFS backend. No injected outcomes.
use std::io::Write;
use umbra_core::*;
use umbra_storage::Storage;
use umbra_storage_nfs_userspace::{NfsUserspaceConfig, NfsUserspaceStorage};

struct Observed {
    inner: NfsUserspaceStorage,
    log: std::fs::File,
    lease: Option<WriterLease>,
    triggered: bool,
}
impl Observed {
    fn marker(&mut self, stage: &str, run: RunId) {
        use umbra_storage_nfs_userspace::transport::raw::{LibnfsRawTransport, RawTransportConfig};
        use umbra_storage_nfs_userspace::transport::{AttrMask, ComponentName, RawTransport};
        let config = self.inner.config();
        let mut raw = RawTransportConfig::loopback(config.port);
        raw.host = String::from_utf8(config.host.clone()).unwrap();
        let deadline = config.deadline;
        let path = format!(
            "{}/{}/{}/.provider/writer.lock",
            String::from_utf8_lossy(config.export.as_bytes()),
            String::from_utf8_lossy(config.run_parent.as_bytes()),
            run.0
        );
        let mut transport = LibnfsRawTransport::connect(raw).unwrap();
        let mut handle = transport.root_filehandle(deadline).unwrap();
        let mut size = 0;
        for part in path.split('/') {
            let (next, attr) = transport
                .lookup(
                    &handle,
                    &ComponentName::new(part.as_bytes().to_vec()).unwrap(),
                    AttrMask::STAT,
                    deadline,
                )
                .unwrap();
            handle = next;
            size = attr.size.unwrap_or(0);
        }
        assert!(size > 0 && size < 4096);
        let mut bytes = Vec::new();
        while bytes.len() < size as usize {
            let got = transport
                .read(
                    &handle,
                    umbra_storage_nfs_userspace::handle::Stateid::ANONYMOUS,
                    bytes.len() as u64,
                    (size as usize - bytes.len()) as u32,
                    deadline,
                )
                .unwrap();
            assert!(!got.data.is_empty());
            bytes.extend(got.data);
        }
        self.record(stage, serde_json::json!(bytes));
    }
    fn record(&mut self, stage: &str, value: serde_json::Value) {
        writeln!(
            self.log,
            "{}",
            serde_json::json!({"stage":stage,"pid":std::process::id(),"value":value})
        )
        .unwrap();
        self.log.sync_all().unwrap();
    }
}
impl Storage for Observed {
    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }
    fn open_run(&mut self, r: &OpenRunRequest) -> Result<RunBinding> {
        let out = self.inner.open_run(r);
        self.record("open", serde_json::json!({"request":r,"result":out}));
        out
    }
    fn acquire_writer(&mut self, r: &AcquireWriterRequest) -> Result<WriterLease> {
        let out = self.inner.acquire_writer(r);
        if let Ok(l) = &out {
            self.lease = Some(l.clone());
        }
        self.record("acquire", serde_json::json!(out));
        out
    }
    fn renew_writer(&mut self, l: &WriterLease) -> Result<WriterLease> {
        let out = self.inner.renew_writer(l);
        if let Ok(l) = &out {
            self.lease = Some(l.clone());
        }
        self.record("renew", serde_json::json!(out));
        out
    }
    fn release_writer(&mut self, l: &WriterLease) -> Result<()> {
        let out = self.inner.release_writer(l);
        self.record("release", serde_json::json!(out));
        out
    }
    fn flush(&mut self, r: &FlushRequest) -> Result<DurabilityReceipt> {
        let out = self.inner.flush(r);
        self.record("flush", serde_json::json!(out));
        out
    }
    fn close_run(&mut self) -> Result<()> {
        let out = self.inner.close_run();
        self.record("close", serde_json::json!(out));
        out
    }
    fn execute(&mut self, r: &StorageRequest) -> Result<StorageResponse> {
        let out = self.inner.execute(r);
        self.record("execute", serde_json::json!({"request":r,"result":out}));
        let is_target = matches!(&r.operation, StorageOperation::Unlink { .. });
        if !self.triggered
            && is_target
            && out.as_ref().is_err_and(|e| {
                e.context.contains("BLOCKED_RECOVERABLE:") && e.context.contains("REMOVE")
            })
        {
            self.triggered = true;
            let lease = self.lease.clone().expect("actual writer lease retained");
            self.marker("marker-before", lease.run_id);
            self.record("authority-before", serde_json::json!({"lease":lease,"admission":self.inner.admission().map(|s| s.lease())}));
            // The harness supplies the precise StoragePath of the seeded DST
            // via options-independent environment, retaining the original run.
            let path: StoragePath =
                serde_json::from_str(&std::env::var("UMBRA_R12_DST").unwrap()).unwrap();
            let mut context = r.context.clone();
            context.idempotency_key = IdempotencyKey("c12-authority-probe".into());
            let probe = self.inner.execute(&StorageRequest {
                context: context.clone(),
                operation: StorageOperation::WriteAt {
                    path,
                    offset: 0,
                    bytes: b"FORBIDDEN-C12-PROBE".to_vec(),
                },
            });
            self.record(
                "probe-write",
                serde_json::json!({"context":context,"result":probe}),
            );
            let flush = self.inner.flush(&FlushRequest {
                context,
                scope: FlushScope::EntireRun,
            });
            self.record("probe-flush", serde_json::json!(flush));
            let release = self.inner.release_writer(&lease);
            self.record("probe-release", serde_json::json!({"result":release,"admission":self.inner.admission().map(|s|s.lease())}));
            let close = self.inner.close_run();
            self.record("probe-close", serde_json::json!(close));
            self.marker("marker-after", lease.run_id);
            self.record("probe-complete", serde_json::json!(true));
        }
        out
    }
}
fn main() {
    let result = umbra_storage::provider::serve_provider(
        umbra_storage_nfs_userspace::PROVIDER_ID,
        |options| {
            let extra: serde_json::Value = serde_json::from_slice(options).unwrap();
            let log_path = extra["c12_log"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| std::env::var("UMBRA_R12_PROVIDER_LOG").unwrap());
            let log = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(log_path)
                .unwrap();
            Ok(Observed {
                inner: NfsUserspaceStorage::connect(NfsUserspaceConfig::from_options(options)?)?,
                log,
                lease: None,
                triggered: false,
            })
        },
    );
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
