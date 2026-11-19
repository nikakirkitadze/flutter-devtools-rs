//! The tokio runtime all capture work runs on.

use std::io;

use tokio::runtime::{Builder, Handle, Runtime};

/// A multi-thread runtime with two workers named `fdt-capture`.
pub struct CaptureRuntime {
    rt: Option<Runtime>,
}

impl CaptureRuntime {
    pub fn new() -> io::Result<CaptureRuntime> {
        let rt = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("fdt-capture")
            .enable_all()
            .build()?;
        Ok(CaptureRuntime { rt: Some(rt) })
    }

    pub fn handle(&self) -> Handle {
        self.rt
            .as_ref()
            .expect("runtime lives until drop")
            .handle()
            .clone()
    }
}

impl Drop for CaptureRuntime {
    fn drop(&mut self) {
        // Never block the caller (often the UI thread) waiting for tasks to wind down.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_are_named() {
        let rt = CaptureRuntime::new().unwrap();
        let name = rt.handle().block_on(async {
            tokio::spawn(async { std::thread::current().name().map(String::from) })
                .await
                .unwrap()
        });
        assert_eq!(name.as_deref(), Some("fdt-capture"));
    }

    #[test]
    fn drop_does_not_block_on_a_busy_task() {
        let rt = CaptureRuntime::new().unwrap();
        rt.handle().spawn(async {
            std::thread::sleep(std::time::Duration::from_secs(3));
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        let t = std::time::Instant::now();
        drop(rt);
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
    }
}
