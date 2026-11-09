//! Records terminal calls to real local storage; injects errors after cleanup.
use std::io::Write;
use umbra_core::*;
use umbra_storage::Storage;

struct Observed {
    inner: umbra_storage_local::LocalStorage,
    log: std::fs::File,
    mode: String,
}
impl Observed {
    fn record(&mut self, text: &str) -> Result<()> {
        writeln!(self.log, "{text}")
            .map_err(|e| UmbraError::new(ErrorKind::Io, "fixture.record", e.to_string()))
    }
}
impl Storage for Observed {
    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }
    fn open_run(&mut self, r: &OpenRunRequest) -> Result<RunBinding> {
        self.inner.open_run(r)
    }
    fn acquire_writer(&mut self, r: &AcquireWriterRequest) -> Result<WriterLease> {
        self.inner.acquire_writer(r)
    }
    fn renew_writer(&mut self, r: &WriterLease) -> Result<WriterLease> {
        self.inner.renew_writer(r)
    }
    fn release_writer(&mut self, r: &WriterLease) -> Result<()> {
        self.record("Release")?;
        self.inner.release_writer(r)?;
        if self.mode == "release" {
            return Err(UmbraError::new(
                ErrorKind::Io,
                "fixture.release",
                "injected writer release",
            ));
        }
        Ok(())
    }
    fn execute(&mut self, r: &StorageRequest) -> Result<StorageResponse> {
        self.inner.execute(r)
    }
    fn flush(&mut self, r: &FlushRequest) -> Result<DurabilityReceipt> {
        self.inner.flush(r)
    }
    fn close_run(&mut self) -> Result<()> {
        self.record("CloseRun")?;
        self.inner.close_run()?;
        if self.mode == "close-run" {
            return Err(UmbraError::new(
                ErrorKind::Io,
                "fixture.close_run",
                "injected storage close",
            ));
        }
        Ok(())
    }
}
fn main() {
    let result = umbra_storage::provider::serve_provider("local", |options| {
        let text = std::str::from_utf8(options).map_err(|e| {
            UmbraError::new(ErrorKind::InvalidInput, "fixture.options", e.to_string())
        })?;
        let mut fields = text.splitn(3, '\n');
        let mode = fields.next().unwrap_or("").to_owned();
        let root = fields.next().ok_or_else(|| {
            UmbraError::new(ErrorKind::InvalidInput, "fixture.options", "root required")
        })?;
        let path = fields.next().ok_or_else(|| {
            UmbraError::new(ErrorKind::InvalidInput, "fixture.options", "log required")
        })?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| UmbraError::new(ErrorKind::Io, "fixture.log", e.to_string()))?;
        Ok(Observed {
            inner: umbra_storage_local::LocalStorage::new(root)?,
            log,
            mode,
        })
    });
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
