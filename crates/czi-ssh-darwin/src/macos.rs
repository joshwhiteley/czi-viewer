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
const CHILD_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

unsafe extern "C" {
    fn ptsname_r(
        fd: libc::c_int,
        buffer: *mut libc::c_char,
        buffer_length: libc::size_t,
    ) -> libc::c_int;
}

/// A spawned child whose stdin and stdout are independent binary stream endpoints and whose stderr
/// and controlling terminal are a local PTY.
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
    lifecycle: Mutex<ProcessLifecycle>,
}

struct ProcessLifecycle {
    leader_reaped: bool,
    process_group_terminated: bool,
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

    /// Wait briefly for the child to be reaped.
    ///
    /// The returned status is the Darwin wait status suitable for
    /// `std::os::unix::process::ExitStatusExt::from_raw`.
    ///
    /// # Errors
    ///
    /// Returns an error when waiting fails or exceeds the bounded reap timeout.
    pub fn wait(&mut self) -> io::Result<Option<i32>> {
        let deadline = std::time::Instant::now() + CHILD_REAP_TIMEOUT;
        loop {
            if let Some(status) = self.wait_once()? {
                return Ok(Some(status));
            }
            if self.is_leader_reaped()? {
                return Ok(None);
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out reaping embedded child",
                ));
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
        if let Some(status) = self.wait_once()? {
            return Ok(Some(status));
        }
        self.terminate_process_group()?;
        self.wait()
    }

    fn is_leader_reaped(&self) -> io::Result<bool> {
        self.state
            .lifecycle
            .lock()
            .map(|lifecycle| lifecycle.leader_reaped)
            .map_err(|_| io::Error::other("embedded child lifecycle lock poisoned"))
    }

