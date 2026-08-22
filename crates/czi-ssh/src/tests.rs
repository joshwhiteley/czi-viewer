use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

use czi_core::{RandomAccessSource, SourceError};

#[cfg(unix)]
use crate::command::SOCKET_PATH_LIMIT;
#[cfg(unix)]
use crate::{
    BridgeCancellation, BridgeListener, authenticate_bridge_client, authenticate_bridge_server,
    connect_bridge_socket,
};
use crate::{
    ControlPath, OPENSSH_PATH, OpenSshConfig, OpenSshConfigError, SftpError, SftpLocation,
    SftpLocationError, SftpProtocolError, SftpSession, SftpSource, SharedSftpSession, SshProfile,
    SshProfileError,
};

use super::{
    Cursor, SSH_FILEXFER_ATTR_ACMODTIME, SSH_FILEXFER_ATTR_EXTENDED, SSH_FILEXFER_ATTR_PERMISSIONS,
    SSH_FILEXFER_ATTR_SIZE, SSH_FILEXFER_ATTR_UIDGID, SSH_FX_EOF, SSH_FX_OK, SSH_FXF_READ,
    SSH_FXP_ATTRS, SSH_FXP_CLOSE, SSH_FXP_DATA, SSH_FXP_FSTAT, SSH_FXP_HANDLE, SSH_FXP_INIT,
    SSH_FXP_NAME, SSH_FXP_OPEN, SSH_FXP_OPENDIR, SSH_FXP_READ, SSH_FXP_READDIR, SSH_FXP_REALPATH,
    SSH_FXP_STATUS, SSH_FXP_VERSION, parse_attributes,
};

struct FakeSsh {
    stream: TcpStream,
}

impl FakeSsh {
    fn read_request(&mut self) -> Request {
        let mut header = [0_u8; 4];
        self.stream.read_exact(&mut header).unwrap();
        let length = usize::try_from(u32::from_be_bytes(header)).unwrap();
        assert!(length > 0);
        let mut frame = vec![0_u8; length];
        self.stream.read_exact(&mut frame).unwrap();
        Request {
            packet_type: frame[0],
            payload: frame[1..].to_vec(),
        }
    }

    fn expect_init(&mut self) {
        assert_eq!(
            self.read_request(),
            Request {
                packet_type: SSH_FXP_INIT,
                payload: 3_u32.to_be_bytes().to_vec(),
            }
        );
    }

    fn send(&mut self, packet_type: u8, payload: &[u8]) {
        let length = u32::try_from(payload.len() + 1).unwrap();
        self.stream.write_all(&length.to_be_bytes()).unwrap();
        self.stream.write_all(&[packet_type]).unwrap();
        self.stream.write_all(payload).unwrap();
        self.stream.flush().unwrap();
    }

    fn send_fragmented(&mut self, packet_type: u8, payload: &[u8]) {
        let length = u32::try_from(payload.len() + 1).unwrap();
        let mut frame = length.to_be_bytes().to_vec();
        frame.push(packet_type);
        frame.extend_from_slice(payload);
        for byte in frame {
            self.stream.write_all(&[byte]).unwrap();
            self.stream.flush().unwrap();
        }
    }

    fn send_status(&mut self, request_id: u32, code: u32) {
        let mut payload = request_id.to_be_bytes().to_vec();
        payload.extend_from_slice(&code.to_be_bytes());
        push_string(&mut payload, b"status");
        push_string(&mut payload, b"en");
        self.send(SSH_FXP_STATUS, &payload);
    }

    fn send_handle(&mut self, request_id: u32, handle: &[u8]) {
        let mut payload = request_id.to_be_bytes().to_vec();
        push_string(&mut payload, handle);
        self.send(SSH_FXP_HANDLE, &payload);
    }

    fn send_data(&mut self, request_id: u32, data: &[u8]) {
        let mut payload = request_id.to_be_bytes().to_vec();
        push_string(&mut payload, data);
        self.send(SSH_FXP_DATA, &payload);
    }

    fn send_version(&mut self, fragmented: bool) {
        let payload = 3_u32.to_be_bytes();
        if fragmented {
            self.send_fragmented(SSH_FXP_VERSION, &payload);
        } else {
            self.send(SSH_FXP_VERSION, &payload);
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Request {
    packet_type: u8,
    payload: Vec<u8>,
}

fn fake_session(
    script: impl FnOnce(&mut FakeSsh) + Send + 'static,
) -> (SftpSession, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        started_tx.send(()).unwrap();
        script(&mut FakeSsh { stream });
    });
    let client = TcpStream::connect(address).unwrap();
    started_rx.recv().unwrap();
    let session = SftpSession::with_test_transport(client).unwrap();
    (session, worker)
}

fn location(value: &str) -> SftpLocation {
    SftpLocation::new(value).unwrap()
}

fn profile(value: &str) -> SshProfile {
    SshProfile::new(value).unwrap()
}

fn push_string(payload: &mut Vec<u8>, value: &[u8]) {
    payload.extend_from_slice(&u32::try_from(value.len()).unwrap().to_be_bytes());
    payload.extend_from_slice(value);
}

fn request_id(request: &Request) -> u32 {
    u32::from_be_bytes(request.payload[..4].try_into().unwrap())
}

fn request_string(request: &Request, offset: &mut usize) -> Vec<u8> {
    let length = u32::from_be_bytes(request.payload[*offset..*offset + 4].try_into().unwrap());
    *offset += 4;
    let length = usize::try_from(length).unwrap();
    let value = request.payload[*offset..*offset + length].to_vec();
    *offset += length;
    value
}

fn attrs(size: u64, mtime: u32) -> Vec<u8> {
    let mut payload = (SSH_FILEXFER_ATTR_SIZE | SSH_FILEXFER_ATTR_ACMODTIME)
        .to_be_bytes()
        .to_vec();
    payload.extend_from_slice(&size.to_be_bytes());
    payload.extend_from_slice(&123_u32.to_be_bytes());
    payload.extend_from_slice(&mtime.to_be_bytes());
    payload
}

fn attrs_response(request_id: u32, attributes: &[u8]) -> Vec<u8> {
    let mut payload = request_id.to_be_bytes().to_vec();
    payload.extend_from_slice(attributes);
    payload
}

fn send_name(fake: &mut FakeSsh, request_id: u32, entries: &[(&str, &str, Vec<u8>)]) {
    let mut payload = request_id.to_be_bytes().to_vec();
    payload.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_be_bytes());
    for (path, long_name, attributes) in entries {
        push_string(&mut payload, path.as_bytes());
        push_string(&mut payload, long_name.as_bytes());
        payload.extend_from_slice(attributes);
    }
    fake.send(SSH_FXP_NAME, &payload);
}

