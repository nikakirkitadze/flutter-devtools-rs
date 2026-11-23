//! Supervised child processes that stream their stdout lines, a one-shot command runner, and
//! the record of the children both started, so a shutdown can ask them to stop.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
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

/// The child processes the watchers started and have not reaped yet. Once
/// [`terminate`](Children::terminate) ran, no new child is let run.
#[derive(Clone, Debug, Default)]
pub struct Children(Arc<Mutex<ChildSet>>);

#[derive(Debug, Default)]
struct ChildSet {
    pids: HashSet<u32>,
    stopped: bool,
}

impl Children {
    fn lock(&self) -> std::sync::MutexGuard<'_, ChildSet> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether [`terminate`](Children::terminate) has run.
    pub fn is_stopped(&self) -> bool {
        self.lock().stopped
    }

    /// The recorded children.
    pub fn pids(&self) -> Vec<u32> {
        self.lock().pids.iter().copied().collect()
    }

    /// Lets no new child run and sends SIGTERM to every recorded one. Returns their pids.
    ///
    /// SIGTERM, not SIGKILL: `simctl spawn` passes SIGTERM on to the process it started inside
    /// the simulator, which SIGKILL would leave running (design F14).
    pub fn terminate(&self) -> Vec<u32> {
        let mut set = self.lock();
        set.stopped = true;
        let pids: Vec<u32> = set.pids.iter().copied().collect();
        drop(set);
        for pid in &pids {
            sigterm(*pid);
        }
        pids
    }

    /// Records a child that just started, until the returned guard drops (once it was reaped,
    /// or its task was aborted). After `terminate`, sends the child SIGTERM instead and returns
    /// `None`; the caller then waits for it to exit.
    fn track(&self, pid: u32) -> Option<Tracked> {
        let mut set = self.lock();
        if set.stopped {
            drop(set);
            sigterm(pid);
            return None;
        }
        set.pids.insert(pid);
        Some(Tracked {
            children: self.clone(),
            pid,
        })
    }
}

/// A recorded child; dropping it forgets the child.
pub(crate) struct Tracked {
    children: Children,
    pid: u32,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.children.lock().pids.remove(&self.pid);
    }
}

/// Starts `cmd` (configured with `kill_on_drop`) and records it in `children`. `Ok(None)` when
/// the watchers are stopping: then nothing starts, or the child is asked to stop and reaped.
pub(crate) async fn spawn_tracked(
    cmd: &mut Command,
    children: &Children,
) -> std::io::Result<Option<(tokio::process::Child, Tracked)>> {
    if children.is_stopped() {
        return Ok(None);
    }
    let mut child = cmd.spawn()?;
    let Some(pid) = child.id() else {
        return Ok(None);
    };
    match children.track(pid) {
        Some(tracked) => Ok(Some((child, tracked))),
        None => {
            let _ = child.wait().await;
            Ok(None)
        }
    }
}

