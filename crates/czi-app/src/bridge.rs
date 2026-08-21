//! The visible-Terminal interactive SFTP bridge.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use czi_ssh::{OpenSshConfig, SshProfile};

#[cfg(unix)]
use czi_ssh::{BridgeListener, authenticate_bridge_server};
#[cfg(unix)]
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

/// Hidden same-executable CLI mode used only by the copied Terminal bridge command.
pub const BRIDGE_MODE: &str = "--czi-sftp-bridge";

struct BridgeInvocation {
    profile: SshProfile,
    socket_path: PathBuf,
}

/// Run the bridge when the process arguments request the exact hidden mode.
///
/// Returns `Ok(false)` for normal GUI invocation and `Ok(true)` after the bridge exits.
///
/// # Errors
///
/// Returns an error for malformed bridge arguments, unsafe socket paths, or bridge I/O failures.
pub fn run_if_requested() -> Result<bool, Box<dyn std::error::Error>> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let Some(invocation) = parse_invocation(&arguments)? else {
        return Ok(false);
    };
    run(&invocation)?;
    Ok(true)
}

fn parse_invocation(arguments: &[OsString]) -> Result<Option<BridgeInvocation>, io::Error> {
    if arguments
        .get(1)
        .is_none_or(|argument| argument != BRIDGE_MODE)
    {
        return Ok(None);
    }
    if arguments.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interactive SFTP bridge requires exactly a profile and private socket path",
        ));
    }
    let profile = arguments[2].clone().into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interactive SFTP bridge profile must be UTF-8",
        )
    })?;
    let profile = SshProfile::new(profile)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(Some(BridgeInvocation {
        profile,
        socket_path: PathBuf::from(&arguments[3]),
    }))
}

#[cfg(unix)]
fn run(invocation: &BridgeInvocation) -> Result<(), Box<dyn std::error::Error>> {
    let listener = BridgeListener::bind(&invocation.socket_path)?;
    eprintln!("CZI Viewer interactive SFTP bridge is waiting for the viewer.");
    eprintln!(
        "Return to the viewer and click Retry, Home, or Browse. Enter any password, 2FA, or host-key confirmation here in Terminal. Keep this Terminal open while the remote file is in use."
    );
    let Some(mut stream) = listener.accept()? else {
        return Ok(());
    };
    authenticate_bridge_server(&mut stream, &invocation.profile)?;
    eprintln!(
        "Viewer connected. OpenSSH may now prompt in this Terminal; SFTP bytes are not printed here."
    );
    proxy_sftp(stream, &invocation.profile)?;
    Ok(())
}

#[cfg(not(unix))]
fn run(_invocation: &BridgeInvocation) -> Result<(), Box<dyn std::error::Error>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "interactive SFTP bridging requires Unix-domain sockets",
    )
    .into())
}

#[cfg(unix)]
fn proxy_sftp(stream: UnixStream, profile: &SshProfile) -> io::Result<()> {
    let argv = OpenSshConfig::interactive_sftp_argv(profile);
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .env_remove("SSH_ASKPASS")
        .env_remove("SSH_ASKPASS_REQUIRE");
    let mut child = command.spawn()?;
    let result = proxy_child(&mut child, stream);
    if result.is_err() {
        terminate_child(&mut child);
    }
    result
}

#[cfg(unix)]
fn proxy_child(child: &mut std::process::Child, stream: UnixStream) -> io::Result<()> {
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("capture OpenSSH stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("capture OpenSSH stdout"))?;
    let input_stream = stream.try_clone()?;
    let output_stream = stream.try_clone()?;
    let shutdown_stream = stream;
    let (finished_tx, finished_rx) = mpsc::channel();

    let input = copy_in_background(input_stream, stdin, finished_tx.clone());
    let output = copy_in_background(stdout, output_stream, finished_tx);
    let _ = finished_rx.recv();
    let _ = shutdown_stream.shutdown(Shutdown::Both);
    let (status, terminated_for_disconnect) = match child.try_wait() {
        Ok(Some(status)) => (status, false),
        Ok(None) => (
            terminate_child(child).ok_or_else(|| io::Error::other("reap OpenSSH child"))?,
            true,
        ),
        Err(source) => {
            let _ = terminate_child(child);
            let _ = input.join();
            let _ = output.join();
            return Err(source);
        }
    };
    let _ = input.join();
    let _ = output.join();
    if terminated_for_disconnect || status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("OpenSSH exited with {status}")))
    }
}

#[cfg(unix)]
fn terminate_child(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    let _ = child.kill();
    child.wait().ok()
}

#[cfg(unix)]
fn copy_in_background<R, W>(
    mut reader: R,
    mut writer: W,
    finished: mpsc::Sender<()>,
) -> thread::JoinHandle<io::Result<()>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    thread::spawn(move || {
        let result = io::copy(&mut reader, &mut writer).and_then(|_| writer.flush());
        let _ = finished.send(());
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_invocation_requires_exact_mode_and_argument_count() {
        let executable = OsString::from("czi-viewer");
        assert!(
            parse_invocation(std::slice::from_ref(&executable))
                .unwrap()
                .is_none()
        );
        assert!(
            parse_invocation(&[
                executable.clone(),
                OsString::from(BRIDGE_MODE),
                OsString::from("profile"),
            ])
            .is_err()
        );
        assert!(
            parse_invocation(&[
                executable,
                OsString::from(BRIDGE_MODE),
                OsString::from("-oProxyCommand=bad"),
                OsString::from("/tmp/cz-safe/s"),
            ])
            .is_err()
        );
    }
}
