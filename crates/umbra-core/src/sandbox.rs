//! Shared sandbox-policy DTOs carried by a supervised launch.
//!
//! Rendering belongs to the supervisor, installation to the platform backend.
//! Nothing here reads a template, touches the filesystem, or installs a policy:
//! these values only carry an already-rendered, bounded policy plus the single
//! writable root it grants, so a platform can validate what it is asked to apply.

use serde::{Deserialize, Serialize};

use crate::{BytePath, ErrorKind, Result, UmbraError};

/// Rendered profile bytes accepted by a platform installer, before argv framing.
pub const MAX_SANDBOX_PROFILE_BYTES: usize = 64 * 1024;

/// The only profile format Umbra renders today: an Apple Seatbelt `(version 1)`
/// policy whose sole persistent write allowance is one per-run root subpath.
pub const SEATBELT_PROFILE_FORMAT: &str = "seatbelt-v1";

/// A rendered enforcement policy for exactly one run root.
///
/// `source` is complete policy text with every template token already resolved;
/// a platform installs it verbatim and must not edit, extend or re-render it.
/// `write_root` restates the single writable root the policy grants so the
/// installer can check it against the run binding it was handed independently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SandboxProfileWire", into = "SandboxProfileWire")]
pub struct SandboxProfile {
    format: String,
    source: Vec<u8>,
    write_root: BytePath,
}

#[derive(Serialize, Deserialize)]
struct SandboxProfileWire {
    format: String,
    source: Vec<u8>,
    write_root: BytePath,
}

impl SandboxProfile {
    /// Validate representation only: format identity, bounded NUL-free source and
    /// an absolute write root. Whether that root exists, is a directory, or is the
    /// run's own root is decided where filesystem and binding knowledge exist.
    pub fn new(
        format: impl Into<String>,
        source: impl Into<Vec<u8>>,
        write_root: BytePath,
    ) -> Result<Self> {
        let format = format.into();
        let source = source.into();
        if format != SEATBELT_PROFILE_FORMAT {
            return Err(invalid("unsupported sandbox profile format"));
        }
        if source.is_empty() || source.len() > MAX_SANDBOX_PROFILE_BYTES {
            return Err(invalid("sandbox profile source exceeds its size bounds"));
        }
        if source.contains(&0) {
            return Err(invalid("sandbox profile source contains a NUL byte"));
        }
        if !write_root.is_absolute() {
            return Err(UmbraError::new(
                ErrorKind::InvalidPath,
                "sandbox_profile",
                "sandbox write root must be absolute",
            ));
        }
        Ok(Self {
            format,
            source,
            write_root,
        })
    }

    /// Format identity the installer must recognize before applying anything.
    pub fn format(&self) -> &str {
        &self.format
    }

    /// Complete rendered policy text; no further substitution is permitted.
    pub fn source(&self) -> &[u8] {
        &self.source
    }

    /// The single persistent write root this policy grants.
    pub fn write_root(&self) -> &BytePath {
        &self.write_root
    }
}

impl TryFrom<SandboxProfileWire> for SandboxProfile {
    type Error = UmbraError;
    fn try_from(wire: SandboxProfileWire) -> Result<Self> {
        Self::new(wire.format, wire.source, wire.write_root)
    }
}

impl From<SandboxProfile> for SandboxProfileWire {
    fn from(profile: SandboxProfile) -> Self {
        Self {
            format: profile.format,
            source: profile.source,
            write_root: profile.write_root,
        }
    }
}

/// Enforcement requirement for one supervised launch.
///
/// Running without a policy is a named, explicit selection that only direct
/// lower-level tracer experiments may make. There is deliberately no default and
/// no `Option`: a launch cannot become unsandboxed by omitting a field, and
/// `umbra run` never constructs [`SandboxRequirement::UnsandboxedExperiment`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxRequirement {
    /// Install this profile and stop the target before its first instruction.
    /// A backend that cannot prove that boundary must fail the launch.
    Required(SandboxProfile),
    /// Explicitly unsandboxed direct-tracer experiment. Backends must reject this
    /// on the supervised contract path; it exists for lower-level tracer tests.
    UnsandboxedExperiment,
}

fn invalid(context: &str) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidInput, "sandbox_profile", context)
}
