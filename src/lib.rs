#![doc = include_str!("../README.md")]

mod handle;
mod spawn;
mod which;

use caps::{CapSet, CapsHashSet};
use log::warn;
use nix::unistd::pipe;
use std::{fs::File, os::fd::OwnedFd, sync::LazyLock};

pub use handle::{Error as HandleError, Handle, Stream};
pub use spawn::{Error as SpawnError, Method, Spawner, StreamMode};
pub use which::{Error as WhichError, SpawnWhich, Which};

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

/// Conditionally create a pipe.
/// Returns either a set of `None`, or the result of `pipe()`
fn cond_pipe(cond: &StreamMode) -> Result<Option<(OwnedFd, OwnedFd)>, SpawnError> {
    match cond {
        StreamMode::Pipe | StreamMode::Log(_) => {
            let (r, w) = pipe()?;
            Ok(Some((r, w)))
        }
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
    use crate::{Spawner, StreamMode, spawn::Method};
    use anyhow::Result;
    use std::{
        env,
        fs::{self},
        io::Write,
        path::{Path, PathBuf},
    };

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

    #[test]
    fn posix_spawn() -> Result<()> {
        let handle = Spawner::abs("/usr/bin/true").spawn()?;
        assert!(handle.spawn_method() == Method::PosixSpawn);
        handle.wait()?;

        let handle = Spawner::abs("/usr/bin/true").new_privileges(true).spawn()?;
        assert!(handle.spawn_method() == Method::ForkExec);
        handle.wait()?;

        let handle = Spawner::abs("/usr/bin/true")
            .cap(caps::Capability::CAP_AUDIT_CONTROL)
            .spawn()?;
        assert!(handle.spawn_method() == Method::ForkExec);
        handle.wait()?;

        let handle = Spawner::abs("/usr/bin/true")
            .dir(PathBuf::from("/"))
            .spawn()?;
        assert!(handle.spawn_method() == Method::ForkExec);
        handle.wait()?;

        #[cfg(feature = "fd")]
        {
            use std::fs::File;
            use std::os::fd::OwnedFd;

            let handle = Spawner::abs("/usr/bin/true")
                .fd(OwnedFd::from(File::create("/tmp/file")?))
                .spawn()?;
            assert!(handle.spawn_method() == Method::ForkExec);
            handle.wait()?;
        }

        #[cfg(feature = "seccomp")]
        {
            use seccomp::{action::Action, filter::Filter};
            let filter = Filter::new(Action::Allow)?;
            let handle = Spawner::abs("/usr/bin/true").seccomp(filter).spawn()?;
            assert!(handle.spawn_method() == Method::ForkExec);
            handle.wait()?;
        }

        Ok(())
    }

    #[test]
    fn bench() -> Result<()> {
        use std::time::Instant;
        let start = Instant::now();
        for _i in 0..100 {
            Spawner::abs("/usr/bin/true").spawn()?.wait()?;
        }
        let end = start.elapsed();
        println!("POSIX: {}", end.as_millis());

        let start = Instant::now();
        for _i in 0..100 {
            Spawner::abs("/usr/bin/true")
                .new_privileges(true)
                .spawn()?
                .wait()?;
        }
        let end = start.elapsed();
        println!("FORK/EXEC: {}", end.as_millis());

        Ok(())
    }
}
