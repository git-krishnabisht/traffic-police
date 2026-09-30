//! The spill file: body bytes past the memory budget, appended to one file in a private per-run
//! directory, `<temp>/traffic-police-<pid>-<suffix>/` (mode 0700 on Unix). The directory goes
//! away when the store that made it is dropped, from the panic hook and on SIGTERM/SIGHUP
//! ([`remove_all`]), and at the next start if this process died without either
//! ([`remove_stale`]).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bytes::{Bytes, BytesMut};

const PREFIX: &str = "traffic-police-";

/// Spill directories this process created and has not removed yet.
static DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// Where spill directories go (`[storage] spill_dir`); the temp directory when unset.
static PARENT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Puts spill directories (and temporary session recordings) under `dir` from now on.
pub fn set_parent(dir: PathBuf) {
    *PARENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
}

/// Where spill directories go.
pub fn parent() -> PathBuf {
    PARENT.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(std::env::temp_dir)
}

#[derive(Debug)]
pub(crate) struct SpillFile {
    dir: PathBuf,
    /// The file and its length (the offset of the next append).
    file: Mutex<(File, u64)>,
}

impl SpillFile {
    pub(crate) fn create() -> io::Result<SpillFile> {
        Self::create_in(&parent())
    }

    pub(crate) fn create_in(parent: &Path) -> io::Result<SpillFile> {
        let dir = private_dir_in(parent)?;
        let file = match OpenOptions::new().read(true).write(true).create_new(true).open(dir.join("bodies.bin")) {
            Ok(f) => f,
            Err(e) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        tracing::info!("spilling bodies to {}", dir.display());
        Ok(SpillFile { dir, file: Mutex::new((file, 0)) })
    }

    /// Appends the chunks back to back; returns where they start and how long they are.
    pub(crate) fn append(&self, chunks: &[&Bytes]) -> io::Result<(u64, u64)> {
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let (file, len) = &mut *guard;
        let start = *len;
        file.seek(SeekFrom::Start(start))?;
        let mut written = 0u64;
        for c in chunks {
            file.write_all(c)?;
            written += c.len() as u64;
        }
        *len += written;
        Ok((start, written))
    }

    pub(crate) fn read(&self, offset: u64, len: u64, out: &mut BytesMut) -> io::Result<()> {
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let (file, _) = &mut *guard;
        file.seek(SeekFrom::Start(offset))?;
        let start = out.len();
        out.resize(start + len as usize, 0);
        file.read_exact(&mut out[start..])
    }

    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        DIRS.lock().unwrap_or_else(|e| e.into_inner()).retain(|d| d != &self.dir);
    }
}

/// A new private directory for this run (`<temp>/traffic-police-<pid>-<suffix>/`, mode 0700 on
/// Unix), removed by [`remove_all`] or found by [`remove_stale`] if this process dies.
pub fn private_dir() -> io::Result<PathBuf> {
    private_dir_in(&parent())
}

fn private_dir_in(parent: &Path) -> io::Result<PathBuf> {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = parent.join(format!("{PREFIX}{}-{:x}{n:x}", std::process::id(), nanos & 0xffff_ffff_ffff));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&dir)?;
    DIRS.lock().unwrap_or_else(|e| e.into_inner()).push(dir.clone());
    Ok(dir)
}

/// Removes every spill directory of this process (panic hook, SIGTERM, SIGHUP). Stores that are
/// still alive lose their spilled bytes, so call this only on the way out.
pub fn remove_all() {
    let dirs = std::mem::take(&mut *DIRS.lock().unwrap_or_else(|e| e.into_inner()));
    for d in dirs {
        let _ = fs::remove_dir_all(d);
    }
}

/// Removes spill directories left in the temp directory by traffic-police processes that no
/// longer run (killed, or lost power). Unix only: elsewhere they stay until the OS cleans temp.
pub fn remove_stale() {
    remove_stale_in(&parent());
}

pub(crate) fn remove_stale_in(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(PREFIX))
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<u32>().ok())
        else {
            continue;
        };
        let ours = ["bodies.bin", "stream.bin"].iter().any(|f| e.path().join(f).exists());
        if pid != std::process::id() && !alive(pid) && ours {
            tracing::info!("removing a spill directory left by pid {pid}: {}", e.path().display());
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return true };
    // SAFETY: kill with signal 0 only checks for existence and permission; it sends nothing.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tp-spill-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn appends_reads_and_cleans_up() {
        let parent = scratch("rw");
        let f = SpillFile::create_in(&parent).unwrap();
        let dir = f.dir().to_path_buf();
        assert_eq!(f.append(&[&Bytes::from_static(b"hello "), &Bytes::from_static(b"world")]).unwrap(), (0, 11));
        assert_eq!(f.append(&[&Bytes::from_static(b"!")]).unwrap(), (11, 1));
        let mut out = BytesMut::new();
        f.read(6, 5, &mut out).unwrap();
        f.read(11, 1, &mut out).unwrap();
        assert_eq!(&out[..], b"world!");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        drop(f);
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[cfg(unix)]
    #[test]
    fn removes_directories_of_dead_processes_only() {
        let parent = scratch("stale");
        // a pid far above any real one is dead; our own pid is alive
        let dead = parent.join(format!("{PREFIX}{}-abc", 999_999_999u32));
        let ours = parent.join(format!("{PREFIX}{}-def", std::process::id()));
        let unrelated = parent.join("traffic-police-notes");
        for d in [&dead, &ours, &unrelated] {
            fs::create_dir_all(d).unwrap();
            fs::write(d.join("bodies.bin"), b"x").unwrap();
        }
        remove_stale_in(&parent);
        assert!(!dead.exists());
        assert!(ours.exists());
        assert!(unrelated.exists());
        let _ = fs::remove_dir_all(parent);
    }
}
