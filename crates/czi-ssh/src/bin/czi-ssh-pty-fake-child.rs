//! Test-only PTY executor used by `tests/embedded_pty.rs`.

#[cfg(target_os = "macos")]
use std::io::{Read, Write};

#[cfg(target_os = "macos")]
const PASSWORD: &[u8] = b"opaque-password-for-pty-test\n";
#[cfg(target_os = "macos")]
const SFTP_INIT: [u8; 9] = [0, 0, 0, 5, 1, 0, 0, 0, 3];

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments
        .get(1)
        .is_none_or(|argument| argument != czi_ssh::EMBEDDED_PTY_EXEC_MODE)
    {
        return Err("fake PTY executor requires the embedded executor mode".into());
    }
    if arguments
        .get(2)
        .is_none_or(|argument| argument != czi_ssh::OPENSSH_PATH)
    {
        return Err("fake PTY executor must receive only /usr/bin/ssh".into());
    }
    let profile = arguments
        .get(arguments.len().saturating_sub(2))
        .and_then(|argument| argument.to_str())
        .ok_or("fake PTY executor requires a UTF-8 profile")?;
    verify_production_argv(&arguments[2..], profile)?;
    czi_ssh_darwin::claim_controlling_terminal()?;
    if profile == "block@example.test" {
        block_on_terminal_input()
    } else {
        authenticate_and_speak_sftp()
    }
}

#[cfg(target_os = "macos")]
fn verify_production_argv(
    actual: &[std::ffi::OsString],
    profile: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let expected = [
        czi_ssh::OPENSSH_PATH,
        "-o",
        "BatchMode=no",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "StrictHostKeyChecking=ask",
        "-T",
        "-s",
        profile,
        "sftp",
    ];
    let actual = actual
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let expected = expected.into_iter().map(str::to_owned).collect::<Vec<_>>();
    if actual != expected {
        return Err(format!("unexpected production OpenSSH argv: {actual:?}").into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn authenticate_and_speak_sftp() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("SSH_ASKPASS").is_some()
        || std::env::var_os("SSH_ASKPASS_REQUIRE").is_some()
    {
        return Err("fake child received an ASKPASS environment variable".into());
    }
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    eprintln!("stderr is on the embedded PTY");
    tty.write_all(b"Password: ")?;
    tty.flush()?;

    let mut password = vec![0; PASSWORD.len()];
    tty.read_exact(&mut password)?;
    if password != PASSWORD {
        return Err("wrong password-like PTY input".into());
    }

    let mut stdin = std::io::stdin().lock();
    let mut init = [0; SFTP_INIT.len()];
    stdin.read_exact(&mut init)?;
    if init != SFTP_INIT {
        return Err("fd 0 was not an isolated binary SFTP pipe".into());
    }

    let mut version = 3_u32.to_be_bytes().to_vec();
    push_string(&mut version, b"fd1-binary-sentinel");
    push_string(&mut version, b"\0\xff\x01");
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&u32::try_from(version.len() + 1)?.to_be_bytes())?;
    stdout.write_all(&[2])?;
    stdout.write_all(&version)?;
    stdout.flush()?;

    tty.write_all(b"Authentication accepted.\n")?;
    tty.flush()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn block_on_terminal_input() -> Result<(), Box<dyn std::error::Error>> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    tty.write_all(b"Waiting for terminal input.\n")?;
    tty.flush()?;
    let mut byte = [0];
    tty.read_exact(&mut byte)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn push_string(payload: &mut Vec<u8>, value: &[u8]) {
    payload.extend_from_slice(
        &u32::try_from(value.len())
            .expect("fixed fake payload")
            .to_be_bytes(),
    );
    payload.extend_from_slice(value);
}

#[cfg(not(target_os = "macos"))]
fn main() {}
