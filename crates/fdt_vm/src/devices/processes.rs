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
    /// The pid and argv of every process whose parent is pid 1 (it was orphaned) and whose
    /// executable file is named `name`.
    fn orphans(&self, name: &str) -> Vec<(u32, Vec<String>)>;
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

    fn orphans(&self, name: &str) -> Vec<(u32, Vec<String>)> {
        mac::pids()
            .into_iter()
            .filter(|&pid| {
                mac::bsd_info(pid).is_some_and(|i| i.pbi_ppid == 1 && mac::comm(&i) == name)
            })
            .filter_map(|pid| Some((pid, mac::argv(pid)?)))
            .collect()
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

    fn orphans(&self, name: &str) -> Vec<(u32, Vec<String>)> {
        self.pids()
            .into_iter()
            .filter(|pid| linux_ppid(*pid) == Some(1))
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|c| c.trim_end() == name)
            })
            .filter_map(|pid| {
                let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
                let argv = raw
                    .split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect();
                Some((pid, argv))
            })
            .collect()
    }
}

/// The parent pid from `/proc/<pid>/stat`, whose second field (the name) may hold spaces.
#[cfg(target_os = "linux")]
fn linux_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl Processes for HostProcesses {
    fn pids(&self) -> Vec<u32> {
        Vec::new()
    }

    fn exe(&self, _pid: u32) -> Option<ExeInfo> {
        None
    }

    fn orphans(&self, _name: &str) -> Vec<(u32, Vec<String>)> {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, OsStr};
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

    pub(super) fn comm(info: &libc::proc_bsdinfo) -> String {
        let bytes: Vec<u8> = info
            .pbi_comm
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The process's arguments from `KERN_PROCARGS2`: an `int` argc, the executable path, NUL
    /// padding, then argc NUL-terminated strings (the environment follows).
    pub(super) fn argv(pid: u32) -> Option<Vec<String>> {
        let pid = libc::c_int::try_from(pid).ok()?;
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut argmax: libc::c_int = 0;
        let mut size = size_of::<libc::c_int>();
        // SAFETY: `argmax` is `size` bytes; no new value is set.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&raw mut argmax).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || argmax <= 0 {
            return None;
        }
        let mut buf = vec![0u8; argmax as usize];
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: `buf` is `size` bytes; the call writes at most that and updates `size`.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || size < size_of::<libc::c_int>() {
            return None;
        }
        let buf = &buf[..size];
        let argc = libc::c_int::from_ne_bytes(buf[..4].try_into().ok()?);
        let argc = usize::try_from(argc).ok()?;
        let mut rest = &buf[4..];
        // Skip the executable path and the NUL padding after it.
        let path_end = rest.iter().position(|b| *b == 0)?;
        rest = &rest[path_end..];
        let start = rest.iter().position(|b| *b != 0)?;
        rest = &rest[start..];
        let mut argv = Vec::with_capacity(argc);
        for _ in 0..argc {
            let s = CStr::from_bytes_until_nul(rest).ok()?;
            argv.push(s.to_string_lossy().into_owned());
            rest = &rest[s.to_bytes().len() + 1..];
        }
        Some(argv)
    }
}

/// A fixed process list for tests; counts how often an executable is read.
#[cfg(all(test, unix))]
#[derive(Debug, Default)]
pub(crate) struct FakeProcesses {
    pub(crate) list: std::sync::Mutex<Vec<(u32, ExeInfo)>>,
    pub(crate) exe_calls: std::sync::atomic::AtomicUsize,
    pub(crate) orphans: std::sync::Mutex<Vec<(u32, Vec<String>)>>,
}

#[cfg(all(test, unix))]
impl FakeProcesses {
    pub(crate) fn set(&self, list: Vec<(u32, ExeInfo)>) {
        *self.list.lock().unwrap() = list;
    }
}

#[cfg(all(test, unix))]
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

    fn orphans(&self, _name: &str) -> Vec<(u32, Vec<String>)> {
        self.orphans.lock().unwrap().clone()
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

    #[test]
    fn orphans_skip_processes_with_a_live_parent() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        // Our child's parent is this test process, so it is not an orphan.
        assert!(
            !HostProcesses
                .orphans("sleep")
                .iter()
                .any(|(p, _)| *p == child.id())
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn argv_reads_each_argument() {
        // `read` is a shell builtin: the shell itself waits on stdin, with no child of its own.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read x", "arg one", "*:S"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        assert_eq!(
            mac::argv(child.id()),
            Some(
                ["/bin/sh", "-c", "read x", "arg one", "*:S"]
                    .map(String::from)
                    .to_vec()
            )
        );
        drop(child.stdin.take());
        child.wait().unwrap();
        assert_eq!(mac::argv(u32::MAX), None);
    }
}
