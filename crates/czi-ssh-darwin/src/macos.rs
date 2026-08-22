use std::collections::BTreeMap;
use std::convert::Infallible;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};

const POSIX_SPAWN_SETSID: libc::c_short = 0x0400;
const POSIX_SPAWN_CLOEXEC_DEFAULT: libc::c_short = 0x4000;
// `sys/ttycom.h`: `_IO('t', 97)`, the Darwin request to become a controlling terminal.
const TIOCSCTTY: libc::c_ulong = 0x2000_7461;
const MIN_CHILD_FD: RawFd = 3;
const SSH_ASKPASS: &str = "SSH_ASKPASS";
const SSH_ASKPASS_REQUIRE: &str = "SSH_ASKPASS_REQUIRE";

/// A spawned child whose stdin and stdout are independent binary pipes and whose stderr and
/// controlling terminal are a local PTY.
pub struct SpawnedPty {
    /// Parent writer for child fd 0.
    pub stdin: File,
    /// Parent reader for child fd 1.
    pub stdout: File,
    /// Parent master side of the child's controlling terminal.
    pub pty_master: PtyMaster,
    /// Process lifecycle handle. Dropping it terminates and reaps the child.
    pub child: Child,
    /// Cloneable, non-reaping cancellation handle.
    pub cancellation: Cancellation,
}

/// Parent handle for reading the PTY's output and immediately writing terminal input.
pub struct PtyMaster {
    file: File,
}

/// Claim child fd 2 as the controlling terminal, then replace this executor with `executable`.
///
/// This function only returns when claiming the terminal or `execve` fails. Call it only from the
/// short, freshly spawned executor process: `spawn` has made that process a session leader and
/// opened the echo-disabled PTY slave at fd 2.
///
/// # Errors
///
/// Returns an error if the executor cannot claim its PTY or `execve` cannot start the absolute
/// target. A successful call does not return.
pub fn claim_controlling_terminal_and_exec(
    executable: &Path,
    argv: &[OsString],
) -> io::Result<Infallible> {
    if !executable.is_absolute() {
        return Err(invalid_input("embedded PTY executable must be absolute"));
    }
    if argv
        .first()
        .is_none_or(|argument| argument.as_os_str() != executable.as_os_str())
    {
        return Err(invalid_input(
            "embedded PTY argv[0] must equal its executable",
        ));
    }
    claim_controlling_terminal()?;
    let executable = c_string(executable.as_os_str(), "embedded PTY executable")?;
    let arguments = argv
        .iter()
        .map(|argument| c_string(argument, "embedded PTY argument"))
        .collect::<io::Result<Vec<_>>>()?;
    let environment = child_environment(&[])?;
    let mut argv_pointers = arguments
        .iter()
        .map(|argument| argument.as_ptr())
        .collect::<Vec<_>>();
    argv_pointers.push(ptr::null());
    let mut env_pointers = environment
        .iter()
        .map(|entry| entry.as_ptr())
        .collect::<Vec<_>>();
    env_pointers.push(ptr::null());
    // SAFETY: all C strings and pointer arrays are valid for this call. execve either replaces
    // this process or returns -1 without retaining pointers.
    unsafe {
        libc::execve(
            executable.as_ptr(),
            argv_pointers.as_ptr(),
            env_pointers.as_ptr(),
        );
    }
    Err(io::Error::last_os_error())
}