fn open_source(
    session: SftpSession,
    requested_path: &SftpLocation,
) -> Result<SftpSource, SftpError> {
    SftpSource::open_with_test_session(session, requested_path)
}

#[test]
fn validates_profiles_and_locations() {
    assert_eq!(SshProfile::new(""), Err(SshProfileError::Empty));
    assert_eq!(
        SshProfile::new("-oProxyCommand=bad"),
        Err(SshProfileError::LeadingDash)
    );
    assert_eq!(
        SshProfile::new("host\0name"),
        Err(SshProfileError::ContainsNul)
    );
    assert!(matches!(
        SshProfile::new("a".repeat(256)),
        Err(SshProfileError::TooLong { length: 256 })
    ));
    assert_eq!(SftpLocation::new(""), Err(SftpLocationError::Empty));
    assert_eq!(
        SftpLocation::new("bad\0path"),
        Err(SftpLocationError::ContainsNul)
    );
    assert!(matches!(
        SftpLocation::new("a".repeat(4097)),
        Err(SftpLocationError::TooLong { length: 4097 })
    ));
    assert_eq!(profile("alice@example.test").as_str(), "alice@example.test");
    assert_eq!(location("/data/image.czi").as_str(), "/data/image.czi");
}

#[test]
#[allow(clippy::too_many_lines)]
fn command_builders_keep_paths_out_of_argv_and_preserve_host_checks() {
    #[cfg(unix)]
    let base = std::path::Path::new("/tmp").join(format!("czi-ssh-test-{}", std::process::id()));
    #[cfg(not(unix))]
    let base = std::env::temp_dir().join(format!("czi-ssh-test-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let private = base.join("socket's directory");
    std::fs::create_dir(&private).unwrap();
    let control = ControlPath::from_private_directory(&private).unwrap();
    let config = OpenSshConfig::new().with_control_path(control);
    let destination = profile("alice@example.test");
    let remote = location("/data/; not-an-argv.czi");
    let argv = config.sftp_argv(&destination);
    let argv = argv
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(argv[0], OPENSSH_PATH);
    assert!(argv.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "ForwardAgent=no"])
    );
    assert!(argv.windows(2).any(|pair| pair == ["-o", "ForwardX11=no"]));
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "ClearAllForwardings=yes"])
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "PermitLocalCommand=no"])
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "StrictHostKeyChecking=yes"])
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "ConnectTimeout=15"])
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "ServerAliveInterval=30"])
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "ServerAliveCountMax=3"])
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "NumberOfPasswordPrompts=0"])
    );
    assert!(argv.windows(2).any(|pair| pair == ["-T", "-s"]));
    assert_eq!(argv.last(), Some(&"sftp".to_owned()));
    assert!(!argv.iter().any(|argument| argument == remote.as_str()));
    assert!(
        !argv
            .iter()
            .any(|argument| argument.contains("HostKeyChecking=no"))
    );
    assert!(
        !argv
            .iter()
            .any(|argument| argument.contains("HostKeyChecking=accept-new"))
    );
    assert!(
        !argv
            .iter()
            .any(|argument| argument.starts_with("UserKnownHostsFile="))
    );
    assert!(
        !argv
            .iter()
            .any(|argument| argument.starts_with("GlobalKnownHostsFile="))
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "ControlMaster=no"])
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-o", "ControlPath=none"])
    );

    let interactive = OpenSshConfig::interactive_sftp_argv(&destination)
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        interactive
            .windows(2)
            .any(|pair| pair == ["-o", "BatchMode=no"])
    );
    assert!(
        interactive
            .windows(2)
            .any(|pair| pair == ["-o", "StrictHostKeyChecking=ask"])
    );
    assert!(interactive.windows(2).any(|pair| pair == ["-T", "-s"]));
    assert!(
        interactive
            .windows(2)
            .any(|pair| pair == ["-o", "ControlMaster=no"])
    );
    assert!(
        interactive
            .windows(2)
            .any(|pair| pair == ["-o", "ControlPath=none"])
    );
    let embedded = OpenSshConfig::embedded_sftp_argv(&destination)
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        embedded,
        [
            OPENSSH_PATH,
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
            "alice@example.test",
            "sftp",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>()
    );

    let bridge = config
        .terminal_bridge_command(
            std::path::Path::new("/tmp/czi viewer"),
            "--czi-sftp-bridge",
            &destination,
        )
        .unwrap();
    assert!(bridge.contains("'/tmp/czi viewer'"));
    assert!(bridge.contains("'--czi-sftp-bridge'"));
    assert!(bridge.contains("'alice@example.test'"));
    assert!(bridge.contains("'\"'\"'"));
    assert!(!bridge.contains(remote.as_str()));
    assert!(!bridge.contains("ControlMaster"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(unix)]
#[test]
fn create_private_uses_a_short_socket_without_changing_tmpdir() {
    let simulated_long_tmpdir = std::path::PathBuf::from("/var/folders").join("x".repeat(90));
    let socket_if_tmpdir_were_used = simulated_long_tmpdir.join("cz-1234-abcdef-0").join("s");
    assert!(
        socket_if_tmpdir_were_used.as_os_str().len() > SOCKET_PATH_LIMIT,
        "the regression test needs a simulated TMPDIR that exceeds the safe limit"
    );

    let control_path = ControlPath::create_private().expect("private control path");
    assert_eq!(
        control_path.directory().parent(),
        Some(std::path::Path::new("/tmp"))
    );
    assert_eq!(
        control_path.socket_path().file_name(),
        Some(std::ffi::OsStr::new("s"))
    );
    assert!(control_path.socket_path().as_os_str().len() <= SOCKET_PATH_LIMIT);

    let directory = control_path.directory().to_path_buf();
    drop(control_path);
    std::fs::remove_dir_all(directory).expect("remove private control path");
}

#[cfg(unix)]
#[test]
fn bridge_socket_requires_a_private_path_and_never_clobbers_an_existing_file() {
    let control_path = ControlPath::create_private().expect("private control path");
    let directory = control_path.directory().to_path_buf();
    let socket_path = control_path.socket_path().to_path_buf();
    std::fs::write(&socket_path, b"do not replace").expect("sentinel socket path");
    assert!(matches!(
        BridgeListener::bind(&socket_path),
        Err(SftpError::InvalidConfig(
            OpenSshConfigError::BridgeSocketAlreadyExists
        ))
    ));
    assert_eq!(std::fs::read(&socket_path).unwrap(), b"do not replace");
    assert!(matches!(
        BridgeListener::bind("/tmp/not-a-czi-bridge/s"),
        Err(SftpError::InvalidConfig(
            OpenSshConfigError::BridgeSocketPathInvalid
        ))
    ));
    std::fs::remove_dir_all(directory).expect("remove private control path");
}

#[cfg(unix)]
#[test]
fn bridge_socket_lifecycle_and_unix_sftp_protocol_are_strict() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let control_path = ControlPath::create_private().expect("private control path");
    let directory = control_path.directory().to_path_buf();
    let socket_path = control_path.socket_path().to_path_buf();
    let listener = BridgeListener::bind(&socket_path).expect("bridge listener");
    let metadata = std::fs::symlink_metadata(&socket_path).expect("bridge socket metadata");
    assert!(metadata.file_type().is_socket());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

    let config = OpenSshConfig::new().with_control_path(control_path);
    let worker = thread::spawn(move || {
        let mut stream = listener
            .accept()
            .expect("bridge accept")
            .expect("bridge viewer connection");
        authenticate_bridge_server(&mut stream, &profile("bridge-host")).expect("bridge handshake");
        let mut header = [0_u8; 4];
        stream.read_exact(&mut header).expect("SFTP header");
        let length = usize::try_from(u32::from_be_bytes(header)).expect("SFTP length");
        let mut frame = vec![0_u8; length];
        stream.read_exact(&mut frame).expect("SFTP init");
        assert_eq!(frame, [SSH_FXP_INIT, 0, 0, 0, 3]);
        let response = [SSH_FXP_VERSION, 0, 0, 0, 3];
        stream
            .write_all(&u32::try_from(response.len()).unwrap().to_be_bytes())
            .expect("SFTP version length");
        stream.write_all(&response).expect("SFTP version");
        stream.flush().expect("SFTP version flush");
    });

    let session = SftpSession::connect_preferred(&profile("bridge-host"), &config)
        .expect("SFTP session over bridge socket");
    assert!(
        !socket_path.exists(),
        "bridge listener path is removed after accept"
    );
    drop(session);
    worker.join().expect("bridge worker");
    assert!(
        connect_bridge_socket(config.control_path().unwrap())
            .expect("missing bridge is not an error")
            .is_none()
    );
    drop(config);
    std::fs::remove_dir_all(directory).expect("remove private control path");
}

