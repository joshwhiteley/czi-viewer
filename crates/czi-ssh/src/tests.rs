use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

use czi_core::{RandomAccessSource, SourceError};

use crate::{
    ControlPath, OPENSSH_PATH, OpenSshConfig, OpenSshConfigError, SftpError, SftpLocation,
    SftpLocationError, SftpProtocolError, SftpSession, SftpSource, SshProfile, SshProfileError,
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
fn command_builders_keep_paths_out_of_argv_and_preserve_host_checks() {
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
            .any(|pair| pair == ["-o", "ControlPersist=no"])
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
            .any(|argument| argument.contains("UserKnownHostsFile=/dev/null"))
    );

    let master = config.noninteractive_master_argv(&destination).unwrap();
    assert!(
        master
            .windows(2)
            .any(|pair| pair == [OsString::from("-o"), OsString::from("BatchMode=yes")])
    );
    let terminal = config.terminal_bootstrap_command(&destination).unwrap();
    assert!(terminal.contains("'BatchMode=no'"));
    assert!(terminal.contains("'\"'\"'"));
    assert!(!terminal.contains(remote.as_str()));

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

#[test]
fn master_command_requires_a_control_path() {
    let error = OpenSshConfig::new()
        .noninteractive_master_argv(&profile("host"))
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
    });
    let Err(error) = open_source(session, &requested) else {
        panic!("FSTAT missing mtime must fail");
    };
    match error {
        SftpError::Protocol(SftpProtocolError::MissingRequiredAttribute { .. }) => {}
        other => panic!("unexpected error: {other:?}"),
    }
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
