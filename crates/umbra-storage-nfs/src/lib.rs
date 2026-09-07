//! Stub storage backend for an externally mounted NFSv4 export.
//!
//! The mount root is runtime configuration, never persistent run identity. This
//! stub neither mounts nor validates the export and performs no filesystem I/O.
//! Mount qualification, run layout, durability and writer fencing remain pending;
//! the local OrbStack experiment does not establish strict remote persistence.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::path::{Path, PathBuf};

use umbra_core::{Result, UmbraError};
use umbra_storage::{
    AcquireWriterRequest, Durability, DurabilityReceipt, Fencing, FlushRequest, OpenRunRequest,
    RunBinding, Storage, StorageCapabilities, StorageRequest, StorageResponse, WriterLease,
};

/// Operational configuration for an existing NFSv4 mount, preserving native path bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NfsStorageConfig {
    /// Physical mount root; must be validated before a future implementation opens a run.
    pub mount_root: PathBuf,
}

impl NfsStorageConfig {
    /// Construct this value from the supplied configuration or fields.
    pub fn new(mount_root: impl Into<PathBuf>) -> Self {
        Self {
            mount_root: mount_root.into(),
        }
    }
}

/// Unimplemented NFSv4 storage provider. Construction only retains configuration.
#[derive(Debug)]
pub struct NfsStorage {
    config: NfsStorageConfig,
}

impl NfsStorage {
    /// Construct this value from the supplied configuration or fields.
    pub fn new(config: NfsStorageConfig) -> Self {
        Self { config }
    }

    /// Return the configured, as-yet unvalidated physical mount root.
    pub fn mount_root(&self) -> &Path {
        &self.config.mount_root
    }
}

impl Storage for NfsStorage {
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            durability: Durability::None,
            strict_remote_persistence: false,
            fencing: Fencing::ReadOnly,
            kernel_shadow: false,
            complete_emulation: false,
            hard_links: false,
            logical_symlinks: false,
            xattrs: false,
            atomic_replace: false,
            atomic_swap: false,
            max_io_bytes: 0,
            max_directory_entries: 0,
        }
    }

    fn open_run(&mut self, _request: &OpenRunRequest) -> Result<RunBinding> {
        Err(UmbraError::not_implemented("nfs.open_run"))
    }

    fn acquire_writer(&mut self, _request: &AcquireWriterRequest) -> Result<WriterLease> {
        Err(UmbraError::not_implemented("nfs.acquire_writer"))
    }

    fn renew_writer(&mut self, _lease: &WriterLease) -> Result<WriterLease> {
        Err(UmbraError::not_implemented("nfs.renew_writer"))
    }

    fn release_writer(&mut self, _lease: &WriterLease) -> Result<()> {
        Err(UmbraError::not_implemented("nfs.release_writer"))
    }

    fn execute(&mut self, _request: &StorageRequest) -> Result<StorageResponse> {
        Err(UmbraError::not_implemented("nfs.execute"))
    }

    fn flush(&mut self, _request: &FlushRequest) -> Result<DurabilityReceipt> {
        Err(UmbraError::not_implemented("nfs.flush"))
    }

    fn close_run(&mut self) -> Result<()> {
        Err(UmbraError::not_implemented("nfs.close_run"))
    }
}