#[cfg(unix)]
#[test]
fn bridge_handshake_rejects_a_different_profile_before_sftp() {
    use std::os::unix::net::UnixStream;

    let (mut client, mut server) = UnixStream::pair().expect("Unix stream pair");
    let worker = thread::spawn(move || {
        assert!(authenticate_bridge_server(&mut server, &profile("profile-a")).is_err());
    });
    assert!(authenticate_bridge_client(&mut client, &profile("profile-b")).is_err());
    worker.join().expect("bridge handshake worker");
}

#[cfg(unix)]
#[test]
fn bridge_cancellation_closes_concurrent_dataset_and_browse_streams() {
    use std::os::unix::net::UnixStream;

    let (dataset, mut dataset_peer) = UnixStream::pair().expect("dataset Unix stream pair");
    let (browse, mut browse_peer) = UnixStream::pair().expect("browse Unix stream pair");
    let cancellation = BridgeCancellation::default();
    let _dataset_registration = cancellation
        .register(&dataset)
        .expect("register dataset bridge stream");
    let _browse_registration = cancellation
        .register(&browse)
        .expect("register browse bridge stream");
    cancellation.cancel();
    assert_bridge_peer_closed(&mut dataset_peer);
    assert_bridge_peer_closed(&mut browse_peer);
}