/// Claim child fd 2 as this fresh session leader's controlling terminal.
///
/// This is the small executor-only half of [`claim_controlling_terminal_and_exec`].
///
/// # Errors
///
/// Returns an error when fd 2 is not the spawned PTY slave or the process is not eligible to
/// claim it as a controlling terminal.
pub fn claim_controlling_terminal() -> io::Result<()> {
    // SAFETY: fd 2 was opened as this fresh session leader's PTY slave by spawn file actions.
    if unsafe { libc::ioctl(libc::STDERR_FILENO, TIOCSCTTY, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl PtyMaster {
    /// Read immediately available PTY output, appending at most `maximum` bytes to `output`.
    ///
    /// This never changes terminal flags and does not buffer or retain terminal input.
    ///
    /// # Errors
    ///
    /// Returns an error when polling or reading the PTY master fails.
    pub fn read_available(&mut self, output: &mut Vec<u8>, maximum: usize) -> io::Result<()> {
        while output.len() < maximum {
            let mut ready = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `ready` is a live one-element pollfd array and the timeout is zero.
            let result = unsafe { libc::poll(&mut ready, 1, 0) };
            if result == 0 {
                return Ok(());
            }
            if result == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready.revents & libc::POLLIN == 0 {
                return Ok(());
            }
            let mut buffer = [0_u8; 4096];
            let limit = (maximum - output.len()).min(buffer.len());
            let read = self.file.read(&mut buffer[..limit]);
            match read {
                Ok(0) => return Ok(()),
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Write terminal input immediately. The bytes are never retained by this API.
    ///
    /// # Errors
    ///
    /// Returns an error when writing or flushing the PTY master fails.
    pub fn write_input(&mut self, input: &[u8]) -> io::Result<()> {
        self.file.write_all(input).and_then(|()| self.file.flush())
    }
}

/// A child process started by [`spawn`].
pub struct Child {
    state: Arc<ProcessState>,
}

/// A cloneable cancellation handle that never reaps the process itself.
#[derive(Clone)]
pub struct Cancellation {
    state: Arc<ProcessState>,
}

struct ProcessState {
    pid: libc::pid_t,
    reaped: Mutex<bool>,
}

impl Child {
    /// Return the operating-system process identifier.
    #[must_use]
    pub fn id(&self) -> u32 {
        u32::try_from(self.state.pid).unwrap_or_default()
    }

    /// Non-blockingly collect a finished child status, if available.
    ///
    /// The returned status is the Darwin wait status suitable for
    /// `std::os::unix::process::ExitStatusExt::from_raw`.
    ///
    /// # Errors
    ///
    /// Returns an error when inspecting the child status fails.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        self.wait_once()
    }

    /// Block until the child is reaped.
    ///
    /// The returned status is the Darwin wait status suitable for
    /// `std::os::unix::process::ExitStatusExt::from_raw`.
    ///
    /// # Errors
    ///
    /// Returns an error when waiting for the child fails.
    pub fn wait(&mut self) -> io::Result<Option<i32>> {
        loop {
            if let Some(status) = self.wait_once()? {
                return Ok(Some(status));
            }
            if self.is_reaped()? {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Kill and then reap the child.
    ///
    /// # Errors
    ///
    /// Returns an error when signalling or waiting for the child fails.
    pub fn terminate_and_wait(&mut self) -> io::Result<Option<i32>> {
        let cancellation = Cancellation {
            state: Arc::clone(&self.state),
        };
        cancellation.cancel()?;
        self.wait()
    }

    fn is_reaped(&self) -> io::Result<bool> {
        self.state
            .reaped
            .lock()
            .map(|reaped| *reaped)
            .map_err(|_| io::Error::other("embedded child lifecycle lock poisoned"))
    }

    fn wait_once(&mut self) -> io::Result<Option<i32>> {
        let mut reaped = self
            .state
            .reaped
            .lock()
            .map_err(|_| io::Error::other("embedded child lifecycle lock poisoned"))?;
        if *reaped {
            return Ok(None);
        }
        let mut status = 0;
        loop {
            // SAFETY: `status` is valid writable storage and `pid` is returned by posix_spawn.
            let result = unsafe { libc::waitpid(self.state.pid, &mut status, libc::WNOHANG) };
            if result == 0 {
                return Ok(None);
            }
            if result == self.state.pid {
                *reaped = true;
                return Ok(Some(status));
            }
            if result == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            return Err(io::Error::other("waitpid returned an unexpected process"));
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.terminate_and_wait();
    }
}

impl Cancellation {
    /// Immediately terminate the child process group. The owner of [`Child`] must still reap it.
    ///
    /// # Errors
    ///
    /// Returns an error when signalling the child process group fails.
    pub fn cancel(&self) -> io::Result<()> {
        let reaped = self
            .state
            .reaped
            .lock()
            .map_err(|_| io::Error::other("embedded child lifecycle lock poisoned"))?;
        if *reaped {
            return Ok(());
        }
        // SAFETY: posix_spawn created a private session and process group whose ID is its PID.
        // Holding the lifecycle lock prevents a concurrent waitpid from reaping and recycling it.
        let result = unsafe { libc::kill(-self.state.pid, libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(error)
    }
}

/// Spawn an absolute-path program with binary stdin/stdout pipes and a controlling PTY.
///
/// `argv[0]` must equal `executable`. The child receives no `SSH_ASKPASS` or
/// `SSH_ASKPASS_REQUIRE`, even when they are present in the parent or `extra_environment`.
/// Child fd 0 is the read end of a pipe, fd 1 is the write end of a pipe, and fd 2 is the
/// echo-disabled PTY slave. Darwin applies file actions before `POSIX_SPAWN_SETSID`, so the short
/// executor must call [`claim_controlling_terminal`] before it `execve`s the final child.
///
/// # Errors
///
/// Returns an error when the executable or arguments contain NUL, the PTY/pipes cannot be made,
/// or Darwin rejects a spawn action.
pub fn spawn(
    executable: &Path,
    argv: &[OsString],
    extra_environment: &[(OsString, OsString)],
) -> io::Result<SpawnedPty> {
    if !executable.is_absolute() {
        return Err(invalid_input("embedded PTY executable must be absolute"));
    }
    if argv
        .first()
        .is_none_or(|argument| argument.as_os_str() != executable.as_os_str())
    {
        return Err(invalid_input(
            "embedded PTY argv[0] must equal its executable",
        ));
    }

    let executable = c_string(executable.as_os_str(), "embedded PTY executable")?;
    let arguments = argv
        .iter()
        .map(|argument| c_string(argument, "embedded PTY argument"))
        .collect::<io::Result<Vec<_>>>()?;
    let environment = child_environment(extra_environment)?;

    let (stdin_read, stdin_write) = pipe()?;
    let (stdout_read, stdout_write) = pipe()?;
    let (pty_master, _pty_slave, slave_name) = open_pty()?;

    let mut actions = FileActions::new()?;
    actions.add_dup2(stdin_read.as_raw_fd(), libc::STDIN_FILENO)?;
    actions.add_dup2(stdout_write.as_raw_fd(), libc::STDOUT_FILENO)?;
    actions.add_open(libc::STDERR_FILENO, &slave_name, libc::O_RDWR, 0)?;

    let mut attributes = SpawnAttributes::new()?;
    attributes.set_flags(POSIX_SPAWN_SETSID | POSIX_SPAWN_CLOEXEC_DEFAULT)?;

    let mut argv_pointers = arguments
        .iter()
        .map(|argument| argument.as_ptr().cast_mut())
        .collect::<Vec<_>>();
    argv_pointers.push(ptr::null_mut());
    let mut env_pointers = environment
        .iter()
        .map(|entry| entry.as_ptr().cast_mut())
        .collect::<Vec<_>>();
    env_pointers.push(ptr::null_mut());

    let mut pid = 0;
    // SAFETY: all C strings and pointer vectors remain alive for this synchronous call. The file
    // action and attribute handles were initialized by Darwin and are destroyed after this call.
    let result = unsafe {
        libc::posix_spawn(
            &mut pid,
            executable.as_ptr(),
            &actions.raw,
            &attributes.raw,
            argv_pointers.as_ptr(),
            env_pointers.as_ptr(),
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }

    let state = Arc::new(ProcessState {
        pid,
        reaped: Mutex::new(false),
    });
    Ok(SpawnedPty {
        stdin: stdin_write,
        stdout: stdout_read,
        pty_master: PtyMaster { file: pty_master },
        child: Child {
            state: Arc::clone(&state),
        },
        cancellation: Cancellation { state },
    })
}

fn pipe() -> io::Result<(File, File)> {
    let mut descriptors = [-1; 2];
    // SAFETY: `descriptors` points to two writable file-descriptor slots.
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe returned owned file descriptors on success.
    let read = unsafe { File::from_raw_fd(descriptors[0]) };
    // SAFETY: pipe returned owned file descriptors on success.
    let write = unsafe { File::from_raw_fd(descriptors[1]) };
    Ok((move_above_stdio(read)?, move_above_stdio(write)?))
}

fn open_pty() -> io::Result<(File, File, CString)> {
    let mut master = -1;
    let mut slave = -1;
    let mut name = [0_i8; libc::PATH_MAX as usize];
    // SAFETY: all pointers point to writable storage. Null termios/winsize request system defaults.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            name.as_mut_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty returned owned file descriptors on success.
    let master = unsafe { File::from_raw_fd(master) };
    // SAFETY: openpty returned owned file descriptors on success.
    let slave = unsafe { File::from_raw_fd(slave) };
    let master = move_above_stdio(master)?;
    let slave = move_above_stdio(slave)?;
    disable_echo(&slave)?;
    // SAFETY: successful openpty writes a NUL-terminated PTY path to the supplied PATH_MAX buffer.
    let name = unsafe { CStr::from_ptr(name.as_ptr()) };
    let name = CString::new(name.to_bytes()).map_err(|_| invalid_input("PTY slave path"))?;
    Ok((master, slave, name))
}

fn move_above_stdio(file: File) -> io::Result<File> {
    if file.as_raw_fd() >= MIN_CHILD_FD {
        set_close_on_exec(&file)?;
        return Ok(file);
    }
    // SAFETY: `file` owns a valid descriptor; F_DUPFD requests a distinct descriptor at least 3.
    let duplicated = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, MIN_CHILD_FD) };
    if duplicated == -1 {
        return Err(io::Error::last_os_error());
    }
    drop(file);
    // SAFETY: fcntl returned a newly owned descriptor.
    let duplicated = unsafe { File::from_raw_fd(duplicated) };
    set_close_on_exec(&duplicated)?;
    Ok(duplicated)
}

fn set_close_on_exec(file: &File) -> io::Result<()> {
    // SAFETY: `file` owns a valid descriptor.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `file` owns a valid descriptor and flags came from F_GETFD.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn disable_echo(slave: &File) -> io::Result<()> {
    let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `settings` is valid uninitialized storage for tcgetattr to fill.
    if unsafe { libc::tcgetattr(slave.as_raw_fd(), settings.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: tcgetattr succeeded above, so the termios value is initialized.
    let mut settings = unsafe { settings.assume_init() };
    settings.c_lflag &= !(libc::ECHO | libc::ECHONL);
    // SAFETY: `settings` is initialized and `slave` is a valid terminal descriptor.
    if unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &settings) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn child_environment(extra_environment: &[(OsString, OsString)]) -> io::Result<Vec<CString>> {
    let mut values = std::env::vars_os()
        .filter(|(name, _)| !is_askpass(name))
        .collect::<BTreeMap<_, _>>();
    for (name, value) in extra_environment {
        validate_environment_name(name)?;
        if !is_askpass(name) {
            values.insert(name.clone(), value.clone());
        }
    }
    values
        .into_iter()
        .map(|(name, value)| {
            let mut pair = name;
            pair.push("=");
            pair.push(value);
            c_string(&pair, "embedded PTY environment")
        })
        .collect()
}

fn validate_environment_name(name: &OsStr) -> io::Result<()> {
    if name.is_empty() || name.as_bytes().contains(&b'=') {
        return Err(invalid_input("embedded PTY environment name"));
    }
    Ok(())
}

fn is_askpass(name: &OsStr) -> bool {
    name == OsStr::new(SSH_ASKPASS) || name == OsStr::new(SSH_ASKPASS_REQUIRE)
}

fn c_string(value: &OsStr, context: &'static str) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| invalid_input(context))
}

fn invalid_input(context: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, context)
}

struct FileActions {
    raw: libc::posix_spawn_file_actions_t,
}

impl FileActions {
    fn new() -> io::Result<Self> {
        let mut raw = ptr::null_mut();
        // SAFETY: Darwin initializes the opaque handle stored at `raw`.
        let result = unsafe { libc::posix_spawn_file_actions_init(&mut raw) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(Self { raw })
    }

    fn add_dup2(&mut self, from: RawFd, to: RawFd) -> io::Result<()> {
        // SAFETY: `raw` is an initialized file-actions handle; descriptors are live until spawn.
        let result = unsafe { libc::posix_spawn_file_actions_adddup2(&mut self.raw, from, to) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }

    fn add_open(
        &mut self,
        fd: RawFd,
        path: &CStr,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<()> {
        // SAFETY: `raw` is initialized and `path` is NUL-terminated for the duration of this call.
        let result = unsafe {
            libc::posix_spawn_file_actions_addopen(&mut self.raw, fd, path.as_ptr(), flags, mode)
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        // SAFETY: raw was initialized exactly once by `new` and is destroyed exactly once here.
        let _ = unsafe { libc::posix_spawn_file_actions_destroy(&mut self.raw) };
    }
}

struct SpawnAttributes {
    raw: libc::posix_spawnattr_t,
}

impl SpawnAttributes {
    fn new() -> io::Result<Self> {
        let mut raw = ptr::null_mut();
        // SAFETY: Darwin initializes the opaque handle stored at `raw`.
        let result = unsafe { libc::posix_spawnattr_init(&mut raw) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(Self { raw })
    }

    fn set_flags(&mut self, flags: libc::c_short) -> io::Result<()> {
        // SAFETY: `raw` is an initialized Darwin spawn-attribute handle.
        let result = unsafe { libc::posix_spawnattr_setflags(&mut self.raw, flags) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }
}

impl Drop for SpawnAttributes {
    fn drop(&mut self) {
        // SAFETY: raw was initialized exactly once by `new` and is destroyed exactly once here.
        let _ = unsafe { libc::posix_spawnattr_destroy(&mut self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_removes_askpass_environment() {
        let executable = std::path::Path::new("/usr/bin/env");
        let argv = vec![executable.as_os_str().to_os_string()];
        let extra_environment = vec![
            (
                OsString::from("SSH_ASKPASS"),
                OsString::from("/tmp/not-allowed"),
            ),
            (
                OsString::from("SSH_ASKPASS_REQUIRE"),
                OsString::from("force"),
            ),
        ];
        let mut spawned = spawn(executable, &argv, &extra_environment).expect("spawn env");
        let mut output = String::new();
        spawned
            .stdout
            .read_to_string(&mut output)
            .expect("read child environment");
        let status = spawned.child.wait().expect("reap env child");
        assert!(status.is_some());
        assert!(!output.contains("SSH_ASKPASS="));
        assert!(!output.contains("SSH_ASKPASS_REQUIRE="));
    }
}