    fn wait_once(&mut self) -> io::Result<Option<i32>> {
        let mut lifecycle = self
            .state
            .lifecycle
            .lock()
            .map_err(|_| io::Error::other("embedded child lifecycle lock poisoned"))?;
        if lifecycle.leader_reaped {
            return Ok(None);
        }
        if lifecycle.process_group_terminated {
            return try_reap_terminated_child(self.state.pid, &mut lifecycle);
        }

        if !leader_is_waitable(self.state.pid)? {
            return Ok(None);
        }
        terminate_process_group_locked(self.state.pid, &mut lifecycle, true)?;

        let mut status = 0;
        loop {
            // SAFETY: WNOWAIT confirmed this exact child is waitable and status is writable.
            let result = unsafe { libc::waitpid(self.state.pid, &mut status, 0) };
            if result == self.state.pid {
                lifecycle.leader_reaped = true;
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

    fn terminate_process_group(&self) -> io::Result<()> {
        let mut lifecycle = self
            .state
            .lifecycle
            .lock()
            .map_err(|_| io::Error::other("embedded child lifecycle lock poisoned"))?;
        let leader_waitable = leader_is_waitable(self.state.pid)?;
        terminate_process_group_locked(self.state.pid, &mut lifecycle, leader_waitable)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.terminate_and_wait().is_err() {
            reap_child_in_background(Arc::clone(&self.state));
        }
    }
}

impl Cancellation {
    /// Immediately terminate the child process group. The owner of [`Child`] must still reap it.
    ///
    /// # Errors
    ///
    /// Returns an error when signalling the child process group fails.
    pub fn cancel(&self) -> io::Result<()> {
        let mut lifecycle = self
            .state
            .lifecycle
            .lock()
            .map_err(|_| io::Error::other("embedded child lifecycle lock poisoned"))?;
        if lifecycle.process_group_terminated {
            return Ok(());
        }
        let leader_waitable = leader_is_waitable(self.state.pid)?;
        terminate_process_group_locked(self.state.pid, &mut lifecycle, leader_waitable)
    }
}

fn terminate_process_group_locked(
    pid: libc::pid_t,
    lifecycle: &mut ProcessLifecycle,
    leader_waitable: bool,
) -> io::Result<()> {
    if lifecycle.process_group_terminated {
        return Ok(());
    }
    if lifecycle.leader_reaped {
        return Err(io::Error::other(
            "embedded child group was not terminated before its leader was reaped",
        ));
    }
    // SAFETY: a live or waitable leader keeps its private session/process-group ID from being
    // recycled. A negative PID targets the whole private group, including SSH helpers.
    let group_result = unsafe { libc::kill(-pid, libc::SIGKILL) };
    let group_error = io::Error::last_os_error();
    if group_result == -1
        && group_error.raw_os_error() != Some(libc::ESRCH)
        // Darwin returns EPERM for a zombie session leader whose group has no live members.
        && !(leader_waitable && group_error.raw_os_error() == Some(libc::EPERM))
    {
        return Err(group_error);
    }
    // SAFETY: pid is the unreaped leader returned by posix_spawn. This direct signal is a
    // fallback for Darwin session/process-group edge cases; it cannot target a recycled PID.
    let leader_result = unsafe { libc::kill(pid, libc::SIGKILL) };
    let leader_error = io::Error::last_os_error();
    if leader_result == -1
        && leader_error.raw_os_error() != Some(libc::ESRCH)
        && !(leader_waitable && leader_error.raw_os_error() == Some(libc::EPERM))
    {
        return Err(leader_error);
    }
    lifecycle.process_group_terminated = true;
    Ok(())
}

fn try_reap_terminated_child(
    pid: libc::pid_t,
    lifecycle: &mut ProcessLifecycle,
) -> io::Result<Option<i32>> {
    let mut status = 0;
    // SAFETY: status is writable and pid is the still-unreaped posix_spawn child.
    let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if result == pid {
        lifecycle.leader_reaped = true;
        return Ok(Some(status));
    }
    if result == 0 {
        return Ok(None);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ECHILD) {
        lifecycle.leader_reaped = true;
        return Ok(None);
    }
    Err(error)
}

fn reap_child_in_background(state: Arc<ProcessState>) {
    let _ = std::thread::Builder::new()
        .name(String::from("czi-ssh-child-reaper"))
        .spawn(move || {
            let needs_reap = {
                let Ok(mut lifecycle) = state.lifecycle.lock() else {
                    return;
                };
                if !lifecycle.leader_reaped && !lifecycle.process_group_terminated {
                    let _ = terminate_process_group_locked(state.pid, &mut lifecycle, false);
                }
                !lifecycle.leader_reaped
            };
            if !needs_reap {
                return;
            }
            // SAFETY: the leader remains unreaped, so its PID cannot be recycled. Retry the
            // direct signal before the background wait in case an earlier group signal raced an
            // exiting Darwin session leader.
            let _ = unsafe { libc::kill(state.pid, libc::SIGKILL) };
            let mut status = 0;
            loop {
                // SAFETY: the background reaper owns the last Child lifecycle path for this PID.
                let result = unsafe { libc::waitpid(state.pid, &mut status, 0) };
                if result == state.pid
                    || (result == -1
                        && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
                {
                    if let Ok(mut lifecycle) = state.lifecycle.lock() {
                        lifecycle.leader_reaped = true;
                    }
                    return;
                }
                if result == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
        });
}

fn leader_is_waitable(pid: libc::pid_t) -> io::Result<bool> {
    // `WNOWAIT` keeps an exited leader waitable. That keeps its PID and process-group ID from
    // being recycled until the group is signalled and waitpid below reaps it.
    // SAFETY: a zeroed siginfo_t is valid writable storage for waitid.
    let mut information = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    // SAFETY: `information` is valid writable storage and `pid` belongs to this child.
    let wait_result = unsafe {
        libc::waitid(
            libc::P_PID,
            libc::id_t::try_from(pid)
                .map_err(|_| io::Error::other("invalid embedded child process identifier"))?,
            &mut information,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if wait_result == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    if information.si_pid == 0 {
        return Ok(false);
    }
    if information.si_pid != pid {
        return Err(io::Error::other("waitid returned an unexpected process"));
    }
    Ok(true)
}

/// Spawn an absolute-path program with binary stdin/stdout pipes and a controlling PTY.
///
/// `argv[0]` must equal `executable`. The child receives no `SSH_ASKPASS` or
/// `SSH_ASKPASS_REQUIRE`, even when they are present in the parent or `extra_environment`.
/// Child fd 0 and fd 1 are independent directional binary socket endpoints, and fd 2 is the
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

    let (stdin_read, stdin_write) = binary_socket_pair()?;
    let (stdout_read, stdout_write) = binary_socket_pair()?;
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
        lifecycle: Mutex::new(ProcessLifecycle {
            leader_reaped: false,
            process_group_terminated: false,
        }),
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

fn binary_socket_pair() -> io::Result<(File, File)> {
    let mut descriptors = [-1; 2];
    // SAFETY: `descriptors` points to two writable file-descriptor slots.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM,
            0,
            descriptors.as_mut_ptr(),
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socketpair returned owned file descriptors on success.
    let first = unsafe { File::from_raw_fd(descriptors[0]) };
    // SAFETY: socketpair returned owned file descriptors on success.
    let second = unsafe { File::from_raw_fd(descriptors[1]) };
    Ok((
        duplicate_close_on_exec(first)?,
        duplicate_close_on_exec(second)?,
    ))
}

fn open_pty() -> io::Result<(File, File, CString)> {
    // SAFETY: posix_openpt has no pointer arguments and returns an owned descriptor on success.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: posix_openpt returned an owned descriptor on success.
    let master = unsafe { File::from_raw_fd(master) };
    // SAFETY: master is a valid pseudo-terminal descriptor.
    if unsafe { libc::grantpt(master.as_raw_fd()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: master is a valid unlocked pseudo-terminal descriptor.
    if unsafe { libc::unlockpt(master.as_raw_fd()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut name = [0_i8; libc::PATH_MAX as usize];
    // SAFETY: the buffer is writable PATH_MAX storage for the slave pathname.
    if unsafe { ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful ptsname_r writes a NUL-terminated PTY path to the supplied buffer.
    let name = unsafe { CStr::from_ptr(name.as_ptr()) };
    // SAFETY: name is a NUL-terminated pathname and open returns an owned descriptor on success.
    let slave = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open returned an owned descriptor on success.
    let slave = unsafe { File::from_raw_fd(slave) };
    disable_echo(&slave)?;
    let name = CString::new(name.to_bytes()).map_err(|_| invalid_input("PTY slave path"))?;
    Ok((master, slave, name))
}

fn duplicate_close_on_exec(file: File) -> io::Result<File> {
    // SAFETY: `file` owns a valid descriptor; F_DUPFD_CLOEXEC atomically creates a distinct
    // descriptor at least 3 with FD_CLOEXEC set.
    let duplicated = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, MIN_CHILD_FD) };
    if duplicated == -1 {
        return Err(io::Error::last_os_error());
    }
    drop(file);
    // SAFETY: fcntl returned a newly owned descriptor.
    Ok(unsafe { File::from_raw_fd(duplicated) })
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

    const DESCENDANT_MODE: &str = "CZI_SSH_DARWIN_TEST_DESCENDANT";

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

    #[test]
    #[allow(
        clippy::zombie_processes,
        reason = "the parent test must reap the session leader and kill this inherited descendant"
    )]
    fn descendant_child_entry() {
        if std::env::var_os(DESCENDANT_MODE).is_none() {
            return;
        }
        std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn descendant that inherits the private process group");
    }

    #[test]
    fn reaping_a_leader_terminates_its_descendant_group() {
        let executable = std::env::current_exe().expect("current czi-ssh-darwin test executable");
        let argv = vec![
            executable.clone().into_os_string(),
            "--exact".into(),
            "macos::tests::descendant_child_entry".into(),
            "--nocapture".into(),
        ];
        let extra_environment = vec![(OsString::from(DESCENDANT_MODE), OsString::from("1"))];
        let SpawnedPty {
            stdin,
            stdout,
            pty_master,
            mut child,
            cancellation,
        } = spawn(&executable, &argv, &extra_environment).expect("spawn descendant test child");
        let process_group = child.id();
        drop((stdin, stdout, pty_master));
        assert!(child.wait().expect("reap leader").is_some());
        cancellation
            .cancel()
            .expect("cancellation after leader reap is already group-safe");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !process_group_is_gone(process_group).expect("inspect descendant process group") {
            assert!(
                std::time::Instant::now() < deadline,
                "descendant process group {process_group} survived leader reaping"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn process_group_is_gone(process_group: u32) -> io::Result<bool> {
        let process_group = libc::pid_t::try_from(process_group)
            .map_err(|_| io::Error::other("invalid test process group"))?;
        // SAFETY: this test owns the private process group created by posix_spawn.
        let result = unsafe { libc::kill(-process_group, 0) };
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(true)
        } else {
            Err(error)
        }
    }
}
