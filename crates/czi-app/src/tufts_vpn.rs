#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io::{self, Read};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    pub const EXEC_MODE: &str = "--czi-tufts-vpn-pty-exec";
    pub const TARGET_HOST: &str = "login-prod.pax.tufts.edu";
    pub const TARGET_PORT: u16 = 22;
    pub const GATEWAY: &str = "https://vpn.tufts.edu/duo";
    const SCRIPT_MODE_ENV: &str = "CZI_TUFTS_VPN_SCRIPT_MODE";
    const SCRIPT_PAIR_ENV: &str = "CZI_TUFTS_VPN_TOOL_PAIR";
    const SCRIPT_PORT_ENV: &str = "CZI_TUFTS_VPN_LOCAL_PORT";
    const SCRIPT_PATH_ENV: &str = "CZI_TUFTS_VPN_SCRIPT_PATH";
    const SSH_BANNER_LIMIT: usize = 1_024;
    const SSH_BANNER_READ_TIMEOUT: Duration = Duration::from_millis(500);
    pub const READY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ToolPair {
        id: &'static str,
        openconnect: &'static str,
        ocproxy: &'static str,
    }

    const TOOL_PAIRS: [ToolPair; 2] = [
        ToolPair {
            id: "homebrew-arm64",
            openconnect: "/opt/homebrew/bin/openconnect",
            ocproxy: "/opt/homebrew/bin/ocproxy",
        },
        ToolPair {
            id: "homebrew-intel",
            openconnect: "/usr/local/bin/openconnect",
            ocproxy: "/usr/local/bin/ocproxy",
        },
    ];

    impl ToolPair {
        pub fn openconnect(self) -> &'static Path {
            Path::new(self.openconnect)
        }

        fn ocproxy(self) -> &'static Path {
            Path::new(self.ocproxy)
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct VpnUsername(String);

    impl VpnUsername {
        pub fn new(value: impl Into<String>) -> io::Result<Self> {
            let value = value.into();
            if value.is_empty() {
                return Err(invalid_input("Enter your Tufts VPN username."));
            }
            if value.len() > 255
                || !value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b'-')
                })
            {
                return Err(invalid_input(
                    "Tufts VPN username must use only letters, digits, '.', '_', '@', or '-'.",
                ));
            }
            Ok(Self(value))
        }

        pub fn as_str(&self) -> &str {
            &self.0
        }
    }

    pub struct VpnConsole {
        master: czi_ssh_darwin::PtyMaster,
        transcript: String,
        latest_output: String,
    }

    impl VpnConsole {
        pub fn drain_output(&mut self) -> io::Result<&str> {
            self.latest_output.clear();
            let mut raw = Vec::new();
            self.master
                .read_available(&mut raw, czi_ssh::SSH_CONSOLE_OUTPUT_LIMIT)?;
            for byte in raw {
                match byte {
                    b'\n' | b'\r' => append_fragment(&mut self.latest_output, "\n"),
                    b'\t' => append_fragment(&mut self.latest_output, "    "),
                    b' '..=b'~' => append_byte(&mut self.latest_output, byte),
                    _ => {}
                }
            }
            append_bounded(&mut self.transcript, &self.latest_output);
            Ok(&self.latest_output)
        }

        pub fn transcript(&self) -> &str {
            &self.transcript
        }

        pub fn clear_transcript(&mut self) {
            self.transcript.clear();
            self.latest_output.clear();
        }

        pub fn write_input(&mut self, input: &[u8]) -> io::Result<()> {
            self.master.write_input(input)
        }
    }

    pub struct VpnProcess {
        child: czi_ssh_darwin::Child,
        cancellation: czi_ssh_darwin::Cancellation,
        helper: ScriptHelper,
        port: u16,
    }

    impl VpnProcess {
        pub fn cancellation(&self) -> czi_ssh_darwin::Cancellation {
            self.cancellation.clone()
        }

        pub fn port(&self) -> u16 {
            self.port
        }

        pub fn wait_until_ready(&mut self, timeout: Duration) -> io::Result<()> {
            wait_for_ssh_banner(self.port, timeout, || {
                self.child.try_wait().map(|status| status.is_none())
            })
        }
    }

    impl Drop for VpnProcess {
        fn drop(&mut self) {
            let _ = self.cancellation.cancel();
            let _ = self.child.terminate_and_wait();
            self.helper.remove();
        }
    }

    pub fn start(username: &VpnUsername, executor: &Path) -> io::Result<(VpnProcess, VpnConsole)> {
        if !executor.is_absolute() {
            return Err(invalid_input("Tufts VPN executor must be absolute"));
        }
        let tools = find_tools()?;
        let helper = ScriptHelper::create(executor)?;
        let port = reserve_ephemeral_port()?;
        let openconnect = openconnect_argv(tools, username, helper.path(), executor);
        let environment = script_environment(tools, port, helper.path());
        let spawned = czi_ssh_darwin::spawn_terminal(executor, &openconnect, &environment)?;
        let czi_ssh_darwin::SpawnedTerminal {
            pty_master,
            child,
            cancellation,
        } = spawned;
        Ok((
            VpnProcess {
                child,
                cancellation,
                helper,
                port,
            },
            VpnConsole {
                master: pty_master,
                transcript: String::new(),
                latest_output: String::new(),
            },
        ))
    }

    /// Dispatch a fixed Tufts VPN PTY executor or ocproxy script helper before GUI startup.
    ///
    /// # Errors
    ///
    /// Returns an error when hidden-mode arguments, the helper environment, process setup, or an
    /// absolute executable fails validation or execution.
    pub fn run_executor_if_requested() -> io::Result<bool> {
        let arguments = std::env::args_os().collect::<Vec<_>>();
        if arguments
            .get(1)
            .is_some_and(|argument| argument == EXEC_MODE)
        {
            return run_openconnect_executor(&arguments).map(|never| match never {});
        }
        if std::env::var_os(SCRIPT_MODE_ENV).as_deref() == Some(OsStr::new("1")) {
            return run_ocproxy_script(&arguments).map(|never| match never {});
        }
        Ok(false)
    }

    fn run_openconnect_executor(arguments: &[OsString]) -> io::Result<std::convert::Infallible> {
        let pair = pair_from_environment()?;
        let actual = arguments
            .get(2..)
            .ok_or_else(|| invalid_input("Tufts VPN executor arguments are missing"))?;
        validate_openconnect_exec_argv(pair, actual)?;
        let helper_path = PathBuf::from(
            std::env::var_os(SCRIPT_PATH_ENV)
                .ok_or_else(|| invalid_input("Tufts VPN script path is missing"))?,
        );
        if actual.get(5).map(OsString::as_os_str) != Some(helper_path.as_os_str()) {
            return Err(invalid_input(
                "Tufts VPN executor script path does not match its private helper",
            ));
        }
        validate_helper_path(&helper_path)?;
        czi_ssh_darwin::claim_controlling_terminal_and_exec(pair.openconnect(), actual)
    }

    fn run_ocproxy_script(arguments: &[OsString]) -> io::Result<std::convert::Infallible> {
        if arguments.len() != 1 {
            return Err(invalid_input("Tufts VPN script accepts no arguments"));
        }
        let helper_path = PathBuf::from(
            std::env::var_os(SCRIPT_PATH_ENV)
                .ok_or_else(|| invalid_input("Tufts VPN script path is missing"))?,
        );
        if arguments[0] != helper_path.as_os_str() {
            return Err(invalid_input(
                "Tufts VPN script path does not match argv[0]",
            ));
        }
        validate_helper_path(&helper_path)?;
        if std::env::var_os("reason").as_deref() != Some(OsStr::new("connect")) {
            return Err(invalid_input("Tufts VPN script requires reason=connect"));
        }
        let vpn_fd =
            std::env::var("VPNFD").map_err(|_| invalid_input("Tufts VPN script requires VPNFD"))?;
        vpn_fd
            .parse::<i32>()
            .ok()
            .filter(|descriptor| *descriptor >= 0)
            .ok_or_else(|| invalid_input("Tufts VPN script VPNFD is invalid"))?;
        let pair = pair_from_environment()?;
        let port = port_from_environment()?;
        let forwarding = ocproxy_forwarding(port);
        let error = Command::new(pair.ocproxy())
            .arg("-L")
            .arg(forwarding)
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .exec();
        Err(error)
    }

    fn validate_openconnect_exec_argv(pair: ToolPair, actual: &[OsString]) -> io::Result<()> {
        if actual.len() != 8
            || actual[0] != pair.openconnect().as_os_str()
            || actual[1] != "--user"
            || VpnUsername::new(actual[2].to_string_lossy().into_owned()).is_err()
            || actual[3] != "--script-tun"
            || actual[4] != "--script"
            || !safe_helper_argument(Path::new(&actual[5]))
            || actual[6] != "--protocol=anyconnect"
            || actual[7] != GATEWAY
        {
            return Err(invalid_input(
                "invalid fixed Tufts VPN OpenConnect arguments",
            ));
        }
        Ok(())
    }

    fn openconnect_argv(
        pair: ToolPair,
        username: &VpnUsername,
        helper: &Path,
        executor: &Path,
    ) -> Vec<OsString> {
        vec![
            executor.as_os_str().to_os_string(),
            EXEC_MODE.into(),
            pair.openconnect().as_os_str().to_os_string(),
            "--user".into(),
            username.as_str().into(),
            "--script-tun".into(),
            "--script".into(),
            helper.as_os_str().to_os_string(),
            "--protocol=anyconnect".into(),
            GATEWAY.into(),
        ]
    }

    fn script_environment(pair: ToolPair, port: u16, helper: &Path) -> Vec<(OsString, OsString)> {
        vec![
            (SCRIPT_MODE_ENV.into(), "1".into()),
            (SCRIPT_PAIR_ENV.into(), pair.id.into()),
            (SCRIPT_PORT_ENV.into(), port.to_string().into()),
            (SCRIPT_PATH_ENV.into(), helper.as_os_str().to_os_string()),
        ]
    }

    fn pair_from_environment() -> io::Result<ToolPair> {
        let id = std::env::var(SCRIPT_PAIR_ENV)
            .map_err(|_| invalid_input("Tufts VPN tool-pair identity is missing"))?;
        TOOL_PAIRS
            .into_iter()
            .find(|pair| pair.id == id)
            .ok_or_else(|| invalid_input("Tufts VPN tool-pair identity is invalid"))
    }

    fn port_from_environment() -> io::Result<u16> {
        std::env::var(SCRIPT_PORT_ENV)
            .ok()
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| invalid_input("Tufts VPN local port is invalid"))
    }

    fn find_tools() -> io::Result<ToolPair> {
        find_tools_with(executable_file).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Tufts VPN tools were not found as a matching pair. Install them separately with: brew install openconnect ocproxy",
            )
        })
    }

    fn find_tools_with(mut available: impl FnMut(&Path) -> bool) -> Option<ToolPair> {
        TOOL_PAIRS.into_iter().find(|pair| {
            available(Path::new(pair.openconnect)) && available(Path::new(pair.ocproxy))
        })
    }

    fn executable_file(path: &Path) -> bool {
        fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }

    fn reserve_ephemeral_port() -> io::Result<u16> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Ok(port)
    }

    fn wait_for_ssh_banner(
        port: u16,
        timeout: Duration,
        mut child_is_live: impl FnMut() -> io::Result<bool>,
    ) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if !child_is_live()? {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "OpenConnect exited before the Tufts SSH tunnel became ready",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for the Tufts SSH tunnel",
                ));
            }
            let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
            if let Ok(mut stream) = TcpStream::connect_timeout(
                &address.into(),
                remaining.min(Duration::from_millis(200)),
            ) {
                stream.set_read_timeout(Some(SSH_BANNER_READ_TIMEOUT.min(remaining)))?;
                if read_valid_ssh_banner(&mut stream).unwrap_or(false) {
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(50).min(remaining));
        }
    }

    fn read_valid_ssh_banner(reader: &mut impl Read) -> io::Result<bool> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 128];
        while bytes.len() < SSH_BANNER_LIMIT {
            let maximum = (SSH_BANNER_LIMIT - bytes.len()).min(buffer.len());
            match reader.read(&mut buffer[..maximum]) {
                Ok(0) => return Ok(false),
                Ok(count) => {
                    bytes.extend_from_slice(&buffer[..count]);
                    while let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
                        let line = bytes.drain(..=newline).collect::<Vec<_>>();
                        if valid_ssh_identification(&line) {
                            return Ok(true);
                        }
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    fn valid_ssh_identification(line: &[u8]) -> bool {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        (line.starts_with(b"SSH-2.0-") || line.starts_with(b"SSH-1.99-"))
            && line.len() <= 255
            && line.iter().all(|byte| matches!(byte, b' '..=b'~'))
    }

    fn ocproxy_forwarding(port: u16) -> String {
        format!("{port}:{TARGET_HOST}:{TARGET_PORT}")
    }

    struct ScriptHelper {
        directory: PathBuf,
        path: PathBuf,
    }

    impl ScriptHelper {
        fn create(executor: &Path) -> io::Result<Self> {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            for counter in 0..256_u16 {
                let directory = Path::new("/tmp").join(format!(
                    "cz-vpn-{:x}-{nanos:x}-{counter:x}",
                    std::process::id()
                ));
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(&directory) {
                    Ok(()) => {
                        let path = directory.join("h");
                        let setup =
                            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                                .and_then(|()| std::os::unix::fs::symlink(executor, &path));
                        if let Err(error) = setup {
                            let _ = fs::remove_dir_all(&directory);
                            return Err(error);
                        }
                        return Ok(Self { directory, path });
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not create a private Tufts VPN helper directory",
            ))
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn remove(&self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_dir(&self.directory);
        }
    }

    impl Drop for ScriptHelper {
        fn drop(&mut self) {
            self.remove();
        }
    }

    fn safe_helper_argument(path: &Path) -> bool {
        path.is_absolute()
            && path
                .parent()
                .is_some_and(|parent| parent.parent() == Some(Path::new("/tmp")))
            && path.file_name() == Some(OsStr::new("h"))
            && path.parent().and_then(Path::file_name).is_some_and(|name| {
                let name = name.as_encoded_bytes();
                name.strip_prefix(b"cz-vpn-").is_some_and(|suffix| {
                    !suffix.is_empty()
                        && suffix
                            .iter()
                            .all(|byte| byte.is_ascii_hexdigit() || *byte == b'-')
                })
            })
    }

    fn validate_helper_path(path: &Path) -> io::Result<()> {
        if !safe_helper_argument(path) {
            return Err(invalid_input("Tufts VPN helper path is invalid"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| invalid_input("Tufts VPN helper parent is missing"))?;
        let parent_metadata = fs::symlink_metadata(parent)?;
        if !parent_metadata.is_dir() || parent_metadata.mode() & 0o777 != 0o700 {
            return Err(invalid_input("Tufts VPN helper directory is not private"));
        }
        let link_metadata = fs::symlink_metadata(path)?;
        if !link_metadata.file_type().is_symlink() {
            return Err(invalid_input("Tufts VPN helper is not a symlink"));
        }
        let target = fs::canonicalize(fs::read_link(path)?)?;
        if target != fs::canonicalize(std::env::current_exe()?)? {
            return Err(invalid_input("Tufts VPN helper target is invalid"));
        }
        Ok(())
    }

    fn append_bounded(transcript: &mut String, output: &str) {
        let combined = transcript.len().saturating_add(output.len());
        if combined > czi_ssh::SSH_CONSOLE_OUTPUT_LIMIT {
            transcript.drain(..combined - czi_ssh::SSH_CONSOLE_OUTPUT_LIMIT);
        }
        transcript.push_str(output);
    }

    fn append_fragment(output: &mut String, fragment: &str) {
        let remaining = czi_ssh::SSH_CONSOLE_OUTPUT_LIMIT.saturating_sub(output.len());
        output.push_str(&fragment[..fragment.len().min(remaining)]);
    }

    fn append_byte(output: &mut String, byte: u8) {
        if output.len() < czi_ssh::SSH_CONSOLE_OUTPUT_LIMIT {
            output.push(char::from(byte));
        }
    }

    fn invalid_input(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        #[test]
        fn username_validation_is_bounded_ascii_and_not_shell_syntax() {
            assert_eq!(VpnUsername::new("jdoe").unwrap().as_str(), "jdoe");
            assert!(VpnUsername::new("jane.doe@tufts.edu").is_ok());
            for invalid in ["", "two words", "bad;command", "bad\nname", "café"] {
                assert!(VpnUsername::new(invalid).is_err(), "accepted {invalid:?}");
            }
            assert!(VpnUsername::new("a".repeat(256)).is_err());
        }

        #[test]
        fn tool_selection_requires_one_complete_fixed_pair() {
            let arm = find_tools_with(|path| path.starts_with("/opt/homebrew"));
            assert_eq!(arm, Some(TOOL_PAIRS[0]));
            let intel = find_tools_with(|path| path.starts_with("/usr/local"));
            assert_eq!(intel, Some(TOOL_PAIRS[1]));
            assert!(find_tools_with(|path| path.ends_with("openconnect")).is_none());
            assert!(find_tools_with(|_| false).is_none());
        }

        #[test]
        fn openconnect_and_ocproxy_argv_are_exact_and_user_text_never_enters_script() {
            let username = VpnUsername::new("jdoe").unwrap();
            let helper = Path::new("/tmp/cz-vpn-1-ab-0/h");
            let executor = Path::new("/Applications/CZI Viewer.app/Contents/MacOS/czi-viewer");
            let argv = openconnect_argv(TOOL_PAIRS[0], &username, helper, executor);
            assert_eq!(
                argv,
                [
                    executor.as_os_str(),
                    OsStr::new(EXEC_MODE),
                    OsStr::new("/opt/homebrew/bin/openconnect"),
                    OsStr::new("--user"),
                    OsStr::new("jdoe"),
                    OsStr::new("--script-tun"),
                    OsStr::new("--script"),
                    helper.as_os_str(),
                    OsStr::new("--protocol=anyconnect"),
                    OsStr::new("https://vpn.tufts.edu/duo"),
                ]
            );
            validate_openconnect_exec_argv(TOOL_PAIRS[0], &argv[2..]).unwrap();
            assert!(!argv[7].to_string_lossy().contains(username.as_str()));
            assert_eq!(
                ocproxy_forwarding(41_337),
                "41337:login-prod.pax.tufts.edu:22"
            );
        }

        #[test]
        fn openconnect_executor_rejects_changed_fixed_arguments() {
            let username = VpnUsername::new("jdoe").unwrap();
            let helper = Path::new("/tmp/cz-vpn-1-ab-0/h");
            let executor = Path::new("/tmp/czi-viewer");
            let argv = openconnect_argv(TOOL_PAIRS[0], &username, helper, executor);
            let mut actual = argv[2..].to_vec();
            actual[4] = "--script=/tmp/user-text".into();
            assert!(validate_openconnect_exec_argv(TOOL_PAIRS[0], &actual).is_err());
            let mut actual = argv[2..].to_vec();
            actual[7] = "https://example.test/".into();
            assert!(validate_openconnect_exec_argv(TOOL_PAIRS[0], &actual).is_err());
        }

        #[test]
        fn fake_readiness_accepts_only_a_bounded_ssh_banner_while_child_is_live() {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .write_all(b"notice\r\nSSH-2.0-test-server\r\n")
                    .unwrap();
            });
            wait_for_ssh_banner(port, Duration::from_secs(2), || Ok(true)).unwrap();
            server.join().unwrap();

            assert!(!valid_ssh_identification(b"HTTP/1.1 200 OK\r\n"));
            assert!(!valid_ssh_identification(b"SSH-3.0-future\r\n"));
            assert!(!valid_ssh_identification(&vec![b'a'; 256]));
        }

        #[test]
        fn readiness_reports_child_exit_and_cancellation_without_prompt_parsing() {
            let live = Arc::new(AtomicBool::new(true));
            let observed = Arc::clone(&live);
            let cancellation = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                live.store(false, Ordering::Release);
            });
            let error = wait_for_ssh_banner(9, Duration::from_secs(1), move || {
                Ok(observed.load(Ordering::Acquire))
            })
            .unwrap_err();
            cancellation.join().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }

        #[test]
        fn vpn_console_pump_clears_transcript_and_never_echoes_input() {
            let executable = Path::new("/bin/sleep");
            let argv = vec![executable.as_os_str().to_os_string(), OsString::from("30")];
            let czi_ssh_darwin::SpawnedTerminal {
                pty_master,
                child,
                cancellation,
            } = czi_ssh_darwin::spawn_terminal(executable, &argv, &[])
                .expect("spawn VPN console test child");
            let console = VpnConsole {
                master: pty_master,
                transcript: String::from("retained-vpn-authentication"),
                latest_output: String::from("retained-latest-output"),
            };
            let pump = crate::ConsolePump::spawn(crate::AuthenticationConsole::Vpn(console))
                .expect("spawn VPN console pump");
            let deadline = Instant::now() + Duration::from_secs(2);
            while !pump
                .snapshot()
                .transcript
                .contains("retained-vpn-authentication")
            {
                assert!(
                    Instant::now() < deadline,
                    "VPN transcript was not published"
                );
                std::thread::sleep(Duration::from_millis(10));
            }

            pump.clear_transcript();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !pump.snapshot().transcript.is_empty() {
                assert!(Instant::now() < deadline, "VPN transcript was not cleared");
                std::thread::sleep(Duration::from_millis(10));
            }
            pump.try_send_input(b"opaque-vpn-input".to_vec())
                .expect("send immediate terminal input");
            std::thread::sleep(Duration::from_millis(30));
            assert!(pump.snapshot().transcript.is_empty());

            drop(pump);
            cancellation
                .cancel()
                .expect("cancel VPN console test child");
            drop(child);
        }

        #[test]
        fn private_helper_path_is_shell_safe_and_removed_on_drop() {
            let executable = std::env::current_exe().unwrap();
            let helper = ScriptHelper::create(&executable).unwrap();
            assert!(safe_helper_argument(helper.path()));
            assert!(
                helper
                    .path()
                    .as_os_str()
                    .as_encoded_bytes()
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-'))
            );
            let directory = helper.directory.clone();
            let path = helper.path.clone();
            drop(helper);
            assert!(!path.exists());
            assert!(!directory.exists());
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(target_os = "macos"))]
/// Return without dispatching because the Tufts VPN integration is macOS-only.
///
/// # Errors
///
/// This platform stub does not return an error.
pub fn run_executor_if_requested() -> std::io::Result<bool> {
    Ok(false)
}
