//! Bounded custody for measurement children, including early error returns.
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

pub struct OwnedChild {
    child: Child,
    deadline: Instant,
    collected: bool,
}
impl OwnedChild {
    pub fn new(child: Child, deadline: Instant) -> Self {
        Self {
            child,
            deadline,
            collected: false,
        }
    }
    pub fn id(&self) -> u32 {
        self.child.id()
    }
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let result = self.child.try_wait()?;
        if result.is_some() {
            self.collected = true;
        }
        Ok(result)
    }
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }
    pub fn collect_until(&mut self, deadline: Instant) -> std::io::Result<Option<ExitStatus>> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline.min(self.deadline) {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.collected {
            return;
        }
        let _ = self.child.kill();
        // This fallback belongs to the original pair deadline. It neither
        // renews it nor creates a detached, unbounded wait task.
        let until = (Instant::now() + Duration::from_secs(1)).min(self.deadline);
        match self.collect_until(until) {
            Ok(Some(status)) => eprintln!(
                "measurement error cleanup: child {} collected {status}",
                self.id()
            ),
            result => eprintln!(
                "MEASUREMENT GUARD FAILURE: child {} collection unconfirmed: {result:?}",
                self.id()
            ),
        }
    }
}
