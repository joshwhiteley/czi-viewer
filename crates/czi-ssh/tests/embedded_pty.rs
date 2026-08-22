#![cfg(target_os = "macos")]

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use czi_ssh::{
    OpenSshConfig, SftpError, SftpLocation, SftpSession, SharedSftpSession, SshConsole, SshProfile,
};

const PASSWORD: &[u8] = b"opaque-password-for-pty-test\n";

fn fake_executor() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_czi-ssh-pty-fake-child"))
}

fn profile(value: &str) -> SshProfile {
    SshProfile::new(value).expect("valid test SSH profile")
}

fn wait_for_console(console: &mut SshConsole, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
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
        .recv_timeout(Duration::from_secs(3))
        .expect("blocked SFTP initialization must return after cancellation");
    assert!(matches!(result, Err(SftpError::ChildExited { .. })));
    worker.join().expect("join cancellation worker");
}

#[test]
fn authenticated_cancellation_allows_a_fresh_second_sftp_initialization() {
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
        .recv_timeout(Duration::from_secs(3))
        .expect("authenticated SFTP operation must unblock after cancellation");
    assert!(matches!(result, Err(SftpError::ChildExited { .. })));
    blocked
        .join()
        .expect("join authenticated cancellation worker");
    drop(shared);

    let (pending, _console) = SftpSession::start_embedded_with_executor(
        &profile("block-after-version@example.test"),
        &OpenSshConfig::new(),
        &fake_executor(),
    )
    .expect("start fresh fake PTY executor");
    let cancellation = pending.cancellation();
    let session = pending
        .initialize()
        .expect("fresh second SFTP initialization");
    cancellation
        .cancel()
        .expect("cancel fresh second SFTP child before dropping it");
    drop(session);
}
