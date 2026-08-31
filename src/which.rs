//! This file exists to ameliorate the `Spawner` from its Antimony roots.
//!
//! You can provide a custom which resolver to `Spawner::which`, defaulting
//! to a basic implementation here used by `Spawner::new`. Which is not a
//! complicated command (Iterate PATH until you find a match), but it may
//! be useful if you need more security guarantees (e.g You don't trust
//! symlinks), or if you want performance (e.g Antimony)

use std::{borrow::Cow, env, path::Path};
use thiserror::Error;

/// Errors when trying to resolve a path.
#[derive(Debug, Error)]
pub enum Error {
    /// For if a cmd does not exist in PATH
    #[error("Could not find {0} in path")]
    NotFound(String),

    /// For if the PATH could not be resolved.
    #[error("Failed to resolve PATH!")]
    Path,
}

/// An implementation of which.
pub trait Which {
    /// Return the absolute path of a command within the PATH.
    ///
    /// ## Errors
    ///
    /// If the path could not be resolved, or does not exist.
    fn which(cmd: &str) -> Result<Cow<'_, str>, Error>;
}

/// The default Which. Does what you'd expect.
pub struct SpawnWhich;
impl Which for SpawnWhich {
    fn which(cmd: &str) -> Result<Cow<'_, str>, Error> {
        if Path::new(cmd).exists() {
            return Ok(Cow::Borrowed(cmd));
        }

        for path in env::var("PATH").map_err(|_| Error::Path)?.split(':') {
            let path = format!("{path}/{cmd}");
            if Path::new(&path).exists() {
                return Ok(Cow::Owned(path));
            }
        }
        Err(Error::NotFound(cmd.to_owned()))
    }
}