#[cfg(unix)]
#[test]
fn stale_bridge_registration_does_not_unregister_a_newer_stream() {
    use std::os::unix::net::UnixStream;

    let (stale_stream, _) = UnixStream::pair().expect("stale Unix stream pair");
    let (active_stream, mut active_peer) = UnixStream::pair().expect("active Unix stream pair");
    let cancellation = BridgeCancellation::default();
    let stale_registration = cancellation
        .register(&stale_stream)
        .expect("register stale bridge stream");
    let _active_registration = cancellation
        .register(&active_stream)
        .expect("register active bridge stream");
    drop(stale_registration);

    cancellation.cancel();
    assert_bridge_peer_closed(&mut active_peer);
}

#[cfg(unix)]
#[test]
fn bridge_cancellation_is_sticky_before_late_registration() {
    use std::os::unix::net::UnixStream;

    let (stream, mut peer) = UnixStream::pair().expect("Unix stream pair");
    let cancellation = BridgeCancellation::default();
    cancellation.cancel();
    assert!(cancellation.register(&stream).is_err());
    assert_bridge_peer_closed(&mut peer);
}

#[cfg(unix)]
#[test]
fn bridge_listener_exits_when_viewer_removes_its_private_directory() {
    let control_path = ControlPath::create_private().expect("private control path");
    let directory = control_path.directory().to_path_buf();
    let listener = BridgeListener::bind(control_path.socket_path()).expect("bridge listener");
    std::fs::remove_dir_all(&directory).expect("remove private control directory");
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        result_tx
            .send(listener.accept())
            .expect("send idle listener result");
    });
    assert!(
        result_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("idle listener result")
            .expect("idle listener should inspect its removed path")
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn bridge_listener_does_not_unlink_a_replacement_socket() {
    use std::os::unix::net::UnixListener;

    let control_path = ControlPath::create_private().expect("private control path");
    let directory = control_path.directory().to_path_buf();
    let socket_path = control_path.socket_path().to_path_buf();
    let listener = BridgeListener::bind(&socket_path).expect("bridge listener");
    std::fs::remove_file(&socket_path).expect("remove original bridge socket");
    let replacement = UnixListener::bind(&socket_path).expect("replacement bridge socket");

    assert!(
        listener
            .accept()
            .expect("inspect replacement bridge socket")
            .is_none()
    );
    assert!(socket_path.exists(), "replacement socket must be preserved");

    drop(replacement);
    std::fs::remove_file(&socket_path).expect("remove replacement bridge socket");
    std::fs::remove_dir_all(directory).expect("remove private control directory");
}

#[cfg(unix)]
fn assert_bridge_peer_closed(peer: &mut std::os::unix::net::UnixStream) {
    peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .expect("set bridge peer timeout");
    let mut byte = [0_u8; 1];
    assert_eq!(peer.read(&mut byte).expect("bridge peer EOF"), 0);
}

#[test]
fn bridge_command_requires_a_control_path() {
    let error = OpenSshConfig::new()
        .terminal_bridge_command(
            std::path::Path::new("/tmp/czi-viewer"),
            "--czi-sftp-bridge",
            &profile("host"),
        )
        .unwrap_err();
    assert_eq!(error, OpenSshConfigError::MissingControlPath);
}

#[test]
fn packet_vectors_and_fragmented_frames_negotiate_v3() {
    let (session, worker) = fake_session(|fake| {
        fake.expect_init();
        fake.send_version(true);
    });
    drop(session);
    worker.join().unwrap();
}

#[test]
fn rejects_malformed_packet_length_before_allocation() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut init = [0_u8; 9];
        stream.read_exact(&mut init).unwrap();
        stream.write_all(&0_u32.to_be_bytes()).unwrap();
        stream.flush().unwrap();
    });
    let stream = TcpStream::connect(address).unwrap();
    let Err(error) = SftpSession::with_test_transport(stream) else {
        panic!("zero packet must be rejected");
    };
    assert!(matches!(
        error,
        SftpError::Protocol(SftpProtocolError::InvalidPacketLength { length: 0 })
    ));
    worker.join().unwrap();
}

#[test]
fn parses_full_v3_attributes_and_rejects_malformed_attributes() {
    let mut bytes = (SSH_FILEXFER_ATTR_SIZE
        | SSH_FILEXFER_ATTR_UIDGID
        | SSH_FILEXFER_ATTR_PERMISSIONS
        | SSH_FILEXFER_ATTR_ACMODTIME
        | SSH_FILEXFER_ATTR_EXTENDED)
        .to_be_bytes()
        .to_vec();
    bytes.extend_from_slice(&9_u64.to_be_bytes());
    bytes.extend_from_slice(&501_u32.to_be_bytes());
    bytes.extend_from_slice(&20_u32.to_be_bytes());
    bytes.extend_from_slice(&0o100_644_u32.to_be_bytes());
    bytes.extend_from_slice(&11_u32.to_be_bytes());
    bytes.extend_from_slice(&12_u32.to_be_bytes());
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    push_string(&mut bytes, b"vendor@example.test");
    push_string(&mut bytes, b"value");
    let mut cursor = Cursor::new(&bytes);
    let decoded = parse_attributes(&mut cursor).unwrap();
    cursor.finish("test").unwrap();
    assert_eq!(decoded.size, Some(9));
    assert_eq!(decoded.uid_gid, Some((501, 20)));
    assert_eq!(decoded.permissions, Some(0o100_644));
    assert_eq!(decoded.access_modify_time, Some((11, 12)));
    assert_eq!(decoded.extended[0].name, b"vendor@example.test");
    assert_eq!(decoded.extended[0].value, b"value");

    let unknown_flags = 0x10_u32.to_be_bytes();
    let mut unknown = Cursor::new(&unknown_flags);
    assert!(matches!(
        parse_attributes(&mut unknown),
        Err(SftpError::Protocol(
            SftpProtocolError::UnknownAttributeFlags { .. }
        ))
    ));
    let mut trailing = attrs(1, 2);
    trailing.push(1);
    let mut cursor = Cursor::new(&trailing);
    parse_attributes(&mut cursor).unwrap();
    assert!(matches!(
        cursor.finish("ATTRS"),
        Err(SftpProtocolError::TrailingData { .. })
    ));
}

