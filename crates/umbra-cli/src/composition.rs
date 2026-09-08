//! Runtime assembly from explicitly installed provider executables, with no backend linking.
use std::os::unix::ffi::OsStrExt;

use umbra_core::{
    provider::{decode, ProviderRegistry},
    BytePath, EnvironmentVariable, ErrorKind, Result, RunId, UmbraError,
};
use umbra_supervisor::{CommandLaunch, RunLaunch, RunObserver, RunSpec, Supervisor};

use crate::commands::run::{invalid, os_bytes, RunArgs};

/// Load bounded registry data; provider options remain opaque to assembly.
pub fn load_registry(path: &std::path::Path) -> Result<ProviderRegistry> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| {
        umbra_core::UmbraError::new(umbra_core::ErrorKind::Io, "registry.open", e.to_string())
    })?;
    let mut bytes = Vec::new();
    file.take(umbra_core::provider::MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            umbra_core::UmbraError::new(umbra_core::ErrorKind::Io, "registry.read", e.to_string())
        })?;
    let registry: ProviderRegistry = decode(&bytes)?;
    registry.validate()?;
    Ok(registry)
}

/// Connect every required role before handing the contracts to the supervisor.
#[cfg(unix)]
pub fn build_supervisor(run_id: RunId, registry: &ProviderRegistry) -> Result<Supervisor> {
    registry.validate()?;
    let timeout = registry.timeout_ms;
    let platform = umbra_platform::provider::connect(registry.get("platform")?, timeout)?;
    let agent = Box::new(umbra_agent::provider::Proxy::connect(
        registry.get("agent")?,
        timeout,
    )?);
    let namespace: Box<dyn umbra_overlay::NamespaceSession + Send> =
        if let Some(descriptor) = registry.providers.get("namespace") {
            Box::new(umbra_overlay::provider::Proxy::connect(
                descriptor, timeout,
            )?)
        } else {
            let storage = Box::new(umbra_storage::provider::Proxy::connect(
                registry.get("storage")?,
                timeout,
            )?);
            let journal = Box::new(umbra_journal::provider::Proxy::connect(
                registry.get("journal")?,
                timeout,
            )?);
            umbra_overlay::standard_namespace(storage, journal)
        };
    Ok(Supervisor::with_namespace(
        run_id,
        platform,
        namespace,
        Some(agent),
    ))
}

/// Unsupported hosts return an explicit error without importing Unix backend types.
#[cfg(not(unix))]
pub fn build_supervisor(_run_id: RunId, _registry: &ProviderRegistry) -> Result<Supervisor> {
    Err(umbra_core::UmbraError::new(
        umbra_core::ErrorKind::UnsupportedCapability,
        "providers",
        "local provider transport requires Unix",
    ))
}

/// Build a run request from validated arguments; performs no provider I/O.
///
/// Path and argument bytes are preserved exactly. The workspace is canonicalized
/// so the base identity and the sandbox both describe one unambiguous directory,
/// and the environment is constructed explicitly: nothing is inherited unless the
/// caller named it.
pub fn run_spec(
    args: RunArgs,
    registry: ProviderRegistry,
    observer: Option<Box<dyn RunObserver>>,
) -> Result<RunSpec> {
    let persistence = args.persistence();
    let workspace = std::fs::canonicalize(&args.workspace).map_err(|e| {
        UmbraError::new(
            ErrorKind::InvalidPath,
            "run.workspace",
            format!("{}: {e}", args.workspace.display()),
        )
    })?;
    if !workspace.is_dir() {
        return Err(invalid(format!(
            "workspace {} is not a directory",
            workspace.display()
        )));
    }
    let mut argv = args.argv.iter().map(os_bytes).collect::<Vec<_>>();
    if argv.is_empty() {
        return Err(invalid("a command is required after `--`"));
    }
    let executable = BytePath::new(argv.remove(0))?;
    if !executable.is_absolute() {
        return Err(UmbraError::new(
            ErrorKind::InvalidPath,
            "run.validate",
            "the command must be an absolute executable path; PATH is not searched",
        ));
    }
    // argv[0] is the executable path itself: the supervised program sees exactly
    // what was asked for, not a shortened basename invented here.
    let mut full_argv = vec![executable.as_bytes().to_vec()];
    full_argv.extend(argv);

    let mut environment = Vec::new();
    for entry in &args.env {
        let bytes = os_bytes(entry);
        let split = bytes
            .iter()
            .position(|b| *b == b'=')
            .ok_or_else(|| invalid("--env expects NAME=VALUE"))?;
        let (name, value) = bytes.split_at(split);
        if name.is_empty() {
            return Err(invalid("--env name must not be empty"));
        }
        environment.push(EnvironmentVariable {
            name: name.to_vec(),
            value: value[1..].to_vec(),
        });
    }
    for name in &args.inherit_env {
        let value = std::env::var_os(name).ok_or_else(|| {
            invalid(format!(
                "--inherit-env {} is not set in this environment",
                name.to_string_lossy()
            ))
        })?;
        environment.push(EnvironmentVariable {
            name: os_bytes(name),
            value: value.as_os_str().as_bytes().to_vec(),
        });
    }

    let cwd = BytePath::new(workspace.as_os_str().as_bytes().to_vec())?;
    Ok(RunSpec {
        registry,
        launch: RunLaunch::Command(CommandLaunch {
            executable,
            argv: full_argv,
            environment,
            cwd,
        }),
        workspace,
        persistence,
        experimental: args.experimental,
        observer,
    })
}
