//! Supervised child processes that stream their stdout lines, and a one-shot command runner.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// A child that has run at least this long restarts after `Backoff::first` again.
const STABLE_RUN: Duration = Duration::from_secs(30);

/// Restart delays: start at `first`, double after each exit, never exceed `max`.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            first: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

/// Handle to a supervised child process. Dropping it kills the child and stops restarting.
pub struct Follower {
    task: JoinHandle<()>,
}

impl Drop for Follower {
    fn drop(&mut self) {
        // The child handle lives inside the task and is `kill_on_drop`, so aborting the task
        // drops it and kills the process.
        self.task.abort();
    }
}

/// Runs `program args` with stdout piped, sending every stdout line (without its newline) to
/// `lines`. When the child exits, or fails to start, it waits for the next backoff delay and
/// starts it again. Stops when the returned [`Follower`] is dropped or `lines` is closed.
pub fn follow(
    handle: &tokio::runtime::Handle,
    program: PathBuf,
    args: Vec<String>,
    restart: Backoff,
    lines: UnboundedSender<String>,
) -> Follower {
    let task = handle.spawn(async move {
        let mut delay = restart.first;
        loop {
            let started = Instant::now();
            if !run_child(&program, &args, &lines).await {
                return;
            }
            if started.elapsed() >= STABLE_RUN {
                delay = restart.first;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(restart.max);
        }
    });
    Follower { task }
}

/// Runs the child once, forwarding its lines. Returns false when the receiver is gone.
async fn run_child(program: &Path, args: &[String], lines: &UnboundedSender<String>) -> bool {
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = spawned else {
        return true;
    };
    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if lines.send(line).is_err() {
                return false;
            }
        }
    }
    let _ = child.wait().await;
    true
}

/// Runs a command to completion and returns its stdout. `None` on a spawn error, a non-zero
/// exit, or a timeout (the child is killed then).
pub async fn run_once(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    // On timeout the future, and with it the child handle, is dropped, which kills the child.
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::pid_alive;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[cfg(unix)]
    fn sh(script: &str) -> (PathBuf, Vec<String>) {
        (
            PathBuf::from("/bin/sh"),
            vec!["-c".to_string(), script.to_string()],
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn follower_sends_each_line() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (p, a) = sh("printf 'a\\nb\\n'; sleep 5");
        let _f = follow(
            &tokio::runtime::Handle::current(),
            p,
            a,
            Backoff::default(),
            tx,
        );
        for want in ["a", "b"] {
            let got = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap();
            assert_eq!(got.as_deref(), Some(want));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn follower_restarts_with_backoff_and_stops_on_drop() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (p, a) = sh("echo x; exit 1");
        let f = follow(
            &tokio::runtime::Handle::current(),
            p,
            a,
            Backoff {
                first: Duration::from_millis(50),
                max: Duration::from_millis(200),
            },
            tx,
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut n = 0;
        while let Ok(l) = rx.try_recv() {
            assert_eq!(l, "x");
            n += 1;
        }
        assert!((4..=12).contains(&n), "got {n} restarts");
        drop(f);
        // Allow a line already in flight, then expect silence.
        tokio::time::sleep(Duration::from_millis(100)).await;
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(rx.try_recv().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_follower_kills_the_child() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (p, a) = sh("echo $$; sleep 30");
        let f = follow(
            &tokio::runtime::Handle::current(),
            p,
            a,
            Backoff::default(),
            tx,
        );
        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let pid: u32 = line.trim().parse().unwrap();
        assert!(pid_alive(pid));
        drop(f);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while pid_alive(pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!pid_alive(pid));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_once_returns_stdout_and_times_out() {
        let sh = Path::new("/bin/sh");
        let out = run_once(sh, &["-c", "echo hi"], Duration::from_secs(2)).await;
        assert_eq!(out.as_deref(), Some("hi\n"));
        let start = std::time::Instant::now();
        let out = run_once(sh, &["-c", "sleep 5"], Duration::from_millis(200)).await;
        assert_eq!(out, None);
        assert!(start.elapsed() < Duration::from_secs(1));
        let missing = Path::new("/definitely/not/a/program");
        assert_eq!(run_once(missing, &[], Duration::from_secs(1)).await, None);
    }
}