#[test]
fn open_uses_read_flag_and_empty_attributes_only() {
    let requested = location("/canonical/image.czi");
    let expected_path = requested.clone();
    let (mut session, worker) = fake_session(move |fake| {
        fake.expect_init();
        fake.send_version(false);
        let request = fake.read_request();
        assert_eq!(request.packet_type, SSH_FXP_OPEN);
        assert_eq!(request_id(&request), 1);
        let mut offset = 4;
        assert_eq!(
            request_string(&request, &mut offset),
            expected_path.as_str().as_bytes()
        );
        assert_eq!(
            u32::from_be_bytes(request.payload[offset..offset + 4].try_into().unwrap()),
            SSH_FXF_READ
        );
        assert_eq!(
            u32::from_be_bytes(request.payload[offset + 4..offset + 8].try_into().unwrap()),
            0
        );
        assert_eq!(offset + 8, request.payload.len());
        fake.send_handle(1, b"read-handle");
    });
    assert_eq!(session.open_read(&requested).unwrap(), b"read-handle");
    drop(session);
    worker.join().unwrap();
}

#[test]
fn source_reads_short_data_and_closes_through_fake_ssh() {
    let requested = location("/input/image.czi");
    let canonical = "/canonical/image.czi";
    let requested_for_server = requested.clone();
    let (session, worker) = fake_session(move |fake| {
        fake.expect_init();
        fake.send_version(true);
        let realpath = fake.read_request();
        assert_eq!(realpath.packet_type, SSH_FXP_REALPATH);
        assert_eq!(request_id(&realpath), 1);
        let mut offset = 4;
        assert_eq!(
            request_string(&realpath, &mut offset),
            requested_for_server.as_str().as_bytes()
        );
        send_name(
            fake,
            1,
            &[(canonical, canonical, 0_u32.to_be_bytes().to_vec())],
        );
        let open = fake.read_request();
        assert_eq!(open.packet_type, SSH_FXP_OPEN);
        assert_eq!(request_id(&open), 2);
        fake.send_handle(2, b"file");
        let fstat = fake.read_request();
        assert_eq!(fstat.packet_type, SSH_FXP_FSTAT);
        fake.send(SSH_FXP_ATTRS, &attrs_response(3, &attrs(8, 77)));
        let first = fake.read_request();
        assert_eq!(first.packet_type, SSH_FXP_READ);
        assert_eq!(request_id(&first), 4);
        fake.send_data(4, b"abc");
        let second = fake.read_request();
        assert_eq!(second.packet_type, SSH_FXP_READ);
        assert_eq!(request_id(&second), 5);
        let mut read_offset = 4;
        assert_eq!(request_string(&second, &mut read_offset), b"file");
        assert_eq!(
            u64::from_be_bytes(
                second.payload[read_offset..read_offset + 8]
                    .try_into()
                    .unwrap()
            ),
            3
        );
        assert_eq!(
            u32::from_be_bytes(
                second.payload[read_offset + 8..read_offset + 12]
                    .try_into()
                    .unwrap()
            ),
            5
        );
        fake.send_data(5, b"defgh");
        let close = fake.read_request();
        assert_eq!(close.packet_type, SSH_FXP_CLOSE);
        assert_eq!(request_id(&close), 6);
        fake.send_status(6, SSH_FX_OK);
    });
    let source = open_source(session, &requested).unwrap();
    assert_eq!(source.canonical_path().as_str(), canonical);
    assert_eq!(source.info().length, 8);
    let mut bytes = [0_u8; 8];
    source.read_at(0, &mut bytes).unwrap();
    assert_eq!(&bytes, b"abcdefgh");
    source.close().unwrap();
    worker.join().unwrap();
}

