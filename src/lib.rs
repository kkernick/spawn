#![doc = include_str!("../README.md")]

mod handle;
mod spawn;

use caps::{CapSet, CapsHashSet};
use log::warn;
use nix::unistd::{dup, pipe};
use std::{
    fs::File,
    os::fd::{AsFd, OwnedFd},
    sync::LazyLock,
};

pub use handle::{Error as HandleError, Handle, Stream};
pub use spawn::{Error as SpawnError, Spawner, StreamMode};

/// An `OwnedFd` pointing to /dev/null, duplicated for
/// `StreamMode::Discard`.
static NULL: LazyLock<OwnedFd> = LazyLock::new(|| {
    File::open("/dev/null")
        .expect("Failed to open /dev/null")
        .into()
});

/// The current processes' Ambient Set.
static AMBIENT: LazyLock<CapsHashSet> =
    LazyLock::new(|| caps::read(None, CapSet::Ambient).unwrap_or_default());

/// The current processes' Effective Set.
static EFFECTIVE: LazyLock<CapsHashSet> =
    LazyLock::new(|| caps::read(None, CapSet::Effective).unwrap_or_default());

/// The current processes' Inheritable Set.
static INHERITABLE: LazyLock<CapsHashSet> =
    LazyLock::new(|| caps::read(None, CapSet::Inheritable).unwrap_or_default());

/// The current processes' Permitted Set.
static PERMITTED: LazyLock<CapsHashSet> =
    LazyLock::new(|| caps::read(None, CapSet::Permitted).unwrap_or_default());

/// Clears the capabilities of the current thread.
fn clear_capabilities(diff: &CapsHashSet) {
    for (set, caps) in [
        (CapSet::Ambient, AMBIENT.intersection(diff)),
        (CapSet::Effective, EFFECTIVE.intersection(diff)),
        (CapSet::Inheritable, INHERITABLE.intersection(diff)),
        (CapSet::Permitted, PERMITTED.intersection(diff)),
    ] {
        for cap in caps {
            if let Err(e) = caps::drop(None, set, *cap) {
                warn!("Could not drop {cap}: {e}");
            }
        }
    }
}

/// Create a duplicate FD pointing to /dev/null
fn dup_null() -> Result<OwnedFd, SpawnError> {
    dup(NULL.as_fd()).map_err(|e| SpawnError::Errno(None, "dup", e))
}

/// Conditionally create a pipe.
/// Returns either a set of `None`, or the result of `pipe()`
fn cond_pipe(cond: &StreamMode) -> Result<Option<(OwnedFd, OwnedFd)>, SpawnError> {
    match cond {
        StreamMode::Pipe | StreamMode::Log(_) => match pipe() {
            Ok((r, w)) => Ok(Some((r, w))),
            Err(e) => Err(SpawnError::Errno(None, "pipe", e)),
        },
        _ => Ok(None),
    }
}

/// Log all activity from the child at the desired level.
fn logger(level: log::Level, fd: OwnedFd, name: &str) {
    let stream = Stream::new(fd);
    while let Some(line) = stream.read_line() {
        log::log!(level, "{name}: {line}");
    }
}

#[cfg(test)]
mod tests {
    use crate::{Spawner, StreamMode};
    use anyhow::Result;
    use std::{env, fs, io::Write, path::Path};

    #[test]
    fn bash() -> Result<()> {
        let string = "Hello, World!";
        let mut handle = Spawner::new("bash")?
            .args(["-c", &format!("echo '{string}'")])
            .output(StreamMode::Pipe)
            .error(StreamMode::Pipe)
            .spawn()?;

        let output = handle.output()?.read_blocking()?;
        assert_eq!(output.trim(), string);
        Ok(())
    }

    #[test]
    fn cat() -> Result<()> {
        let mut handle = Spawner::new("cat")?
            .input(StreamMode::Pipe)
            .output(StreamMode::Pipe)
            .spawn()?;

        let string = "Hello, World!";
        write!(handle, "{string}")?;
        handle.close()?;

        let output = handle.output()?.read_blocking()?;
        assert_eq!(output.trim(), string);
        Ok(())
    }

    #[test]
    fn read() -> Result<()> {
        let string = "Hello!";
        let mut handle = Spawner::new("echo")?
            .arg(string)
            .output(StreamMode::Pipe)
            .spawn()?;

        let bytes = handle.output()?.read_bytes(Some(string.len()));
        let output = String::from_utf8_lossy(&bytes);
        assert_eq!(output.trim(), string);
        Ok(())
    }

    #[test]
    fn clear_env() -> Result<()> {
        let mut handle = Spawner::new("bash")?
            .args(["-c", "echo $USER"])
            .output(StreamMode::Pipe)
            .error(StreamMode::Pipe)
            .spawn()?;

        let output = handle.output()?.read_blocking()?;
        assert!(output.trim().is_empty());
        Ok(())
    }

    #[test]
    fn preserve_env() -> Result<()> {
        let user = "Test";
        let mut handle = Spawner::new("bash")?
            .args(["-c", "echo $USER"])
            .env("USER", user)
            .output(StreamMode::Pipe)
            .error(StreamMode::Pipe)
            .spawn()?;

        let output = handle.output()?.read_blocking()?;
        assert_eq!(output.trim(), user);
        Ok(())
    }

    #[test]
    fn change_dir() -> Result<()> {
        let old = env::current_dir()?;
        Spawner::new("bash")?
            .args(["-c", "echo Hello > test.txt"])
            .dir("/tmp")
            .spawn()?
            .wait()?;

        let path = Path::new("/tmp/test.txt");
        assert!(path.exists());
        fs::remove_file(path)?;
        assert_eq!(old, env::current_dir()?);
        Ok(())
    }
}
