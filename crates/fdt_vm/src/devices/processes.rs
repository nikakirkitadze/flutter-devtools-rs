//! Host processes, read in-process from the kernel, so watching them starts no child process.

use std::path::PathBuf;
use std::time::SystemTime;

/// What the simulator watcher needs to know about one host process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExeInfo {
    /// The executable's path.
    pub path: PathBuf,
    /// When the process started, if the system said.
    pub started: Option<SystemTime>,
}

/// Lists host processes. [`HostProcesses`] asks the kernel; tests stand in a fixed list.
pub trait Processes: Send + Sync + std::fmt::Debug {
    /// The pids of all processes.
    fn pids(&self) -> Vec<u32>;
    /// The executable and start time of `pid`, if it can be read.
    fn exe(&self, pid: u32) -> Option<ExeInfo>;
}

/// The processes of this machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostProcesses;

#[cfg(target_os = "macos")]
impl Processes for HostProcesses {
    fn pids(&self) -> Vec<u32> {
        mac::pids()
    }

    fn exe(&self, pid: u32) -> Option<ExeInfo> {
        Some(ExeInfo {
            path: mac::path(pid)?,
            started: mac::bsd_info(pid).map(|i| {
                SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(i.pbi_start_tvsec)
                    + std::time::Duration::from_micros(i.pbi_start_tvusec)
            }),
        })
    }
}

#[cfg(target_os = "linux")]
impl Processes for HostProcesses {
    fn pids(&self) -> Vec<u32> {
        let Ok(rd) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect()
    }

    fn exe(&self, pid: u32) -> Option<ExeInfo> {
        Some(ExeInfo {
            path: std::fs::read_link(format!("/proc/{pid}/exe")).ok()?,
            started: None,
        })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl Processes for HostProcesses {
    fn pids(&self) -> Vec<u32> {
        Vec::new()
    }

    fn exe(&self, _pid: u32) -> Option<ExeInfo> {
        None
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    pub(super) fn pids() -> Vec<u32> {
        // SAFETY: a null buffer only asks how many pids there are.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let Ok(n) = usize::try_from(n) else {
            return Vec::new();
        };
        // Room for processes started since the count.
        let mut buf: Vec<libc::pid_t> = vec![0; n + 64];
        let Ok(bytes) = libc::c_int::try_from(buf.len() * size_of::<libc::pid_t>()) else {
            return Vec::new();
        };
        // SAFETY: `buf` holds `bytes` bytes; the call writes at most that many and returns how
        // many pids it wrote.
        let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) };
        let Ok(got) = usize::try_from(got) else {
            return Vec::new();
        };
        buf.truncate(got.min(buf.len()));
        buf.into_iter()
            .filter_map(|p| u32::try_from(p).ok())
            .filter(|&p| p != 0)
            .collect()
    }

    pub(super) fn path(pid: u32) -> Option<PathBuf> {
        let pid = libc::c_int::try_from(pid).ok()?;
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `buf` is `PROC_PIDPATHINFO_MAXSIZE` bytes, the size the call is told.
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        let n = usize::try_from(n)
            .ok()
            .filter(|&n| n > 0 && n <= buf.len())?;
        Some(PathBuf::from(OsStr::from_bytes(&buf[..n])))
    }

    pub(super) fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
        let pid = libc::c_int::try_from(pid).ok()?;
        // SAFETY: `proc_bsdinfo` is plain data, so all zeroes is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is `size` bytes, the size the call is told.
        let n = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        (n == size).then_some(info)
    }
}

/// A fixed process list for tests; counts how often an executable is read.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeProcesses {
    pub(crate) list: std::sync::Mutex<Vec<(u32, ExeInfo)>>,
    pub(crate) exe_calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl FakeProcesses {
    pub(crate) fn set(&self, list: Vec<(u32, ExeInfo)>) {
        *self.list.lock().unwrap() = list;
    }
}

#[cfg(test)]
impl Processes for FakeProcesses {
    fn pids(&self) -> Vec<u32> {
        self.list.lock().unwrap().iter().map(|(p, _)| *p).collect()
    }

    fn exe(&self, pid: u32) -> Option<ExeInfo> {
        self.exe_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let list = self.list.lock().unwrap();
        list.iter().find(|(p, _)| *p == pid).map(|(_, i)| i.clone())
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn host_processes_include_a_child_with_its_path() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let procs = HostProcesses;
        assert!(procs.pids().contains(&pid));
        assert!(procs.pids().contains(&std::process::id()));
        let info = procs.exe(pid).expect("our child is readable");
        assert_eq!(info.path.file_name().unwrap(), "sleep");
        if let Some(started) = info.started {
            assert!(started <= SystemTime::now());
            let age = SystemTime::now().duration_since(started).unwrap();
            assert!(age < std::time::Duration::from_secs(60), "{age:?}");
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!procs.pids().contains(&pid));
        assert_eq!(procs.exe(pid), None);
    }
}