#[test]
fn shared_session_reuses_one_init_across_browse_open_and_browse() {
    let requested = location("/input/image.czi");
    let (session, worker) = fake_session(|fake| {
        fake.expect_init();
        fake.send_version(false);

        let home = fake.read_request();
        assert_eq!(home.packet_type, SSH_FXP_REALPATH);
        assert_eq!(request_id(&home), 1);
        send_name(
            fake,
            1,
            &[("/home/test", "/home/test", 0_u32.to_be_bytes().to_vec())],
        );

        let realpath = fake.read_request();
        assert_eq!(realpath.packet_type, SSH_FXP_REALPATH);
        assert_eq!(request_id(&realpath), 2);
        send_name(
            fake,
            2,
            &[(
                "/canonical/image.czi",
                "image.czi",
                0_u32.to_be_bytes().to_vec(),
            )],
        );
        let open = fake.read_request();
        assert_eq!(open.packet_type, SSH_FXP_OPEN);
        assert_eq!(request_id(&open), 3);
        fake.send_handle(3, b"file");
        let fstat = fake.read_request();
        assert_eq!(fstat.packet_type, SSH_FXP_FSTAT);
        assert_eq!(request_id(&fstat), 4);
        fake.send(SSH_FXP_ATTRS, &attrs_response(4, &attrs(8, 77)));

        let opendir = fake.read_request();
        assert_eq!(opendir.packet_type, SSH_FXP_OPENDIR);
        assert_eq!(request_id(&opendir), 5);
        fake.send_handle(5, b"directory");
        let readdir = fake.read_request();
        assert_eq!(readdir.packet_type, SSH_FXP_READDIR);
        assert_eq!(request_id(&readdir), 6);
        fake.send_status(6, SSH_FX_EOF);
        let close_directory = fake.read_request();
        assert_eq!(close_directory.packet_type, SSH_FXP_CLOSE);
        assert_eq!(request_id(&close_directory), 7);
        fake.send_status(7, SSH_FX_OK);

        let close_file = fake.read_request();
        assert_eq!(close_file.packet_type, SSH_FXP_CLOSE);
        assert_eq!(request_id(&close_file), 8);
        fake.send_status(8, SSH_FX_OK);
    });
    let shared = SharedSftpSession::new(session);
    let home = location(".");
    assert_eq!(
        shared
            .with_session(|session| session.realpath(&home))
            .unwrap()
            .as_str(),
        "/home/test"
    );
    let source = SftpSource::open_with_shared_session(shared.clone(), &requested).unwrap();
    assert!(
        shared
            .with_session(|session| session.read_dir(&location("/home/test")))
            .unwrap()
            .is_empty()
    );
    source.close().unwrap();
    drop(shared);
    worker.join().unwrap();
}

#[test]
fn dropping_source_clone_keeps_shared_browser_session_alive() {
    let requested = location("/input/image.czi");
    let (session, worker) = fake_session(|fake| {
        fake.expect_init();
        fake.send_version(false);
        let realpath = fake.read_request();
        send_name(
            fake,
            request_id(&realpath),
            &[(
                "/canonical/image.czi",
                "image.czi",
                0_u32.to_be_bytes().to_vec(),
            )],
        );
        let open = fake.read_request();
        fake.send_handle(request_id(&open), b"file");
        let fstat = fake.read_request();
        fake.send(
            SSH_FXP_ATTRS,
            &attrs_response(request_id(&fstat), &attrs(8, 77)),
        );
        let close = fake.read_request();
        assert_eq!(close.packet_type, SSH_FXP_CLOSE);
        fake.send_status(request_id(&close), SSH_FX_OK);
        let browse = fake.read_request();
        assert_eq!(browse.packet_type, SSH_FXP_REALPATH);
        send_name(
            fake,
            request_id(&browse),
            &[("/home/test", "/home/test", 0_u32.to_be_bytes().to_vec())],
        );
        let mut eof = [0_u8; 1];
        assert_eq!(fake.stream.read(&mut eof).unwrap(), 0);
    });
    let shared = SharedSftpSession::new(session);
    let source = SftpSource::open_with_shared_session(shared.clone(), &requested).unwrap();
    drop(source);
    assert_eq!(
        shared
            .with_session(|session| session.realpath(&location(".")))
            .unwrap()
            .as_str(),
        "/home/test"
    );
    drop(shared);
    worker.join().unwrap();
}

#[test]
fn contended_source_drop_defers_close_until_the_session_unlocks() {
    let requested = location("/input/image.czi");
    let (browse_started_tx, browse_started_rx) = mpsc::channel();
    let (release_browse_tx, release_browse_rx) = mpsc::channel();
    let (session, worker) = fake_session(move |fake| {
        fake.expect_init();
        fake.send_version(false);
        let realpath = fake.read_request();
        send_name(
            fake,
            request_id(&realpath),
            &[(
                "/canonical/image.czi",
                "image.czi",
                0_u32.to_be_bytes().to_vec(),
            )],
        );
        let open = fake.read_request();
        fake.send_handle(request_id(&open), b"file");
        let fstat = fake.read_request();
        fake.send(
            SSH_FXP_ATTRS,
            &attrs_response(request_id(&fstat), &attrs(8, 77)),
        );
        let browse = fake.read_request();
        assert_eq!(browse.packet_type, SSH_FXP_REALPATH);
        browse_started_tx.send(()).expect("report blocked browser");
        release_browse_rx.recv().expect("release blocked browser");
        send_name(
            fake,
            request_id(&browse),
            &[("/home/test", "/home/test", 0_u32.to_be_bytes().to_vec())],
        );
        let close = fake.read_request();
        assert_eq!(close.packet_type, SSH_FXP_CLOSE);
        fake.send_status(request_id(&close), SSH_FX_OK);
        let mut eof = [0_u8; 1];
        assert_eq!(fake.stream.read(&mut eof).unwrap(), 0);
    });
    let shared = SharedSftpSession::new(session);
    let source = SftpSource::open_with_shared_session(shared.clone(), &requested).unwrap();
    let browser_session = shared.clone();
    let browser = thread::spawn(move || {
        browser_session
            .with_session(|session| session.realpath(&location(".")))
            .expect("complete browser operation")
    });
    browse_started_rx
        .recv()
        .expect("browser holds session lock");
    drop(source);
    release_browse_tx.send(()).expect("release browser");
    assert_eq!(browser.join().expect("join browser").as_str(), "/home/test");
    drop(shared);
    worker.join().unwrap();
}