/// Sends SIGTERM to `pid`.
#[cfg(unix)]
pub(crate) fn sigterm(pid: u32) {
    // kill(0, _) would signal the whole process group.
    if let Ok(pid) = libc::pid_t::try_from(pid)
        && pid > 0
    {
        // SAFETY: plain syscall with no pointers.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

/// No signals here; `kill_on_drop` ends the children.
#[cfg(not(unix))]
pub(crate) fn sigterm(_pid: u32) {}

/// Whether our child `pid` has exited, without reaping it (`waitid` with `WNOWAIT`), so tokio
/// still collects its status. A pid that is no longer our child counts as exited.
#[cfg(unix)]
pub(crate) fn has_exited(pid: u32) -> bool {
    let id = libc::id_t::from(pid);
    // SAFETY: `siginfo_t` is plain data, so all zeroes is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid `siginfo_t` the call fills in.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // ECHILD: already reaped. Otherwise a zero pid means it is still running.
    // SAFETY: `waitid` filled in `info` (or left it zeroed).
    rc != 0 || unsafe { info.si_pid() } != 0
}

#[cfg(not(unix))]
pub(crate) fn has_exited(_pid: u32) -> bool {
    true
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
/// starts it again. Stops when the returned [`Follower`] is dropped, `lines` is closed, or
/// `children` was terminated. Each child is recorded in `children` while it runs.
pub fn follow(
    handle: &tokio::runtime::Handle,
    program: PathBuf,
    args: Vec<String>,
    restart: Backoff,
    lines: UnboundedSender<String>,
    children: Children,
) -> Follower {
    let task = handle.spawn(async move {
        let mut delay = restart.first;
        loop {
            let started = Instant::now();
            if !run_child(&program, &args, &lines, &children).await || children.is_stopped() {
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
async fn run_child(
    program: &Path,
    args: &[String],
    lines: &UnboundedSender<String>,
    children: &Children,
) -> bool {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(Some((mut child, _tracked))) = spawn_tracked(&mut cmd, children).await else {
        return true;
    };
    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            // Raw bytes: logs can hold truncated multibyte characters, which must not end us.
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if buf.ends_with(b"\n") {
                buf.pop();
                if buf.ends_with(b"\r") {
                    buf.pop();
                }
            }
            if lines
                .send(String::from_utf8_lossy(&buf).into_owned())
                .is_err()
            {
                return false;
            }
        }
    }
    let _ = child.wait().await;
    true
}

/// Runs a command to completion and returns its stdout. `None` on a spawn error, a non-zero
/// exit, a timeout (the child is killed then), or once `children` was terminated. The child is
/// recorded in `children` while it runs.
pub async fn run_once(
    program: &Path,
    args: &[&str],
    timeout: Duration,
    children: &Children,
) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let (child, _tracked) = spawn_tracked(&mut cmd, children).await.ok()??;
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::discovery::pid_alive;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn sh(script: &str) -> (PathBuf, Vec<String>) {
        (
            PathBuf::from("/bin/sh"),
            vec!["-c".to_string(), script.to_string()],
        )
    }

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
            Children::default(),
        );
        for want in ["a", "b"] {
            let got = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap();
            assert_eq!(got.as_deref(), Some(want));
        }
    }

    #[tokio::test]
    async fn follower_survives_invalid_utf8() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (p, a) = sh("printf 'a\\377b\\nok\\n'; sleep 5");
        let _f = follow(
            &tokio::runtime::Handle::current(),
            p,
            a,
            Backoff::default(),
            tx,
            Children::default(),
        );
        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(first.starts_with('a') && first.ends_with('b'), "{first:?}");
        let second = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert_eq!(second.as_deref(), Some("ok"));
    }

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
            Children::default(),
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
            Children::default(),
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

    #[tokio::test]
    async fn run_once_returns_stdout_and_times_out() {
        let sh = Path::new("/bin/sh");
        let c = Children::default();
        let out = run_once(sh, &["-c", "echo hi"], Duration::from_secs(2), &c).await;
        assert_eq!(out.as_deref(), Some("hi\n"));
        let start = std::time::Instant::now();
        let out = run_once(sh, &["-c", "sleep 5"], Duration::from_millis(200), &c).await;
        assert_eq!(out, None);
        assert!(start.elapsed() < Duration::from_secs(1));
        let missing = Path::new("/definitely/not/a/program");
        assert_eq!(
            run_once(missing, &[], Duration::from_secs(1), &c).await,
            None
        );
        assert!(c.pids().is_empty(), "finished children are forgotten");
    }

    #[tokio::test]
    async fn terminate_sends_sigterm_and_stops_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let children = Children::default();
        let script = format!(
            "trap 'touch \"{}/term\"; exit 0' TERM; echo $$; while :; do sleep 0.05; done",
            dir.path().display()
        );
        let (p, a) = sh(&script);
        let _f = follow(
            &tokio::runtime::Handle::current(),
            p,
            a,
            Backoff {
                first: Duration::from_millis(50),
                max: Duration::from_millis(50),
            },
            tx,
            children.clone(),
        );
        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let pid: u32 = line.trim().parse().unwrap();
        assert_eq!(children.pids(), vec![pid]);
        assert_eq!(children.terminate(), vec![pid]);
        let term = dir.path().join("term");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !term.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(term.exists(), "the child got SIGTERM");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !children.pids().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(children.pids().is_empty(), "the exited child is forgotten");
        // No restart: nothing more arrives after the backoff.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn nothing_starts_after_terminate() {
        let dir = tempfile::tempdir().unwrap();
        let children = Children::default();
        children.terminate();
        let marker = dir.path().join("ran");
        let script = format!("touch \"{}\"", marker.display());
        let out = run_once(
            Path::new("/bin/sh"),
            &["-c", &script],
            Duration::from_secs(2),
            &children,
        )
        .await;
        assert_eq!(out, None);
        assert!(!marker.exists());
        assert!(children.pids().is_empty());
    }

    #[test]
    fn has_exited_sees_an_exit_without_reaping_it() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert!(!has_exited(child.id()));
        sigterm(child.id());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !has_exited(child.id()) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(has_exited(child.id()));
        // Still waitable: `has_exited` did not reap it.
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
        assert!(has_exited(child.id()), "a reaped child counts as exited");
    }
}
