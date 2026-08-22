#![cfg(target_os = "macos")]

use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use czi_ssh::{
    OpenSshConfig, SftpError, SftpLocation, SftpSession, SharedSftpSession, SshConsole, SshProfile,
};

const PASSWORD: &[u8] = b"opaque-password-for-pty-test\n";
const CI_SAFE_TIMEOUT: Duration = Duration::from_secs(10);
const ISOLATED_TEST: &str = "CZI_SSH_EMBEDDED_PTY_ISOLATED";

fn embedded_child_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn run_in_isolated_process(test_name: &str) -> bool {
    if std::env::var_os(ISOLATED_TEST).as_deref() == Some(std::ffi::OsStr::new(test_name)) {
        return false;
    }
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", test_name, "--nocapture"])
        .env(ISOLATED_TEST, test_name)
        .status()
        .expect("run isolated embedded PTY test");
    assert!(
        status.success(),
        "isolated embedded PTY test failed: {test_name}"
    );
    true
}

fn fake_executor() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_czi-ssh-pty-fake-child"))
}

fn profile(value: &str) -> SshProfile {
    SshProfile::new(value).expect("valid test SSH profile")
}

fn wait_for_console(console: &mut SshConsole, expected: &str) {
    let deadline = Instant::now() + CI_SAFE_TIMEOUT;
    loop {
        console.drain_output().expect("drain embedded console");
        if console.transcript().contains(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "embedded console did not contain {expected:?}: {:?}",
            console.transcript()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn pty_keeps_sftp_binary_and_password_input_out_of_console() {
    let _guard = embedded_child_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if run_in_isolated_process("pty_keeps_sftp_binary_and_password_input_out_of_console") {
        return;
    }
    let (pending, mut console) = SftpSession::start_embedded_with_executor(
        &profile("auth@example.test"),
        &OpenSshConfig::new(),
        &fake_executor(),
    )
    .expect("start fake PTY executor");

    wait_for_console(&mut console, "Password:");
    assert!(
        console
            .transcript()
            .contains("stderr is on the embedded PTY")
    );
    console
        .write_input(PASSWORD)
        .expect("write immediate password input");
    let session = pending
        .initialize()
        .expect("strict SFTP VERSION from fake child");
    wait_for_console(&mut console, "Authentication accepted.");
    drop(session);
    assert!(
        !console
            .transcript()
            .contains("opaque-password-for-pty-test")
    );
    assert!(!console.transcript().contains("fd1-binary-sentinel"));
}

#[test]
fn pty_cancellation_reaps_a_child_blocked_on_terminal_input() {
    let _guard = embedded_child_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if run_in_isolated_process("pty_cancellation_reaps_a_child_blocked_on_terminal_input") {
        return;
    }
    let (pending, mut console) = SftpSession::start_embedded_with_executor(
        &profile("block@example.test"),
        &OpenSshConfig::new(),
        &fake_executor(),
    )
    .expect("start blocking fake PTY executor");
    let cancellation = pending.cancellation();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        finished_tx
            .send(pending.initialize())
            .expect("report pending initialization result");
    });

    wait_for_console(&mut console, "Waiting for terminal input.");
    cancellation.cancel().expect("cancel blocked child");
    let result = finished_rx
        .recv_timeout(CI_SAFE_TIMEOUT)
        .expect("blocked SFTP initialization must return after cancellation");
    assert!(matches!(result, Err(SftpError::ChildExited { .. })));
    worker.join().expect("join cancellation worker");
}

#[test]
fn authenticated_cancellation_allows_a_fresh_second_sftp_initialization() {
    let _guard = embedded_child_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if run_in_isolated_process(
        "authenticated_cancellation_allows_a_fresh_second_sftp_initialization",
    ) {
        return;
    }
    let (pending, mut console) = SftpSession::start_embedded_with_executor(
        &profile("block-after-version@example.test"),
        &OpenSshConfig::new(),
        &fake_executor(),
    )
    .expect("start authenticated blocking fake PTY executor");
    let cancellation = pending.cancellation();
    let session = pending
        .initialize()
        .expect("strict SFTP VERSION from blocking child");
    wait_for_console(&mut console, "SFTP VERSION accepted");
    let shared = SharedSftpSession::new_embedded(session, 7, cancellation);
    assert!(!shared.cancel_embedded_connection(8));

    let blocked_session = shared.clone();
    let (finished_tx, finished_rx) = mpsc::channel();
    let blocked = thread::spawn(move || {
        finished_tx
            .send(blocked_session.with_session(|session| {
                session.realpath(&SftpLocation::new(".").expect("fixed test location"))
            }))
            .expect("report blocked authenticated operation");
    });
    thread::sleep(Duration::from_millis(50));
    assert!(shared.cancel_embedded_connection(7));
    let result = finished_rx
        .recv_timeout(CI_SAFE_TIMEOUT)
        .expect("authenticated SFTP operation must unblock after cancellation");
    assert!(matches!(result, Err(SftpError::ChildExited { .. })));
    blocked
        .join()
        .expect("join authenticated cancellation worker");
    drop(shared);

    let (pending, mut console) = SftpSession::start_embedded_with_executor(
        &profile("block-after-version@example.test"),
        &OpenSshConfig::new(),
        &fake_executor(),
    )
    .expect("start fresh fake PTY executor");
    let cancellation = pending.cancellation();
    let session = pending
        .initialize()
        .expect("fresh second SFTP initialization");
    wait_for_console(&mut console, "SFTP VERSION accepted");
    cancellation
        .cancel()
        .expect("cancel fresh second SFTP child before dropping it");
    drop((session, console));
}

#[test]
fn second_embedded_child_is_rejected_until_the_first_is_reaped() {
    let _guard = embedded_child_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if run_in_isolated_process("second_embedded_child_is_rejected_until_the_first_is_reaped") {
        return;
    }
    let (pending, console) = SftpSession::start_embedded_with_executor(
        &profile("block@example.test"),
        &OpenSshConfig::new(),
        &fake_executor(),
    )
    .expect("start first fake PTY executor");
    match SftpSession::start_embedded_with_executor(
        &profile("block@example.test"),
        &OpenSshConfig::new(),
        &fake_executor(),
    ) {
        Err(SftpError::Spawn { source }) => {
            assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
            assert!(source.to_string().contains("already active"));
        }
        Err(error) => panic!("unexpected second-child error: {error}"),
        Ok(_) => panic!("second embedded child unexpectedly started"),
    }
    let mut console = console;
    wait_for_console(&mut console, "Waiting for terminal input.");
    pending
        .cancellation()
        .cancel()
        .expect("cancel first embedded child");
    drop((pending, console));
}