#[test]
fn final_shared_owner_drains_a_deferred_source_close() {
    let requested = location("/input/image.czi");
    let (session, worker) = fake_session(|fake| {
        fake.expect_init();
        fake.send_version(false);
        let realpath = fake.read_request();
        send_name(
            fake,
            request_id(&realpath),
            &[(
                "/canonical/image.czi",
                "image.czi",
                0_u32.to_be_bytes().to_vec(),
            )],
        );
        let open = fake.read_request();
        fake.send_handle(request_id(&open), b"file");
        let fstat = fake.read_request();
        fake.send(
            SSH_FXP_ATTRS,
            &attrs_response(request_id(&fstat), &attrs(8, 77)),
        );
        let close = fake.read_request();
        assert_eq!(close.packet_type, SSH_FXP_CLOSE);
        fake.send_status(request_id(&close), SSH_FX_OK);
    });
    let shared = SharedSftpSession::new(session);
    let source = SftpSource::open_with_shared_session(shared.clone(), &requested).unwrap();
    drop(source);
    drop(shared);
    worker.join().unwrap();
}

#[test]
fn source_uses_u64_read_offsets_above_four_gibibytes() {
    let requested = location("/input/large.czi");
    let canonical = "/canonical/large.czi";
    let offset = 0x1_0000_0010_u64;
    let size = offset + 4;
    let (session, worker) = fake_session(move |fake| {
        fake.expect_init();
        fake.send_version(false);
        let realpath = fake.read_request();
        send_name(
            fake,
            request_id(&realpath),
            &[(canonical, canonical, 0_u32.to_be_bytes().to_vec())],
        );
        let open = fake.read_request();
        fake.send_handle(request_id(&open), b"large");
        let fstat = fake.read_request();
        fake.send(
            SSH_FXP_ATTRS,
            &attrs_response(request_id(&fstat), &attrs(size, 88)),
        );
        let read = fake.read_request();
        assert_eq!(read.packet_type, SSH_FXP_READ);
        let mut read_offset = 4;
        assert_eq!(request_string(&read, &mut read_offset), b"large");
        assert_eq!(
            u64::from_be_bytes(
                read.payload[read_offset..read_offset + 8]
                    .try_into()
                    .unwrap()
            ),
            offset
        );
        fake.send_data(request_id(&read), b"data");
        let close = fake.read_request();
        fake.send_status(request_id(&close), SSH_FX_OK);
    });
    let source = open_source(session, &requested).unwrap();
    let mut bytes = [0_u8; 4];
    source.read_at(offset, &mut bytes).unwrap();
    assert_eq!(&bytes, b"data");
    source.close().unwrap();
    worker.join().unwrap();
}

#[test]
fn read_pipeline_accepts_out_of_order_responses() {
    let requested = location("/input/pipelined.czi");
    let canonical = "/canonical/pipelined.czi";
    let total = 300 * 1024_u64;
    let (session, worker) = fake_session(move |fake| {
        fake.expect_init();
        fake.send_version(false);
        let realpath = fake.read_request();
        send_name(
            fake,
            request_id(&realpath),
            &[(canonical, canonical, 0_u32.to_be_bytes().to_vec())],
        );
        let open = fake.read_request();
        fake.send_handle(request_id(&open), b"pipeline");
        let fstat = fake.read_request();
        fake.send(
            SSH_FXP_ATTRS,
            &attrs_response(request_id(&fstat), &attrs(total, 9)),
        );
        let first = fake.read_request();
        let second = fake.read_request();
        assert_eq!(first.packet_type, SSH_FXP_READ);
        assert_eq!(second.packet_type, SSH_FXP_READ);
        assert_eq!(
            u32::from_be_bytes(first.payload[first.payload.len() - 4..].try_into().unwrap()),
            256 * 1024
        );
        assert_eq!(
            u32::from_be_bytes(
                second.payload[second.payload.len() - 4..]
                    .try_into()
                    .unwrap()
            ),
            44 * 1024
        );
        fake.send_data(request_id(&second), &vec![b'b'; 44 * 1024]);
        fake.send_data(request_id(&first), &vec![b'a'; 256 * 1024]);
        let close = fake.read_request();
        fake.send_status(request_id(&close), SSH_FX_OK);
    });
    let source = open_source(session, &requested).unwrap();
    let mut bytes = vec![0_u8; usize::try_from(total).unwrap()];
    source.read_at(0, &mut bytes).unwrap();
    assert!(bytes[..256 * 1024].iter().all(|byte| *byte == b'a'));
    assert!(bytes[256 * 1024..].iter().all(|byte| *byte == b'b'));
    source.close().unwrap();
    worker.join().unwrap();
}

#[test]
fn bounded_read_rejects_eof_before_captured_length() {
    let requested = location("/input/eof.czi");
    let canonical = "/canonical/eof.czi";
    let (session, worker) = fake_session(move |fake| {
        fake.expect_init();
        fake.send_version(false);
        let realpath = fake.read_request();
        send_name(
            fake,
            request_id(&realpath),
            &[(canonical, canonical, 0_u32.to_be_bytes().to_vec())],
        );
        let open = fake.read_request();
        fake.send_handle(request_id(&open), b"eof");
        let fstat = fake.read_request();
        fake.send(
            SSH_FXP_ATTRS,
            &attrs_response(request_id(&fstat), &attrs(5, 9)),
        );
        let read = fake.read_request();
        fake.send_status(request_id(&read), SSH_FX_EOF);
    });
    let source = open_source(session, &requested).unwrap();
    let mut bytes = [0_u8; 5];
    assert!(matches!(
        source.read_at(0, &mut bytes),
        Err(SourceError::Io(_))
    ));
    drop(source);
    worker.join().unwrap();
}

