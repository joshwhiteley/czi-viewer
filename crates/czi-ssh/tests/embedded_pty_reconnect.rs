#![cfg(target_os = "macos")]

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use czi_ssh::{
    OpenSshConfig, SftpError, SftpLocation, SftpSession, SharedSftpSession, SshConsole, SshProfile,
};

const TIMEOUT: Duration = Duration::from_secs(10);

fn fake_executor() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_czi-ssh-pty-fake-child"))
}

fn profile() -> SshProfile {
    SshProfile::new("block-after-version@example.test").expect("valid test SSH profile")
}

fn wait_for_console(console: &mut SshConsole) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        console.drain_output().expect("drain embedded console");
        if console.transcript().contains("SFTP VERSION accepted") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "embedded console did not reach VERSION"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sequential_reconnects_release_each_embedded_child_before_the_next_start() {
    let mut consoles = Vec::new();
    for _ in 0..10 {
        let deadline = Instant::now() + TIMEOUT;
        let (pending, mut console) = loop {
            match SftpSession::start_embedded_with_executor(
                &profile(),
                &OpenSshConfig::new(),
                &fake_executor(),
            ) {
                Ok(connection) => break connection,
                Err(SftpError::Spawn { source })
                    if source.kind() == std::io::ErrorKind::AlreadyExists
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("start sequential embedded child: {error}"),
            }
        };
        let cancellation = pending.cancellation();
        let session = pending.initialize().expect("initialize embedded child");
        wait_for_console(&mut console);
        let shared = SharedSftpSession::new_embedded(session, 1, cancellation);
        let blocked_shared = shared.clone();
        let (finished_tx, finished_rx) = mpsc::channel();
        let blocked = thread::spawn(move || {
            finished_tx
                .send(blocked_shared.with_session(|session| {
                    session.realpath(&SftpLocation::new(".").expect("fixed test location"))
                }))
                .expect("report blocked operation");
        });
        thread::sleep(Duration::from_millis(20));
        assert!(shared.cancel_embedded_connection(1));
        assert!(matches!(
            finished_rx
                .recv_timeout(TIMEOUT)
                .expect("blocked operation must unblock"),
            Err(SftpError::ChildExited { .. })
        ));
        blocked.join().expect("join blocked operation");
        drop(shared);
        consoles.push(console);
    }
    drop(consoles);
}
