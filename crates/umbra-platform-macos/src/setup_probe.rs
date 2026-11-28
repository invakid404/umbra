//! Qualification rendezvous around a real debugger request before session acceptance.
use std::path::PathBuf;
use umbra_core::{
    managed::{Participant, Role},
    Result,
};
use umbra_isolation::managed::{
    boot_nanos,
    process::{identity_with_parent, probe_stop_current, signal_current},
};

pub(crate) struct Hold {
    directory: PathBuf,
    debugger: Participant,
}
impl Hold {
    pub fn begin(body: &str, pid: u32) -> Result<Option<Self>> {
        if body != "vCont?" {
            return Ok(None);
        }
        let Some(directory) = std::env::var_os("UMBRA_NATIVE_SETUP_PROBE") else {
            return Ok(None);
        };
        let directory = PathBuf::from(directory);
        if directory.join("setup.json").exists() {
            return Ok(None);
        }
        let (debugger, parent) = identity_with_parent(pid, Role::Provider)
            .map_err(|e| crate::error("native setup probe identity", e))?;
        if parent != std::process::id() {
            return Err(crate::error(
                "native setup probe",
                "debugger is not an owned child",
            ));
        }
        probe_stop_current(debugger).map_err(|e| crate::error("native setup probe stop", e))?;
        let hold = Self {
            directory,
            debugger,
        };
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let mut status = 0;
            let waited =
                unsafe { libc::waitpid(pid as i32, &mut status, libc::WUNTRACED | libc::WNOHANG) };
            if waited == pid as i32 {
                if libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGSTOP {
                    break;
                }
                return Err(crate::error(
                    "native setup probe",
                    "debugger exited before setup hold",
                ));
            }
            if waited < 0 || std::time::Instant::now() >= limit {
                return Err(crate::error(
                    "native setup probe",
                    "debugger stop was not observed",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        hold.write(
            "setup",
            serde_json::json!({
                "provider": parent, "debugger": debugger, "request": body,
                "nanos": boot_nanos().map_err(|e| crate::error("native setup clock", e))?,
            }),
        )?;
        Ok(Some(hold))
    }
    fn write(&self, name: &str, value: serde_json::Value) -> Result<()> {
        let path = self.directory.join(format!("{name}.tmp"));
        let bytes =
            serde_json::to_vec(&value).map_err(|e| crate::error("native setup probe", e))?;
        std::fs::write(&path, bytes).map_err(|e| crate::error("native setup probe", e))?;
        std::fs::rename(path, self.directory.join(format!("{name}.json")))
            .map_err(|e| crate::error("native setup probe", e))
    }
    pub fn finish(&self, result: &Result<String>, cutoff: Result<Option<u64>>) -> Result<()> {
        self.write(
            "observed",
            serde_json::json!({
                "nanos": boot_nanos().map_err(|e| crate::error("native setup clock", e))?,
                "cutoff": cutoff?,
                "error": result.as_ref().err().map(ToString::to_string),
                "accepted": result.is_ok(),
            }),
        )
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        // Birth-validated resume also runs on probe I/O failures. The ordinary
        // native owner remains responsible for killing and waiting its child.
        if let Err(error) = signal_current(self.debugger, libc::SIGCONT) {
            eprintln!("native setup probe resume: {error}");
        }
    }
}