#[test]
fn rejects_mismatched_response_ids() {
    let requested = location("/input/id.czi");
    let (mut session, worker) = fake_session(|fake| {
        fake.expect_init();
        fake.send_version(false);
        let request = fake.read_request();
        send_name(
            fake,
            request_id(&request) + 1,
            &[("/canonical/id.czi", "id", 0_u32.to_be_bytes().to_vec())],
        );
    });
    assert!(matches!(
        session.realpath(&requested),
        Err(SftpError::Protocol(
            SftpProtocolError::MismatchedRequestId { .. }
        ))
    ));
    drop(session);
    worker.join().unwrap();
}

#[test]
fn readdir_reaches_eof_and_closes_handle() {
    let directory = location("/data");
    let (mut session, worker) = fake_session(|fake| {
        fake.expect_init();
        fake.send_version(false);
        let open = fake.read_request();
        assert_eq!(open.packet_type, SSH_FXP_OPENDIR);
        fake.send_handle(request_id(&open), b"dir");
        let page = fake.read_request();
        assert_eq!(page.packet_type, SSH_FXP_READDIR);
        send_name(
            fake,
            request_id(&page),
            &[
                ("one.czi", "one", attrs(1, 2)),
                ("two.czi", "two", 0_u32.to_be_bytes().to_vec()),
            ],
        );
        let eof = fake.read_request();
        assert_eq!(eof.packet_type, SSH_FXP_READDIR);
        fake.send_status(request_id(&eof), SSH_FX_EOF);
        let close = fake.read_request();
        assert_eq!(close.packet_type, SSH_FXP_CLOSE);
        fake.send_status(request_id(&close), SSH_FX_OK);
    });
    let entries = session.read_dir(&directory).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path.as_str(), "one.czi");
    assert_eq!(entries[0].attributes.size, Some(1));
    assert_eq!(entries[1].path.as_str(), "two.czi");
    drop(session);
    worker.join().unwrap();
}

#[test]
fn readdir_limit_rejects_a_page_that_exceeds_the_budget() {
    let directory = location("/data");
    let (mut session, worker) = fake_session(|fake| {
        fake.expect_init();
        fake.send_version(false);
        let open = fake.read_request();
        fake.send_handle(request_id(&open), b"dir");
        let page = fake.read_request();
        assert_eq!(page.packet_type, SSH_FXP_READDIR);
        send_name(
            fake,
            request_id(&page),
            &[
                ("one.czi", "one", 0_u32.to_be_bytes().to_vec()),
                ("two.czi", "two", 0_u32.to_be_bytes().to_vec()),
            ],
        );
    });
    assert!(matches!(
        session.read_dir_limited(&directory, 1),
        Err(SftpError::Protocol(
            SftpProtocolError::DirectoryEntryLimit { limit: 1 }
        ))
    ));
    drop(session);
    worker.join().unwrap();
}

#[test]
fn readdir_rejects_an_empty_name_page() {
    let directory = location("/data");
    let (mut session, worker) = fake_session(|fake| {
        fake.expect_init();
        fake.send_version(false);
        let open = fake.read_request();
        fake.send_handle(request_id(&open), b"dir");
        let page = fake.read_request();
        let payload = [request_id(&page).to_be_bytes(), 0_u32.to_be_bytes()].concat();
        fake.send(SSH_FXP_NAME, &payload);
    });
    assert!(matches!(
        session.read_dir(&directory),
        Err(SftpError::Protocol(SftpProtocolError::EmptyNameResponse))
    ));
    drop(session);
    worker.join().unwrap();
}

#[test]
fn source_requires_fstat_size_and_modification_time() {
    let requested = location("/input/attrs.czi");
    let (session, worker) = fake_session(|fake| {
        fake.expect_init();
        fake.send_version(false);
        let realpath = fake.read_request();
        send_name(
            fake,
            request_id(&realpath),
            &[(
                "/canonical/attrs.czi",
                "attrs",
                0_u32.to_be_bytes().to_vec(),
            )],
        );
        let open = fake.read_request();
        fake.send_handle(request_id(&open), b"attrs");
        let fstat = fake.read_request();
        let mut attributes = SSH_FILEXFER_ATTR_SIZE.to_be_bytes().to_vec();
        attributes.extend_from_slice(&1_u64.to_be_bytes());
        fake.send(
            SSH_FXP_ATTRS,
            &attrs_response(request_id(&fstat), &attributes),
        );
        let close = fake.read_request();
        assert_eq!(close.packet_type, SSH_FXP_CLOSE);
        fake.send_status(request_id(&close), SSH_FX_OK);
        let browse = fake.read_request();
        assert_eq!(browse.packet_type, SSH_FXP_REALPATH);
        send_name(
            fake,
            request_id(&browse),
            &[("/home/test", "/home/test", 0_u32.to_be_bytes().to_vec())],
        );
    });
    let shared = SharedSftpSession::new(session);
    let Err(error) = SftpSource::open_with_shared_session(shared.clone(), &requested) else {
        panic!("FSTAT missing mtime must fail");
    };
    match error {
        SftpError::Protocol(SftpProtocolError::MissingRequiredAttribute { .. }) => {}
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(
        shared
            .with_session(|session| session.realpath(&location(".")))
            .unwrap()
            .as_str(),
        "/home/test"
    );
    drop(shared);
    worker.join().unwrap();
}

#[test]
fn source_version_is_stable_fnv1a() {
    assert_eq!(
        SftpSource::test_source_version(
            &location("/data/image.czi"),
            1_234_567_890_123,
            1_700_000_000
        ),
        0x9680_0614_a0c7_5c3e
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn embedded_pty_reports_unsupported_platform() {
    assert!(matches!(
        SftpSession::start_embedded(&profile("host.example"), &OpenSshConfig::new()),
        Err(SftpError::UnsupportedPlatform {
            feature: "embedded SSH console"
        })
    ));
}
