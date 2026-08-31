//! Spawn subprocesses with more fine-grained control over File Descriptors,
//! UID/GID, and File Stream handling.

use crate::{
    NULL, clear_capabilities, cond_pipe,
    handle::Handle,
    logger,
    which::{self, SpawnWhich, Which},
};
use caps::{Capability, CapsHashSet};
use dashmap::{DashMap, DashSet, mapref::one::RefMut};
use log::trace;
use nix::{
    errno,
    libc::{STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO},
    spawn::{PosixSpawnAttr, PosixSpawnFileActions, posix_spawn},
    sys::{prctl, signal::Signal::SIGTERM},
    unistd::{ForkResult, Pid, close, dup, dup2_stderr, dup2_stdin, dup2_stdout, execve, fork},
};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::{CString, NulError, OsString},
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
    thread,
};
use thiserror::Error;

#[cfg(feature = "user")]
use user::Mode;

#[cfg(feature = "seccomp")]
use seccomp::filter::{self, Filter};

#[cfg(feature = "fd")]
use nix::fcntl::{FcntlArg, FdFlag, fcntl};

#[cfg(feature = "cache")]
use std::fs;

/// Errors related to the Spawner.
#[derive(Debug, Error)]
pub enum Error {
    /// Errors when passed arguments contain Null values.
    #[error("Invalid string: {0}")]
    Null(#[from] NulError),

    /// Errors when the cache is improperly accessed.
    #[cfg(feature = "cache")]
    #[error("Cache error: {0}")]
    Cache(&'static str),

    /// Errors reading/writing to the cache.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Errors to various functions that return `Errno`.
    #[error("Errno: {0}")]
    Errno(#[from] errno::Errno),

    /// Errors resolving binary paths.
    #[error("Failed to resolve binary: {0}")]
    Path(#[from] which::Error),

    /// An error when trying to fork.
    #[error("Fork error: {0}")]
    Fork(errno::Errno),

    /// An error when exec fails
    #[error("Exec error: {0}")]
    Exec(errno::Errno),

    #[cfg(feature = "user")]
    /// An error for switching operating user
    #[error("User error: {0}")]
    User(#[from] user::Error),

    /// An error when the spawner fails to parse the environment.
    #[error("Failed to parse environment")]
    Environment,

    #[cfg(feature = "seccomp")]
    /// An error trying to apply the *SECCOMP* Filter.
    #[error("SECCOMP error: {0}")]
    Seccomp(#[from] filter::Error),
}

/// How to handle the standard input/out/error streams
#[derive(Default)]
pub enum StreamMode {
    /// Collect the stream contents in a Stream object via a
    /// pipe that can be retrieved in the `spawn::Handle`
    Pipe,

    /// Share STDIN/STDOUT/STDERR with the process, such that it can write
    /// to the parent. This is the default.
    #[default]
    Share,

    /// Send the output to the system logger at the provided level. If the log
    /// level is below this, output is discarded.
    Log(log::Level),

    /// Send output to /dev/null.
    Discard,

    #[cfg(feature = "fd")]
    /// Send the output to the provided File Descriptor.
    Fd(OwnedFd),
}

/// Values passed between the various spawn helpers.
struct SpawnPackage {
    /// The stdin pipe
    pub stdin: Option<(OwnedFd, OwnedFd)>,

    /// The stdout pipe
    pub stdout: Option<(OwnedFd, OwnedFd)>,

    /// The stderr pipe
    pub stderr: Option<(OwnedFd, OwnedFd)>,

    /// Our current stdin mode.
    pub stdin_mode: StreamMode,

    /// Our current stdout mode.
    pub stdout_mode: StreamMode,

    /// Our current stderr mode.
    pub stderr_mode: StreamMode,

    /// Arguments to the process.
    pub args_c: Vec<CString>,

    /// The environment of the process.
    pub envs: Vec<CString>,

    /// The unique name of the handle, if any
    pub unique_name: Option<String>,

    /// The command to run
    pub cmd: String,

    #[cfg(feature = "user")]
    /// What operating mode to run under, if desired
    pub mode: Option<Mode>,

    /// Associated handles, if any.
    pub associated: DashMap<String, Handle>,
}

/// How the child was spawned. Mostly for diagonstics.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Via `posix_spawn`
    PosixSpawn,

    /// Via `fork`/`exec`
    ForkExec,
}

/// Spawn a child.
///
/// ## Thread Safety
///
/// This entire object is safe to pass and construct across multiple threads.
///
/// ## Examples
/// Launch bash in a child, inheriting the parent's input/output/error:
/// ```rust
/// spawn::Spawner::new("bash").unwrap().spawn().unwrap();
/// ```
///
/// Launch cat, feeding it input from the parent:
/// ```rust
/// use std::io::Write;
/// let mut handle = spawn::Spawner::new("cat").unwrap()
///     .input(spawn::StreamMode::Pipe)
///     .output(spawn::StreamMode::Pipe)
///     .spawn()
///     .unwrap();
/// let string = "Hello, World!";
/// write!(handle, "{}", &string);
/// handle.close();
/// let output = handle.output().unwrap().read_blocking().unwrap();
/// assert!(output == string);
/// ```
pub struct Spawner {
    /// The binary to run
    cmd: String,

    /// A unique name for the process, to be used to reference it by the Handle.
    unique_name: Mutex<Option<String>>,

    /// A path to execute the command from
    directory: Mutex<Option<PathBuf>>,

    /// Arguments
    args: Mutex<Vec<String>>,

    /// Whether to pipe **STDIN**. This lets you call `Handle::write()` to
    /// the process handle to send any Display value to the child.
    input: Mutex<StreamMode>,

    /// Capture the child's **STDOUT**.
    output: Mutex<StreamMode>,

    /// Capture the child's **STDERR**.
    error: Mutex<StreamMode>,

    /// Clear the environment before spawning the child.
    preserve_env: AtomicBool,

    /// Don't clear privileges.
    no_new_privileges: AtomicBool,

    /// Whitelisted capabilities.
    whitelist: DashSet<Capability>,

    /// Environment variables
    env: DashMap<String, String>,

    /// A list of other Pids that the eventual Handle should be responsible for,
    /// attached to the main child.
    associated: DashMap<String, Handle>,

    /// An index to cache parts of the command line
    #[cfg(feature = "cache")]
    cache_index: Mutex<Option<usize>>,

    /// FD's to pass to the program. These do not include 0,1,2 who's
    /// logic is controlled via input/capture respectively.
    #[cfg(feature = "fd")]
    fds: Mutex<Vec<OwnedFd>>,

    /// The User to run the program under.
    #[cfg(feature = "user")]
    mode: Mutex<Option<user::Mode>>,

    /// An optional *SECCOMP* policy to load on the child.
    #[cfg(feature = "seccomp")]
    seccomp: Mutex<Option<Filter>>,
}
impl Spawner {
    /// Construct a `Spawner` to spawn *cmd*.
    /// *cmd* will be resolved from **PATH**.
    ///
    /// ## Errors
    /// If the path could not be found.
    pub fn new(cmd: impl Into<String>) -> Result<Self, Error> {
        Self::which::<SpawnWhich>(cmd)
    }

    /// Construct a `Spawner` using a specialized `which` resolver.
    ///
    /// ## Errors
    ///
    /// If the path could not be found.
    pub fn which<T: Which>(cmd: impl Into<String>) -> Result<Self, Error> {
        let cmd = cmd.into();
        let path = T::which(&cmd)?;
        Ok(Self::abs(path))
    }

    /// Construct a `Spanwner` to spawn *cmd*.
    /// This function treats *cmd* as an absolute
    /// path. No resolution is performed.
    pub fn abs(cmd: impl Into<String>) -> Self {
        Self {
            cmd: cmd.into(),
            unique_name: Mutex::new(None),
            directory: Mutex::new(None),
            args: Mutex::default(),
            input: Mutex::new(StreamMode::Share),
            output: Mutex::new(StreamMode::Share),
            error: Mutex::new(StreamMode::Share),

            preserve_env: AtomicBool::new(false),
            no_new_privileges: AtomicBool::new(true),
            whitelist: DashSet::new(),
            env: DashMap::new(),
            associated: DashMap::new(),

            #[cfg(feature = "cache")]
            cache_index: Mutex::new(None),

            #[cfg(feature = "fd")]
            fds: Mutex::default(),

            #[cfg(feature = "user")]
            mode: Mutex::default(),

            #[cfg(feature = "seccomp")]
            seccomp: Mutex::new(None),
        }
    }

    /// Control whether to hook the child's standard input.
    #[must_use]
    pub fn input(self, input: StreamMode) -> Self {
        self.input_i(input);
        self
    }

    /// Control whether to hook the child's standard output.
    #[must_use]
    pub fn output(self, output: StreamMode) -> Self {
        self.output_i(output);
        self
    }

    /// Control whether to hook the child's standard error.
    #[must_use]
    pub fn error(self, error: StreamMode) -> Self {
        self.error_i(error);
        self
    }

    /// Give a unique name to the process, so you can refer to the Handle.
    /// If no name is set, the string passed to `Spawn::new()` will be used
    #[must_use]
    pub fn name(self, name: &str) -> Self {
        *self.unique_name.lock() = Some(name.to_owned());
        self
    }

    /// Attach another process that is attached to the main child, and should be killed
    /// when the eventual Handle goes out of scope.
    pub fn associate(&self, process: Handle) {
        let _ = self.associated.insert(process.name().to_owned(), process);
    }

    /// Returns a mutable reference to an associate within the Handle, if it exists.
    /// The associate is another Handle instance.
    pub fn get_associate<'b>(&'b self, name: &str) -> Option<RefMut<'b, String, Handle>> {
        self.associated.get_mut(name)
    }

    /// Drop privilege to the provided user mode on the child,
    /// immediately after the fork. This does not affected the parent
    /// process, but prevents the child from changing outside
    /// of the assigned UID.
    ///
    /// If is set to *Original*, the child is launched with the exact
    /// same operating set as the parent, persisting setuid privilege.
    ///
    /// If mode is not set, or set to *Existing*, it adopts whatever operating
    /// set the parent is in when `spawn()` is called. This is ill-advised.
    ///
    /// If the parent is not setuid, this parameter is a no-op
    #[cfg(feature = "user")]
    #[must_use]
    pub fn mode(self, mode: user::Mode) -> Self {
        self.mode_i(mode);
        self
    }

    /// Preserve the environment of the parent when launching the child.
    /// `Spawner` defaults to clearing the environment.
    #[must_use]
    pub fn preserve_env(self, preserve: bool) -> Self {
        self.preserve_env_i(preserve);
        self
    }

    /// Add a capability to the child's capability set.
    /// Note that this function cannot grant capability the program
    /// does not possess, it merely prevents existing capabilities from
    /// being cleared.
    #[must_use]
    pub fn cap(self, cap: Capability) -> Self {
        let _ = self.whitelist.insert(cap);
        self
    }

    /// Add capabilities to the child's capability set.
    /// Note that this function cannot grant capability the program
    /// does not possess, it merely prevents existing capabilities from
    /// being cleared.
    #[must_use]
    pub fn caps(self, caps: impl IntoIterator<Item = Capability>) -> Self {
        caps.into_iter().for_each(|cap| {
            let _ = self.whitelist.insert(cap);
        });
        self
    }

    /// Control whether the child is allowed new privileges.
    /// Note that this function cannot grant privilege the program
    /// does not already have, but merely allows it access to existing privileges
    /// not shared by the parent.
    #[must_use]
    pub fn new_privileges(self, allow: bool) -> Self {
        self.new_privileges_i(allow);
        self
    }

    /// Sets an environment variable to pass to the process.
    /// Note that if `preserve_env` is set to true, this value will
    /// overwrite the existing value, if it exists.
    ///
    /// ## Errors
    /// `Error::Environment` if the key or value are not valid `CString`s
    #[must_use]
    pub fn env(self, key: impl Into<String>, var: impl Into<String>) -> Self {
        self.env_i(key, var);
        self
    }

    /// Passes the value of the provided environment variable to the child.
    /// If `preserve_env` is true, this is functionally a no-op.
    ///
    /// ## Errors
    /// `Error::Environment` if the key or value are not valid `CString`s
    pub fn pass_env(self, key: impl Into<String>) -> Result<Self, Error> {
        self.pass_env_i(key)?;
        Ok(self)
    }

    /// Passes the value of the provided environment variable to the child.
    /// If `preserve_env` is true, this is functionally a no-op.
    ///
    /// ## Errors
    /// `Error::Environment` if the key or value are not valid `CString`s
    pub fn env_or(
        self,
        key: impl Into<String>,
        fallback: impl Into<String>,
    ) -> Result<Self, Error> {
        self.env_or_i(key, fallback)?;
        Ok(self)
    }

    #[cfg(feature = "seccomp")]
    /// Move a *SECCOMP* filter to the `Spawner`, loading in the child after forking.
    /// *SECCOMP* is the last operation applied. This has several consequences:
    ///
    /// 1.  The child will be running under the assigned operating set mode,
    ///     and said operating set must have permission to load the filter.
    ///  2.  If using Notify, the path to the monitor socket must
    ///      be accessible by the operating set mode.
    ///  3.  Your *SECCOMP* filter must permit `execve` to launch the application.
    ///      This does not have to be ALLOW. See the caveats to Notify if
    ///      you are using it.
    #[must_use]
    pub fn seccomp(self, seccomp: Filter) -> Self {
        self.seccomp_i(seccomp);
        self
    }

    /// Move a new argument to the argument vector.
    /// This function is guaranteed to append to the end of the current argument
    /// vector.
    #[must_use]
    pub fn arg(self, arg: impl Into<String>) -> Self {
        self.arg_i(arg);
        self
    }

    /// Move a new FD to the `Spawner`.
    /// FD's will be shared to the child under the same value.
    /// Any FD's in the parent not explicitly passed will be dropped.
    #[cfg(feature = "fd")]
    #[must_use]
    pub fn fd(self, fd: impl Into<OwnedFd>) -> Self {
        self.fd_i(fd);
        self
    }

    /// Move a FD to the `Spawner`, and attach it to an argument to ensure the
    /// value is identical.
    ///
    /// ## Example
    /// Bubblewrap supports the --file flag, which accepts a FD and destination.
    /// If you want to ensure you don't accidentally mismatch FDs, you can
    /// commit both the FD and argument in the same transaction:
    ///
    /// ```rust
    /// let file = std::fs::File::create("file.txt").unwrap();
    /// spawn::Spawner::new("bwrap").unwrap()
    ///     .fd_arg("--file", file)
    ///     .arg("/file.txt")
    ///     .spawn().unwrap();
    /// std::fs::remove_file("file.txt").unwrap();
    /// ```
    #[cfg(feature = "fd")]
    #[must_use]
    pub fn fd_arg(self, arg: impl Into<String>, fd: impl Into<OwnedFd>) -> Self {
        self.fd_arg_i(arg, fd);
        self
    }

    /// Move an iterator of arguments into the `Spawner`.
    /// It is guaranteed that the arguments
    /// in the iterator will appear sequentially, and in the same order.
    #[must_use]
    pub fn args<I, S>(self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args_i(args);
        self
    }

    /// Move an iterator of FD's to the `Spawner`.
    #[cfg(feature = "fd")]
    #[must_use]
    pub fn fds<I, S>(self, fds: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OwnedFd>,
    {
        self.fds_i(fds);
        self
    }

    /// The directory to pivot into before executing the child.
    /// This does not change the current directory of the parent.
    /// It is run after changing users. If you are using the users
    /// feature, this directory must be accessible by the desired
    /// mode.
    #[must_use]
    pub fn dir(self, directory: impl Into<PathBuf>) -> Self {
        self.dir_i(directory);
        self
    }

    /// Set the input flag without consuming the `Spawner`.
    pub fn input_i(&self, input: StreamMode) {
        *self.input.lock() = input;
    }

    /// Set the output flag without consuming the `Spawner`.
    pub fn output_i(&self, output: StreamMode) {
        *self.output.lock() = output;
    }

    /// Set the error flag without consuming the `Spawner`.
    pub fn error_i(&self, error: StreamMode) {
        *self.error.lock() = error;
    }

    /// Set the preserve environment flag without consuming the `Spawner`.
    pub fn preserve_env_i(&self, preserve: bool) {
        self.preserve_env.store(preserve, Ordering::Relaxed);
    }

    /// Add a capability without consuming the `Spawner`.
    pub fn cap_i(&mut self, cap: Capability) {
        let _ = self.whitelist.insert(cap);
    }

    /// Adds a capability set without consuming the `Spawner`.
    pub fn caps_i(&mut self, caps: impl IntoIterator<Item = Capability>) {
        caps.into_iter().for_each(|cap| {
            let _ = self.whitelist.insert(cap);
        });
    }

    /// Set the `NO_NEW_PRIVS` flag without consuming the `Spawner`.
    pub fn new_privileges_i(&self, allow: bool) {
        self.no_new_privileges.store(!allow, Ordering::Relaxed);
    }

    /// Sets an environment variable to the child process without consuming the `Spawner`.
    pub fn env_i(&self, key: impl Into<String>, value: impl Into<String>) {
        let _ = self.env.insert(key.into(), value.into());
    }

    /// Pass an environment variable to the child process without consuming the `Spawner`.
    ///
    /// ## Errors
    /// `Error::Environment` if the key or value are not valid `CString`s
    pub fn pass_env_i(&self, key: impl Into<String>) -> Result<(), Error> {
        let key = key.into();
        let os_key = OsString::from_str(&key).map_err(|_| Error::Environment)?;
        env::var(&os_key).map_or_else(
            |_| Err(Error::Environment),
            |env| {
                let _ = self.env.insert(key, env);
                Ok(())
            },
        )
    }

    /// Pass an environment variable to the child process without consuming the `Spawner`.
    ///
    /// ## Errors
    /// `Error::Environment` if the key or value are not valid `CString`s
    pub fn env_or_i(
        &self,
        key: impl Into<String>,
        fallback: impl Into<String>,
    ) -> Result<(), Error> {
        let key = key.into();
        let os_key = OsString::from_str(&key).map_err(|_| Error::Environment)?;
        if let Ok(env) = env::var(&os_key) {
            let _ = self.env.insert(key, env);
        } else {
            let _ = self.env.insert(key, fallback.into());
        }
        Ok(())
    }

    /// Set the user mode without consuming the `Spawner`.
    #[cfg(feature = "user")]
    pub fn mode_i(&self, mode: user::Mode) {
        *self.mode.lock() = Some(mode);
    }

    /// Set a *SECCOMP* filter without consuming the `Spawner`.
    #[cfg(feature = "seccomp")]
    pub fn seccomp_i(&self, seccomp: Filter) {
        *self.seccomp.lock() = Some(seccomp);
    }

    /// Move an argument to the `Spawner` in-place.
    pub fn arg_i(&self, arg: impl Into<String>) {
        self.args.lock().push(arg.into());
    }

    /// Move a FD to the `Spawner` in-place.
    #[cfg(feature = "fd")]
    pub fn fd_i(&self, fd: impl Into<OwnedFd>) {
        self.fds.lock().push(fd.into());
    }

    /// Move FDs to the `Spawner` in-place, passing it as an argument.
    #[cfg(feature = "fd")]
    pub fn fd_arg_i(&self, arg: impl Into<String>, fd: impl Into<OwnedFd>) {
        let fd = fd.into();
        self.args_i([arg.into(), format!("{}", fd.as_raw_fd())]);
        self.fd_i(fd);
    }

    /// Move an iterator of FDs to the `Spawner` in-place.
    #[cfg(feature = "fd")]
    pub fn fds_i<I, S>(&self, fds: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<OwnedFd>,
    {
        self.fds.lock().extend(fds.into_iter().map(Into::into));
    }

    /// Get all currently stored FDs as `RawFd`s
    #[cfg(feature = "fd")]
    pub fn get_fds(&self) -> Vec<RawFd> {
        self.fds.lock().iter().map(AsRawFd::as_raw_fd).collect()
    }

    /// Move an iterator of arguments to the `Spawner` in-place.
    /// Both sequence and order are guaranteed.
    pub fn args_i<I, S>(&self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut a = self.args.lock();

        for s in args {
            a.push(s.into());
        }
    }

    /// The directory to pivot into before executing the child.
    /// This does not change the current directory of the parent.
    /// It is run after changing users. If you are using the users
    /// feature, this directory must be accessible by the desired
    /// mode.
    pub fn dir_i(&self, directory: impl Into<PathBuf>) {
        *self.directory.lock() = Some(directory.into());
    }

    /// Set the cache index.
    /// Once the cache flag has been set, all subsequent arguments will
    /// be cached to the file provided to `cache_write`.
    /// On future runs, `cache_read` can be used to append those cached
    /// contents to the `Spawner`'s arguments.
    /// This function fails if `cache_start` is called twice without having
    /// first called `cache_write`.
    ///
    /// ## Examples
    ///
    /// ```rust,ignore
    /// let cache = std::path::PathBuf::from("cmd.cache");
    /// let mut handle = spawn::Spawner::abs("/usr/bin/bash");
    /// if cache.exists() {
    ///     handle.cache_read(&cache).unwrap();
    /// } else {
    ///     handle.cache_start().unwrap();
    ///     handle.arg_i("arg").unwrap();
    ///     handle.cache_write(&cache).unwrap();
    /// }
    /// std::fs::remove_file(cache);
    /// ```
    ///
    /// ## Caveat
    ///
    /// Because the cache is written to disk, ephemeral values, such
    /// as FD values, temporary files, etc, must not be passed to the
    /// Spawner, otherwise those values would be cached, and likely
    /// be invalid when trying to use the cached results.
    ///
    /// ## Errors
    /// `Error::Cache`: If the cache was already started.
    #[cfg(feature = "cache")]
    #[allow(clippy::significant_drop_tightening)]
    pub fn cache_start(&self) -> Result<(), Error> {
        let mut index = self.cache_index.lock();
        if index.is_some() {
            Err(Error::Cache("Caching already started!"))
        } else {
            *index = Some(self.args.lock().len());
            Ok(())
        }
    }

    /// Write all arguments added to the `Spawner` since `cache_start`
    /// was called to the file provided.
    /// This function will fail if `cache_start` was not called,
    /// or if there are errors writing to the provided path.
    ///
    /// ## Errors
    /// `Error::Cache`: If the cache was not started.
    #[cfg(feature = "cache")]
    #[allow(clippy::significant_drop_tightening)]
    pub fn cache_write(&self, path: &Path) -> Result<(), Error> {
        use std::io::Write;
        let mut index = self.cache_index.lock();
        if let Some(i) = *index {
            let args = self.args.lock();
            if let Some(parent) = path.parent()
                && !parent.exists()
            {
                fs::create_dir(parent)?;
            }
            let mut file = fs::File::create(path)?;
            let bytes = args[i..].join(" ");
            file.write_all(bytes.as_bytes())?;
            *index = None;
            Ok(())
        } else {
            Err(Error::Cache("Cache not started!"))
        }
    }

    /// Read from the cache file, adding its contents to the `Spawner`'s
    /// arguments.
    /// This function will fail if there is an error reading the file,
    /// or if the contents contain strings will NULL bytes.
    ///
    /// ## Errors
    /// If the cache could not be read
    #[cfg(feature = "cache")]
    pub fn cache_read(&self, path: &Path) -> Result<(), Error> {
        let mut cached: Vec<String> = fs::read_to_string(path)?
            .split(' ')
            .map(String::from)
            .collect();
        self.args.lock().append(&mut cached);
        Ok(())
    }

    /// Manage the parent in a `spawn` call.
    ///
    /// This function runs after a child `Pid` has been acquired. In the fork/exec,
    /// this is run within the parent arm (Hence the same). In `posix_spawn`, this
    /// is run immediately after the `posix_spawn` call.
    fn parent_arm(child: Pid, pkg: SpawnPackage, method: Method) -> Result<Handle, Error> {
        let name = if let Some(name) = pkg.unique_name {
            name
        } else {
            pkg.cmd
        };

        // Set the relevant pipes.
        let stdin = if let Some((read, write)) = pkg.stdin {
            close(read)?;
            Some(write)
        } else {
            None
        };

        let stdout = if let Some((read, write)) = pkg.stdout {
            close(write)?;
            if let StreamMode::Log(log) = pkg.stdout_mode {
                let name = name.clone();
                let _ = thread::spawn(move || logger(log, read, &name));
                None
            } else {
                Some(read)
            }
        } else {
            None
        };

        let stderr = if let Some((read, write)) = pkg.stderr {
            close(write)?;
            if let StreamMode::Log(log) = pkg.stderr_mode {
                let name = name.clone();
                let _ = thread::spawn(move || logger(log, read, &name));
                None
            } else {
                Some(read)
            }
        } else {
            None
        };

        #[cfg(feature = "user")]
        let mode = pkg.mode.unwrap_or(user::Mode::Existing);

        let associated: Vec<Handle> = pkg.associated.into_iter().map(|(_, v)| v).collect();

        // Return.
        let handle = Handle::new(
            name,
            child,
            #[cfg(feature = "user")]
            mode,
            stdin,
            stdout,
            stderr,
            associated,
            method,
        );
        Ok(handle)
    }

    /// Determine if we can use `posix_spawn`
    ///
    /// To use `posix_spawn`, we cannot use:
    ///
    /// 1. Any capabilities (If no caps are specified, capabilities are not dropped)
    /// 2. No SECCOMP Filter
    /// 3. No arbitrary FD passthrough
    /// 4. No directory for child to start in
    /// 5. No `no_new_privileges` set
    /// 6. No user mode.
    fn can_posix(&self) -> bool {
        #[cfg(feature = "user")]
        let user = self.mode.lock().is_none();

        #[cfg(not(feature = "user"))]
        let user = true;

        #[cfg(feature = "seccomp")]
        let seccomp = self.seccomp.lock().is_none();

        #[cfg(not(feature = "seccomp"))]
        let seccomp = true;

        #[cfg(feature = "fd")]
        let fds = self.fds.lock().is_empty();

        #[cfg(not(feature = "fd"))]
        let fds = true;

        self.whitelist.is_empty()
            && seccomp
            && fds
            && self.directory.lock().is_none()
            && self.no_new_privileges.load(Ordering::Relaxed)
            && user
    }

    /// Spawn the child process.
    /// This consumes the structure, returning a `spawn::Handle`.
    ///
    /// This function will either use a traditional fork/exec, or `posix_spawn`, depending on how the handle
    /// is configured, and what is faster.
    ///
    /// ## Errors
    /// This function can fail for many reasons--pretty much every error type defined in
    /// this crate's Error enum, for multiple reasons. However, every error is thrown as the
    /// result of a failing system call, or invalid argument.
    ///
    /// More specifically, you can cause an error by (non-exhaustively):
    ///
    /// * Passing a string that cannot be converted into a `CString` to `env`/`arg`
    /// * Stealing the `RawFd` of a supposed `OwnedFd` passed to the `Spawner` and closing it.
    /// * Passing a malicious `seccomp::Filter` implementation.
    /// * Denying necessary syscalls through something like `AppArmor` or SECCOMP.
    ///
    /// In other words, unless you are actively trying to cause an error, this function will not
    /// throw one.
    #[allow(clippy::too_many_lines, clippy::arithmetic_side_effects)]
    pub fn spawn(mut self) -> Result<Handle, Error> {
        let can_posix = self.can_posix();

        let stdout_mode = self.output.into_inner();
        let stderr_mode = self.error.into_inner();
        let stdin_mode = self.input.into_inner();

        let stdout = cond_pipe(&stdout_mode)?;
        let stderr = cond_pipe(&stderr_mode)?;
        let stdin = cond_pipe(&stdin_mode)?;

        let args = self.args.into_inner();
        let associated = self.associated;

        #[cfg(feature = "user")]
        let mode = self.mode.into_inner();

        let unique_name = self.unique_name.into_inner();
        let cmd = self.cmd;

        let mut args_c = Vec::with_capacity(args.len() + 1);
        args_c.push(CString::new(cmd.as_str())?);

        for arg in args {
            args_c.push(CString::from_str(&arg)?);
        }

        if self.preserve_env.load(Ordering::Relaxed) {
            let environment: HashMap<_, _> = env::vars_os()
                .filter_map(|(key, value)| {
                    let key = key.into_string().ok()?;
                    let value = value.into_string().ok()?;

                    if self.env.contains_key(&key) {
                        None
                    } else {
                        Some((key, value))
                    }
                })
                .collect();

            self.env.extend(environment);
        }

        let envs: Vec<_> = self
            .env
            .into_iter()
            .filter_map(|(k, v)| CString::from_str(&format!("{k}={v}")).ok())
            .collect();

        // Log if desired.
        if log::log_enabled!(log::Level::Trace) {
            let method = if can_posix {
                "posix_spawn"
            } else {
                "fork/exec"
            };

            let formatted = args_c
                .iter()
                .map(|e| e.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");

            if self.preserve_env.load(Ordering::Relaxed) {
                trace!("[SYSTEM ENVIRONMENT] ({method}) {formatted}");
            } else if !envs.is_empty() {
                let env_formatted = envs
                    .iter()
                    .map(|e| e.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ");

                trace!("({method}) {env_formatted} {formatted}");
            } else {
                trace!("({method}) {formatted}");
            }
        }

        let pkg = SpawnPackage {
            stdin,
            stdout,
            stderr,
            stdin_mode,
            stdout_mode,
            stderr_mode,
            args_c,
            envs,
            unique_name,
            cmd,

            #[cfg(feature = "user")]
            mode,

            associated,
        };

        if can_posix {
            Self::posix_spawn(pkg)
        } else {
            Self::fork_exec(
                pkg,
                &self.whitelist.into_iter().collect(),
                #[cfg(feature = "seccomp")]
                {
                    let mut filter = self.seccomp.into_inner();
                    if let Some(filter) = &mut filter {
                        filter.setup()?;
                    }
                    filter
                },
                // Clear F_SETFD to allow passed FD's to persist after execve
                #[cfg(feature = "fd")]
                &self.fds.into_inner(),
                self.directory.into_inner(),
                &self.no_new_privileges,
            )
        }
    }

    /// Spawn the process with a traditional fork/exec
    ///
    /// This has the benefit of having more control--particularly SECCOMP, arbitrary FD preservation,
    /// and capabilities, but *might* be slower than the `posix_spawn` mode.
    ///
    /// Which method is used depends on what features were enabled on the `Spawner`.
    #[allow(clippy::unwrap_used, clippy::unreachable)]
    fn fork_exec(
        pkg: SpawnPackage,
        set: &HashSet<Capability>,
        #[cfg(feature = "seccomp")] filter: Option<Filter>,
        #[cfg(feature = "fd")] fds: &Vec<OwnedFd>,
        directory: Option<PathBuf>,
        no_new_privileges: &AtomicBool,
    ) -> Result<Handle, Error> {
        let cmd_c = &pkg.args_c[0];

        let all = caps::all();
        let diff: CapsHashSet = all.difference(set).copied().collect();

        let fork = unsafe { fork() }.map_err(Error::Fork)?;
        match fork {
            ForkResult::Parent { child } => Self::parent_arm(child, pkg, Method::ForkExec),
            ForkResult::Child => {
                if let Some((read, write)) = pkg.stdin {
                    let _ = close(write);
                    let _ = dup2_stdin(read);
                } else if matches!(pkg.stdin_mode, StreamMode::Discard) {
                    let _ = dup2_stdin(dup(NULL.as_fd()).unwrap());
                }
                #[cfg(feature = "fd")]
                if let StreamMode::Fd(fd) = pkg.stdin_mode {
                    let _ = dup2_stdin(fd);
                }

                if let Some((read, write)) = pkg.stdout {
                    let _ = close(read);
                    let _ = dup2_stdout(write);
                } else if matches!(pkg.stdout_mode, StreamMode::Discard) {
                    let _ = dup2_stdout(dup(NULL.as_fd()).unwrap());
                }
                #[cfg(feature = "fd")]
                if let StreamMode::Fd(fd) = pkg.stdout_mode {
                    let _ = dup2_stdout(fd);
                }

                if let Some((read, write)) = pkg.stderr {
                    let _ = close(read);
                    let _ = dup2_stderr(write);
                } else if matches!(pkg.stderr_mode, StreamMode::Discard) {
                    let _ = dup2_stderr(dup(NULL.as_fd()).unwrap());
                }
                #[cfg(feature = "fd")]
                if let StreamMode::Fd(fd) = pkg.stderr_mode {
                    let _ = dup2_stderr(fd);
                }

                #[cfg(feature = "fd")]
                for fd in fds {
                    let _ = fcntl(fd, FcntlArg::F_SETFD(FdFlag::empty()))?;
                }

                let _ = prctl::set_pdeathsig(SIGTERM);

                // Drop modes
                #[cfg(feature = "user")]
                if let Some(mode) = pkg.mode {
                    assert!(user::drop(mode).is_ok());
                }

                if let Some(dir) = directory {
                    let _ = env::set_current_dir(dir);
                }

                clear_capabilities(&diff);

                if no_new_privileges.load(Ordering::Relaxed) {
                    assert!(prctl::set_no_new_privs().is_ok());
                }

                // Apply SECCOMP.
                // Because we can't just trust the application is able/willing to
                // apply a SECCOMP filter on it's own, we have to do it before the execve
                // call. That means the SECCOMP filter needs to either Allow, Log, Notify,
                // or some other mechanism to let the process to spawn.
                #[cfg(feature = "seccomp")]
                if let Some(filter) = filter {
                    filter.load();
                }

                // Execve. Note that the unwrap will never fail; either this child
                // is replaced with the exec call, or it fails.
                let _ = execve(cmd_c, &pkg.args_c, &pkg.envs);
                unreachable!()
            }
        }
    }

    /// Spawns the process using `posix_spawn`.
    ///
    /// This mode is more limited in functionality, but avoids having to `fork` in userspace.
    #[allow(clippy::too_many_lines, clippy::unwrap_used, clippy::unreachable)]
    fn posix_spawn(pkg: SpawnPackage) -> Result<Handle, Error> {
        let needs_null = matches!(pkg.stdin_mode, StreamMode::Discard)
            || matches!(pkg.stdout_mode, StreamMode::Discard)
            || matches!(pkg.stderr_mode, StreamMode::Discard);

        let null = if needs_null {
            Some(dup(NULL.as_fd())?)
        } else {
            None
        };
        let mut file_actions = PosixSpawnFileActions::init()?;

        let stdin_fd = RawFd::from(STDIN_FILENO);
        let stdout_fd = RawFd::from(STDOUT_FILENO);
        let stderr_fd = RawFd::from(STDERR_FILENO);

        if let Some((read, write)) = pkg.stdin.as_ref() {
            file_actions.add_close(write.as_raw_fd())?;
            file_actions.add_dup2(read.as_raw_fd(), stdin_fd)?;
        } else if matches!(pkg.stdin_mode, StreamMode::Discard) {
            file_actions.add_dup2(null.as_ref().unwrap().as_raw_fd(), stdin_fd)?;
        }
        #[cfg(feature = "fd")]
        if let StreamMode::Fd(fd) = &pkg.stdin_mode {
            file_actions.add_dup2(fd.as_raw_fd(), stdin_fd)?;
        }

        if let Some((read, write)) = pkg.stdout.as_ref() {
            file_actions.add_close(read.as_raw_fd())?;
            file_actions.add_dup2(write.as_raw_fd(), stdout_fd)?;
        } else if matches!(pkg.stdout_mode, StreamMode::Discard) {
            file_actions.add_dup2(null.as_ref().unwrap().as_raw_fd(), stdout_fd)?;
        }
        #[cfg(feature = "fd")]
        if let StreamMode::Fd(fd) = &pkg.stdout_mode {
            file_actions.add_dup2(fd.as_raw_fd(), stdout_fd)?;
        }

        if let Some((read, write)) = pkg.stderr.as_ref() {
            file_actions.add_close(read.as_raw_fd())?;
            file_actions.add_dup2(write.as_raw_fd(), stderr_fd)?;
        } else if matches!(pkg.stderr_mode, StreamMode::Discard) {
            file_actions.add_dup2(null.as_ref().unwrap().as_raw_fd(), stderr_fd)?;
        }
        #[cfg(feature = "fd")]
        if let StreamMode::Fd(fd) = &pkg.stderr_mode {
            file_actions.add_dup2(fd.as_raw_fd(), stderr_fd)?;
        }

        let attr = PosixSpawnAttr::init()?;
        let child = posix_spawn(
            Path::new(&pkg.cmd),
            &file_actions,
            &attr,
            &pkg.args_c,
            &pkg.envs,
        )?;
        Self::parent_arm(child, pkg, Method::PosixSpawn)
    }
}
