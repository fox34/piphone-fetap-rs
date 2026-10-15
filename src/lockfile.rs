//! A lock file, so that only one instance of the phone runs at a time.
//!
//! Two instances on one machine fight over the hardware: both open the GPIO
//! lines, both try to bind the web port, and the one that loses the port is not
//! told why it is not answering. The lock is taken before any of that happens,
//! so the second instance says so and stops instead.
//!
//! The lock is a `flock` on the file, not the mere existence of it: the kernel
//! releases a `flock` when the last descriptor is closed, including when the
//! process is killed. A file that only had to exist would have to be cleaned up
//! on the way out, and the way out that matters here - a crash, a `kill -9`, a
//! power cut - is exactly the one that never runs cleanup code, so the phone
//! would refuse to start until somebody deleted the file by hand. On a device
//! that hangs up in a hallway, that is not a check anybody does.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The lock file used when the caller does not name one.
///
/// `/run/lock` is where the FHS puts lock files (`/var/lock` is a symlink to
/// it), it is a tmpfs, and it is cleared on every boot. Off Linux - where the
/// tests run - the temp directory stands in for it.
pub fn default_path() -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from("/run/lock/piphone.lock")
    } else {
        std::env::temp_dir().join("piphone.lock")
    }
}

/// An exclusive claim on the lock file.
///
/// Held for as long as it is alive; the claim goes away when it is dropped, or
/// when the process ends without dropping it.
pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Claim the lock at `path`, or explain why not.
    ///
    /// The file is created if it is missing, and only truncated once the lock
    /// is held: its contents are the process id of whoever holds it, and that
    /// is what a refused instance reports.
    pub fn acquire(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("Sperrdatei {} nicht zu öffnen: {}", path.display(), e))?;

        // Safety: `file` owns a valid descriptor and outlives the call, and the
        // descriptor is not used again while the lock is being taken.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            // EWOULDBLOCK is EAGAIN on both Linux and macOS: the file is locked,
            // rather than there being anything wrong with it.
            if error.kind() == ErrorKind::WouldBlock {
                return Err(match holder(&file) {
                    Some(pid) => format!(
                        "Es läuft bereits eine Instanz von PiPhone (Prozess {}).",
                        pid
                    ),
                    None => "Es läuft bereits eine Instanz von PiPhone.".to_string(),
                });
            }
            return Err(format!(
                "Sperrdatei {} nicht zu sperren: {}",
                path.display(),
                error
            ));
        }

        // The lock is ours, so the file gets our id. A reader that finds the
        // lock held can therefore say which process it is.
        let mut lock = InstanceLock {
            file,
            path: path.to_path_buf(),
        };
        if let Err(error) = lock.write_pid() {
            // The claim is the important part and it is held; failing to
            // annotate the file is not worth refusing to run over.
            eprintln!(
                "Warnung: Prozessnummer nicht in {} schreibbar: {}",
                lock.path.display(),
                error
            );
        }
        Ok(lock)
    }

    /// The path this lock was taken on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write_pid(&mut self) -> std::io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        writeln!(self.file, "{}", std::process::id())
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Unlocking explicitly rather than leaving it to the close, so that the
        // claim does not outlive this struct by accident of the field order.
        // Safety: as in `acquire` - a valid descriptor, still owned here.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        // The file itself stays. It is in a tmpfs and costs a few bytes, while
        // removing it races with the next instance, which may already have
        // opened it and would then hold a lock on a file nobody can name.
    }
}

/// The process id written into a held lock file, if it is readable and a number.
///
/// Only ever called on a file whose lock could not be taken, so it never blocks
/// and never sees a half-written line: the holder writes its id after it has
/// the lock, and truncates before it does.
fn holder(file: &File) -> Option<u32> {
    // `try_clone` shares the offset with the original, and the holder left it at
    // the end of the id it wrote, so the read has to start over.
    let mut clone = file.try_clone().ok()?;
    clone.seek(SeekFrom::Start(0)).ok()?;
    let mut text = String::new();
    clone.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_in(dir: &tempfile::TempDir) -> InstanceLock {
        InstanceLock::acquire(&dir.path().join("piphone.lock"))
            .expect("die Sperrdatei ist zu bekommen")
    }

    #[test]
    fn the_second_instance_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _first = lock_in(&dir);

        let error = InstanceLock::acquire(&dir.path().join("piphone.lock"))
            .err()
            .expect("eine zweite Instanz muss abgewiesen werden");

        assert!(
            error.contains(&std::process::id().to_string()),
            "die Meldung nennt den Prozess, der läuft, aber sie lautet: {}",
            error
        );
    }

    #[test]
    fn the_lock_is_free_again_after_it_was_dropped() {
        let dir = tempfile::tempdir().unwrap();
        drop(lock_in(&dir));

        let second = lock_in(&dir);
        assert_eq!(second.path(), dir.path().join("piphone.lock"));
    }

    #[test]
    fn the_held_lock_names_this_process() {
        let dir = tempfile::tempdir().unwrap();
        let lock = lock_in(&dir);

        let mut clone = lock.file.try_clone().unwrap();
        clone.seek(SeekFrom::Start(0)).unwrap();
        let mut text = String::new();
        clone.read_to_string(&mut text).expect("zu lesen");

        assert_eq!(text.trim(), std::process::id().to_string());
        assert_eq!(holder(&lock.file), Some(std::process::id()));
    }

    #[test]
    fn a_lock_file_without_a_pid_is_still_refused() {
        // The holder was killed before it could write anything. The lock is
        // still held, so the file is still authoritative and the refusal must
        // not depend on being able to read it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("piphone.lock");
        let _first = lock_in(&dir);
        std::fs::write(&path, "").unwrap();

        let error = InstanceLock::acquire(&path)
            .err()
            .expect("muss abgewiesen werden");
        assert!(error.contains("bereits"), "{}", error);
    }

    #[test]
    fn the_default_location_is_the_linux_one() {
        let path = default_path();
        if cfg!(target_os = "linux") {
            assert_eq!(path, Path::new("/run/lock/piphone.lock"));
        } else {
            assert_eq!(path, std::env::temp_dir().join("piphone.lock"));
        }
    }
}
