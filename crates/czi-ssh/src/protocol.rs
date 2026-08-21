use std::io::{self, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::thread::{self, JoinHandle};

use crate::{
    OpenSshConfig, RemoteDirEntry, SftpAttributes, SftpError, SftpExtendedAttribute, SftpLocation,
    SftpProtocolError, SshProfile,
};

pub(crate) const MAX_PACKET_LENGTH: usize = 1024 * 1024;
pub(crate) const MAX_READ_LENGTH: usize = 256 * 1024;
const MAX_READ_REQUESTS: usize = 8;
const MAX_READ_WINDOW: usize = 2 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;

const SSH_FXP_INIT: u8 = 1;
const SSH_FXP_VERSION: u8 = 2;
const SSH_FXP_OPEN: u8 = 3;
const SSH_FXP_CLOSE: u8 = 4;
const SSH_FXP_READ: u8 = 5;
const SSH_FXP_FSTAT: u8 = 8;
const SSH_FXP_OPENDIR: u8 = 11;
const SSH_FXP_READDIR: u8 = 12;
const SSH_FXP_REALPATH: u8 = 16;
const SSH_FXP_STATUS: u8 = 101;
const SSH_FXP_HANDLE: u8 = 102;
const SSH_FXP_DATA: u8 = 103;
const SSH_FXP_NAME: u8 = 104;
const SSH_FXP_ATTRS: u8 = 105;

const SSH_FX_OK: u32 = 0;
const SSH_FX_EOF: u32 = 1;
const SSH_FXF_READ: u32 = 0x0000_0001;

const SSH_FILEXFER_ATTR_SIZE: u32 = 0x0000_0001;
const SSH_FILEXFER_ATTR_UIDGID: u32 = 0x0000_0002;
const SSH_FILEXFER_ATTR_PERMISSIONS: u32 = 0x0000_0004;
const SSH_FILEXFER_ATTR_ACMODTIME: u32 = 0x0000_0008;
const SSH_FILEXFER_ATTR_EXTENDED: u32 = 0x8000_0000;
const KNOWN_ATTRIBUTE_FLAGS: u32 = SSH_FILEXFER_ATTR_SIZE
    | SSH_FILEXFER_ATTR_UIDGID
    | SSH_FILEXFER_ATTR_PERMISSIONS
    | SSH_FILEXFER_ATTR_ACMODTIME
    | SSH_FILEXFER_ATTR_EXTENDED;

/// A live strict-SFTP v3 session over an OpenSSH subsystem child.
pub struct SftpSession {
    transport: Transport,
    next_request_id: Option<u32>,
}

impl SftpSession {
    /// Start `/usr/bin/ssh`, request its `sftp` subsystem, and negotiate SFTP v3.
    ///
    /// # Errors
    ///
    /// Returns an error if OpenSSH cannot start or the resulting stream is not strict SFTP v3.
    pub fn connect(profile: &SshProfile, config: &OpenSshConfig) -> Result<Self, SftpError> {
        let argv = config.sftp_argv(profile);
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|source| SftpError::Spawn { source })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            SftpError::io("capture OpenSSH stdin", io::Error::other("missing stdin"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            SftpError::io("capture OpenSSH stdout", io::Error::other("missing stdout"))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            SftpError::io("capture OpenSSH stderr", io::Error::other("missing stderr"))
        })?;

        let stderr = match start_stderr_drain(stderr) {
            Ok(stderr) => stderr,
            Err(source) => {
                drop(stdin);
                drop(stdout);
                let _ = child.kill();
                let _ = child.wait();
                return Err(SftpError::io("start OpenSSH stderr drain", source));
            }
        };
        let mut session = Self {
            transport: Transport::Process {
                child,
                stdin: Some(stdin),
                stdout: Some(stdout),
                stderr: Some(stderr),
            },
            next_request_id: Some(1),
        };
        let result = session.initialize_inner();
        session.finish(result)?;
        Ok(session)
    }

    /// Resolve an SFTP path and return the server's canonical UTF-8 path.
    ///
    /// # Errors
    ///
    /// Returns an error if the server rejects the request or sends a malformed response.
    pub fn realpath(&mut self, location: &SftpLocation) -> Result<SftpLocation, SftpError> {
        let result = self.realpath_inner(location);
        self.finish(result)
    }

    /// List a directory using OPENDIR and READDIR until `SSH_FX_EOF`, then close its handle.
    ///
    /// # Errors
    ///
    /// Returns an error if the server rejects a request or sends a malformed response.
    pub fn read_dir(&mut self, location: &SftpLocation) -> Result<Vec<RemoteDirEntry>, SftpError> {
        let result = self.read_dir_inner(location, None);
        self.finish(result)
    }

    /// List a directory using OPENDIR and READDIR, rejecting listings above `max_entries`.
    ///
    /// # Errors
    ///
    /// Returns an error if the server rejects a request, sends a malformed response, or returns
    /// more entries than `max_entries`.
    pub fn read_dir_limited(
        &mut self,
        location: &SftpLocation,
        max_entries: usize,
    ) -> Result<Vec<RemoteDirEntry>, SftpError> {
        let result = self.read_dir_inner(location, Some(max_entries));
        self.finish(result)
    }

    pub(crate) fn open_read(&mut self, location: &SftpLocation) -> Result<Vec<u8>, SftpError> {
        let result = self.open_read_inner(location);
        self.finish(result)
    }

    pub(crate) fn fstat(&mut self, handle: &[u8]) -> Result<SftpAttributes, SftpError> {
        let result = self.fstat_inner(handle);
        self.finish(result)
    }

    pub(crate) fn read_exact_at(
        &mut self,
        handle: &[u8],
        offset: u64,
        dst: &mut [u8],
    ) -> Result<(), SftpError> {
        let result = self.read_exact_at_inner(handle, offset, dst);
        self.finish(result)
    }

    pub(crate) fn close(&mut self, handle: &[u8]) -> Result<(), SftpError> {
        let result = self.close_inner(handle);
        self.finish(result)
    }

    fn finish<T>(&mut self, result: Result<T, SftpError>) -> Result<T, SftpError> {
        if result.is_err() {
            self.transport.shutdown();
        }
        result
    }

    fn initialize_inner(&mut self) -> Result<(), SftpError> {
        self.write_packet(SSH_FXP_INIT, &3_u32.to_be_bytes(), "INIT")?;
        let packet = self.read_packet("VERSION")?;
        if packet.packet_type != SSH_FXP_VERSION {
            return Err(SftpProtocolError::UnexpectedPacket {
                actual: packet.packet_type,
                operation: "VERSION",
            }
            .into());
        }
        let mut cursor = Cursor::new(packet.payload());
        let version = cursor.u32("VERSION version")?;
        if version != 3 {
            return Err(SftpProtocolError::UnsupportedVersion { version }.into());
        }
        while !cursor.is_empty() {
            cursor.bytes("VERSION extension name")?;
            cursor.bytes("VERSION extension data")?;
        }
        Ok(())
    }

    fn realpath_inner(&mut self, location: &SftpLocation) -> Result<SftpLocation, SftpError> {
        let request_id = self.send_path_request(SSH_FXP_REALPATH, location)?;
        let entries = self.receive_name(request_id, "REALPATH")?;
        let actual = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        if actual != 1 {
            return Err(SftpProtocolError::UnexpectedNameCount {
                expected: 1,
                actual,
            }
            .into());
        }
        let entry = entries
            .into_iter()
            .next()
            .ok_or(SftpProtocolError::UnexpectedNameCount {
                expected: 1,
                actual: 0,
            })?;
        Ok(entry.path)
    }

    fn open_read_inner(&mut self, location: &SftpLocation) -> Result<Vec<u8>, SftpError> {
        let path_bytes = location.as_str().as_bytes();
        let payload_length = checked_payload_len(4 + path_bytes.len() + 4 + 4, "OPEN")?;
        let (request_id, mut payload) = self.start_request(payload_length)?;
        push_string(&mut payload, path_bytes);
        payload.extend_from_slice(&SSH_FXF_READ.to_be_bytes());
        payload.extend_from_slice(&0_u32.to_be_bytes());
        self.write_packet(SSH_FXP_OPEN, &payload, "OPEN")?;
        self.receive_handle(request_id, "OPEN")
    }

    fn fstat_inner(&mut self, handle: &[u8]) -> Result<SftpAttributes, SftpError> {
        let request_id = self.send_handle_request(SSH_FXP_FSTAT, handle, "FSTAT")?;
        let packet = self.read_packet("FSTAT")?;
        if packet.packet_type == SSH_FXP_STATUS {
            return Err(Self::parse_status_error(
                packet.payload(),
                request_id,
                "FSTAT",
            )?);
        }
        if packet.packet_type != SSH_FXP_ATTRS {
            return Err(SftpProtocolError::UnexpectedPacket {
                actual: packet.packet_type,
                operation: "FSTAT",
            }
            .into());
        }
        let mut cursor = Cursor::new(packet.payload());
        expect_request_id(&mut cursor, request_id, "FSTAT")?;
        let attributes = parse_attributes(&mut cursor)?;
        cursor.finish("FSTAT ATTRS")?;
        if attributes.size.is_none() {
            return Err(SftpProtocolError::MissingRequiredAttribute { attribute: "size" }.into());
        }
        if attributes.access_modify_time.is_none() {
            return Err(SftpProtocolError::MissingRequiredAttribute {
                attribute: "modification time",
            }
            .into());
        }
        Ok(attributes)
    }

    fn read_exact_at_inner(
        &mut self,
        handle: &[u8],
        offset: u64,
        dst: &mut [u8],
    ) -> Result<(), SftpError> {
        if dst.is_empty() {
            return Ok(());
        }

        let mut pending = Vec::new();
        pending.try_reserve_exact(MAX_READ_REQUESTS).map_err(|_| {
            SftpProtocolError::Allocation {
                size: MAX_READ_REQUESTS,
            }
        })?;
        let mut next_offset = offset;
        let mut next_index = 0_usize;
        let mut remaining = dst.len();
        let mut outstanding_bytes = 0_usize;

        while remaining > 0 || !pending.is_empty() {
            while remaining > 0
                && pending.len() < MAX_READ_REQUESTS
                && outstanding_bytes < MAX_READ_WINDOW
            {
                let available = MAX_READ_WINDOW - outstanding_bytes;
                let length = remaining.min(MAX_READ_LENGTH).min(available);
                if length == 0 {
                    break;
                }
                let request_id = self.send_read_request(handle, next_offset, length)?;
                pending.push(PendingRead {
                    request_id,
                    offset: next_offset,
                    destination_start: next_index,
                    length,
                });
                outstanding_bytes += length;
                next_offset = next_offset
                    .checked_add(u64::try_from(length).unwrap_or(u64::MAX))
                    .ok_or(SftpProtocolError::UnexpectedEof)?;
                next_index += length;
                remaining -= length;
            }

            let packet = self.read_packet("READ")?;
            let response_id = response_id(packet.payload(), "READ response")?;
            let pending_index = pending
                .iter()
                .position(|request| request.request_id == response_id)
                .ok_or(SftpProtocolError::MismatchedRequestId {
                    expected: pending.first().map_or(0, |request| request.request_id),
                    actual: response_id,
                })?;
            let request = pending.swap_remove(pending_index);
            outstanding_bytes -= request.length;

            match packet.packet_type {
                SSH_FXP_DATA => {
                    let data_length = copy_data_response(
                        packet.payload(),
                        request.request_id,
                        request.length,
                        &mut dst
                            [request.destination_start..request.destination_start + request.length],
                    )?;
                    if data_length == 0 {
                        return Err(SftpProtocolError::EmptyData.into());
                    }
                    if data_length < request.length {
                        let remainder = request.length - data_length;
                        let remote_offset = request
                            .offset
                            .checked_add(u64::try_from(data_length).unwrap_or(u64::MAX))
                            .ok_or(SftpProtocolError::UnexpectedEof)?;
                        let request_id =
                            self.send_read_request(handle, remote_offset, remainder)?;
                        pending.push(PendingRead {
                            request_id,
                            offset: remote_offset,
                            destination_start: request.destination_start + data_length,
                            length: remainder,
                        });
                        outstanding_bytes += remainder;
                    }
                }
                SSH_FXP_STATUS => {
                    let error =
                        Self::parse_status_error(packet.payload(), request.request_id, "READ")?;
                    match error {
                        SftpError::RemoteStatus {
                            code: SSH_FX_EOF, ..
                        } => return Err(SftpProtocolError::UnexpectedEof.into()),
                        _ => return Err(error),
                    }
                }
                actual => {
                    return Err(SftpProtocolError::UnexpectedPacket {
                        actual,
                        operation: "READ",
                    }
                    .into());
                }
            }
        }
        Ok(())
    }

    fn read_dir_inner(
        &mut self,
        location: &SftpLocation,
        max_entries: Option<usize>,
    ) -> Result<Vec<RemoteDirEntry>, SftpError> {
        let handle = self.open_dir_inner(location)?;
        let result = self.read_dir_entries_inner(&handle, max_entries);
        match result {
            Ok(entries) => {
                self.close_inner(&handle)?;
                Ok(entries)
            }
            Err(error) => Err(error),
        }
    }

    fn open_dir_inner(&mut self, location: &SftpLocation) -> Result<Vec<u8>, SftpError> {
        let request_id = self.send_path_request(SSH_FXP_OPENDIR, location)?;
        self.receive_handle(request_id, "OPENDIR")
    }

    fn read_dir_entries_inner(
        &mut self,
        handle: &[u8],
        max_entries: Option<usize>,
    ) -> Result<Vec<RemoteDirEntry>, SftpError> {
        let mut entries = Vec::new();
        loop {
            let request_id = self.send_handle_request(SSH_FXP_READDIR, handle, "READDIR")?;
            let packet = self.read_packet("READDIR")?;
            match packet.packet_type {
                SSH_FXP_NAME => {
                    let mut page = parse_name_packet(packet.payload(), request_id, "READDIR")?;
                    if page.is_empty() {
                        return Err(SftpProtocolError::EmptyNameResponse.into());
                    }
                    if max_entries.is_some_and(|limit| entries.len() + page.len() > limit) {
                        return Err(SftpProtocolError::DirectoryEntryLimit {
                            limit: max_entries.expect("checked above"),
                        }
                        .into());
                    }
                    entries
                        .try_reserve(page.len())
                        .map_err(|_| SftpProtocolError::Allocation { size: page.len() })?;
                    entries.append(&mut page);
                }
                SSH_FXP_STATUS => {
                    let error = Self::parse_status_error(packet.payload(), request_id, "READDIR")?;
                    match error {
                        SftpError::RemoteStatus {
                            code: SSH_FX_EOF, ..
                        } => return Ok(entries),
                        _ => return Err(error),
                    }
                }
                actual => {
                    return Err(SftpProtocolError::UnexpectedPacket {
                        actual,
                        operation: "READDIR",
                    }
                    .into());
                }
            }
        }
    }

    fn close_inner(&mut self, handle: &[u8]) -> Result<(), SftpError> {
        let request_id = self.send_handle_request(SSH_FXP_CLOSE, handle, "CLOSE")?;
        self.expect_status_ok(request_id, "CLOSE")
    }

    fn send_path_request(
        &mut self,
        packet_type: u8,
        path: &SftpLocation,
    ) -> Result<u32, SftpError> {
        let path = path.as_str().as_bytes();
        let payload_length = checked_payload_len(4 + path.len(), "path request")?;
        let (request_id, mut payload) = self.start_request(payload_length)?;
        push_string(&mut payload, path);
        self.write_packet(packet_type, &payload, "path request")?;
        Ok(request_id)
    }

    fn send_handle_request(
        &mut self,
        packet_type: u8,
        handle: &[u8],
        operation: &'static str,
    ) -> Result<u32, SftpError> {
        let payload_length = checked_payload_len(4 + handle.len(), operation)?;
        let (request_id, mut payload) = self.start_request(payload_length)?;
        push_string(&mut payload, handle);
        self.write_packet(packet_type, &payload, operation)?;
        Ok(request_id)
    }

    fn send_read_request(
        &mut self,
        handle: &[u8],
        offset: u64,
        length: usize,
    ) -> Result<u32, SftpError> {
        let length = u32::try_from(length)
            .map_err(|_| SftpProtocolError::InvalidPacketLength { length: u32::MAX })?;
        let handle_length = checked_payload_len(4 + handle.len(), "READ")?;
        let payload_length = checked_payload_len(handle_length + 8 + 4, "READ")?;
        let (request_id, mut payload) = self.start_request(payload_length)?;
        push_string(&mut payload, handle);
        payload.extend_from_slice(&offset.to_be_bytes());
        payload.extend_from_slice(&length.to_be_bytes());
        self.write_packet(SSH_FXP_READ, &payload, "READ")?;
        Ok(request_id)
    }

    fn start_request(&mut self, payload_length: usize) -> Result<(u32, Vec<u8>), SftpError> {
        let request_id = self
            .next_request_id
            .take()
            .ok_or(SftpProtocolError::RequestIdExhausted)?;
        self.next_request_id = request_id.checked_add(1);
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(payload_length)
            .map_err(|_| SftpProtocolError::Allocation {
                size: payload_length,
            })?;
        payload.extend_from_slice(&request_id.to_be_bytes());
        Ok((request_id, payload))
    }

    fn receive_handle(
        &mut self,
        request_id: u32,
        operation: &'static str,
    ) -> Result<Vec<u8>, SftpError> {
        let packet = self.read_packet(operation)?;
        if packet.packet_type == SSH_FXP_STATUS {
            return Err(Self::parse_status_error(
                packet.payload(),
                request_id,
                operation,
            )?);
        }
        if packet.packet_type != SSH_FXP_HANDLE {
            return Err(SftpProtocolError::UnexpectedPacket {
                actual: packet.packet_type,
                operation,
            }
            .into());
        }
        let mut cursor = Cursor::new(packet.payload());
        expect_request_id(&mut cursor, request_id, operation)?;
        let handle = copy_bytes(cursor.bytes("HANDLE")?)?;
        cursor.finish("HANDLE")?;
        if handle.is_empty() {
            return Err(SftpProtocolError::Truncated { field: "HANDLE" }.into());
        }
        Ok(handle)
    }

    fn receive_name(
        &mut self,
        request_id: u32,
        operation: &'static str,
    ) -> Result<Vec<RemoteDirEntry>, SftpError> {
        let packet = self.read_packet(operation)?;
        if packet.packet_type == SSH_FXP_STATUS {
            return Err(Self::parse_status_error(
                packet.payload(),
                request_id,
                operation,
            )?);
        }
        if packet.packet_type != SSH_FXP_NAME {
            return Err(SftpProtocolError::UnexpectedPacket {
                actual: packet.packet_type,
                operation,
            }
            .into());
        }
        parse_name_packet(packet.payload(), request_id, operation)
    }

    fn expect_status_ok(
        &mut self,
        request_id: u32,
        operation: &'static str,
    ) -> Result<(), SftpError> {
        let packet = self.read_packet(operation)?;
        if packet.packet_type != SSH_FXP_STATUS {
            return Err(SftpProtocolError::UnexpectedPacket {
                actual: packet.packet_type,
                operation,
            }
            .into());
        }
        let error = Self::parse_status_error(packet.payload(), request_id, operation)?;
        match error {
            SftpError::RemoteStatus {
                code: SSH_FX_OK, ..
            } => Ok(()),
            _ => Err(error),
        }
    }

    fn parse_status_error(
        payload: &[u8],
        request_id: u32,
        operation: &'static str,
    ) -> Result<SftpError, SftpError> {
        let mut cursor = Cursor::new(payload);
        expect_request_id(&mut cursor, request_id, operation)?;
        let code = cursor.u32("STATUS code")?;
        let message = copy_utf8(cursor.bytes("STATUS message")?, "STATUS message")?;
        cursor.bytes("STATUS language")?;
        cursor.finish("STATUS")?;
        Ok(SftpError::RemoteStatus {
            operation,
            code,
            message,
        })
    }

    fn write_packet(
        &mut self,
        packet_type: u8,
        payload: &[u8],
        operation: &'static str,
    ) -> Result<(), SftpError> {
        let length = payload
            .len()
            .checked_add(1)
            .ok_or(SftpProtocolError::InvalidPacketLength { length: u32::MAX })?;
        if length > MAX_PACKET_LENGTH {
            return Err(SftpProtocolError::InvalidPacketLength {
                length: u32::try_from(length).unwrap_or(u32::MAX),
            }
            .into());
        }
        let length = u32::try_from(length)
            .map_err(|_| SftpProtocolError::InvalidPacketLength { length: u32::MAX })?;
        self.transport
            .write_all(&length.to_be_bytes(), operation)
            .and_then(|()| self.transport.write_all(&[packet_type], operation))
            .and_then(|()| self.transport.write_all(payload, operation))
            .and_then(|()| self.transport.flush(operation))
    }

    fn read_packet(&mut self, operation: &'static str) -> Result<Packet, SftpError> {
        let mut header = [0_u8; 4];
        self.transport.read_exact(&mut header, operation)?;
        let length = u32::from_be_bytes(header);
        let length_usize = usize::try_from(length)
            .map_err(|_| SftpProtocolError::InvalidPacketLength { length })?;
        if length_usize == 0 || length_usize > MAX_PACKET_LENGTH {
            return Err(SftpProtocolError::InvalidPacketLength { length }.into());
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length_usize)
            .map_err(|_| SftpProtocolError::Allocation { size: length_usize })?;
        bytes.resize(length_usize, 0);
        self.transport.read_exact(&mut bytes, operation)?;
        Ok(Packet {
            packet_type: bytes[0],
            bytes,
        })
    }
}

impl Drop for SftpSession {
    fn drop(&mut self) {
        self.transport.shutdown();
    }
}

struct Packet {
    packet_type: u8,
    bytes: Vec<u8>,
}

impl Packet {
    fn payload(&self) -> &[u8] {
        &self.bytes[1..]
    }
}

struct PendingRead {
    request_id: u32,
    offset: u64,
    destination_start: usize,
    length: usize,
}

enum Transport {
    Process {
        child: Child,
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    },
    #[cfg(test)]
    Test {
        reader: Option<std::net::TcpStream>,
        writer: Option<std::net::TcpStream>,
    },
}

impl Transport {
    fn write_all(&mut self, bytes: &[u8], operation: &'static str) -> Result<(), SftpError> {
        let result = match self {
            Self::Process { stdin, .. } => stdin.as_mut().map_or_else(
                || Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed")),
                |stdin| stdin.write_all(bytes),
            ),
            #[cfg(test)]
            Self::Test { writer, .. } => writer.as_mut().map_or_else(
                || {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "test writer closed",
                    ))
                },
                |writer| writer.write_all(bytes),
            ),
        };
        self.write_result(result, operation)
    }

    fn flush(&mut self, operation: &'static str) -> Result<(), SftpError> {
        let result = match self {
            Self::Process { stdin, .. } => stdin.as_mut().map_or_else(
                || Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed")),
                ChildStdin::flush,
            ),
            #[cfg(test)]
            Self::Test { writer, .. } => writer.as_mut().map_or_else(
                || {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "test writer closed",
                    ))
                },
                Write::flush,
            ),
        };
        self.write_result(result, operation)
    }

    fn write_result(
        &mut self,
        result: io::Result<()>,
        operation: &'static str,
    ) -> Result<(), SftpError> {
        match result {
            Ok(()) => Ok(()),
            Err(source)
                if source.kind() == io::ErrorKind::BrokenPipe
                    && matches!(self, Self::Process { .. }) =>
            {
                Err(self.child_exited(operation))
            }
            Err(source) => Err(SftpError::io(operation, source)),
        }
    }

    fn read_exact(&mut self, bytes: &mut [u8], operation: &'static str) -> Result<(), SftpError> {
        let result = match self {
            Self::Process { stdout, .. } => stdout
                .as_mut()
                .ok_or_else(|| {
                    SftpError::io(
                        operation,
                        io::Error::new(io::ErrorKind::UnexpectedEof, "stdout closed"),
                    )
                })?
                .read_exact(bytes),
            #[cfg(test)]
            Self::Test { reader, .. } => reader
                .as_mut()
                .ok_or_else(|| {
                    SftpError::io(
                        operation,
                        io::Error::new(io::ErrorKind::UnexpectedEof, "test reader closed"),
                    )
                })?
                .read_exact(bytes),
        };
        match result {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => {
                if matches!(self, Self::Process { .. }) {
                    Err(self.child_exited(operation))
                } else {
                    Err(SftpError::io(operation, source))
                }
            }
            Err(source) => Err(SftpError::io(operation, source)),
        }
    }

    fn child_exited(&mut self, operation: &'static str) -> SftpError {
        match self {
            Self::Process {
                child,
                stdin,
                stdout,
                stderr,
            } => {
                drop(stdin.take());
                drop(stdout.take());
                let status = if let Ok(Some(status)) = child.try_wait() {
                    Some(status)
                } else {
                    let _ = child.kill();
                    child.wait().ok()
                };
                let stderr = collect_stderr(stderr);
                SftpError::ChildExited {
                    operation,
                    status,
                    stderr,
                }
            }
            #[cfg(test)]
            Self::Test { .. } => SftpError::io(
                operation,
                io::Error::new(io::ErrorKind::UnexpectedEof, "test transport ended"),
            ),
        }
    }

    fn shutdown(&mut self) {
        match self {
            Self::Process {
                child,
                stdin,
                stdout,
                stderr,
            } => {
                drop(stdin.take());
                drop(stdout.take());
                let _ = child.kill();
                let _ = child.wait();
                let _ = collect_stderr(stderr);
            }
            #[cfg(test)]
            Self::Test { reader, writer } => {
                drop(reader.take());
                drop(writer.take());
            }
        }
    }
}

fn start_stderr_drain(mut stderr: ChildStderr) -> io::Result<JoinHandle<io::Result<Vec<u8>>>> {
    thread::Builder::new()
        .name("czi-ssh-stderr".into())
        .spawn(move || {
            let mut captured = Vec::new();
            let mut can_capture = true;
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stderr.read(&mut buffer)?;
                if count == 0 {
                    return Ok(captured);
                }
                if can_capture && captured.len() < STDERR_LIMIT {
                    let take = (STDERR_LIMIT - captured.len()).min(count);
                    if captured.try_reserve(take).is_ok() {
                        captured.extend_from_slice(&buffer[..take]);
                    } else {
                        can_capture = false;
                    }
                }
            }
        })
}

fn collect_stderr(stderr: &mut Option<JoinHandle<io::Result<Vec<u8>>>>) -> String {
    let Some(join) = stderr.take() else {
        return String::new();
    };
    match join.join() {
        Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(Err(error)) => format!("stderr drain failed: {error}"),
        Err(_) => "stderr drain thread panicked".to_owned(),
    }
}

fn checked_payload_len(length: usize, _operation: &'static str) -> Result<usize, SftpError> {
    let total = length
        .checked_add(4)
        .ok_or(SftpProtocolError::InvalidPacketLength { length: u32::MAX })?;
    let frame_length = total
        .checked_add(1)
        .ok_or(SftpProtocolError::InvalidPacketLength { length: u32::MAX })?;
    if frame_length > MAX_PACKET_LENGTH {
        return Err(SftpProtocolError::InvalidPacketLength {
            length: u32::try_from(frame_length).unwrap_or(u32::MAX),
        }
        .into());
    }
    Ok(total)
}

fn push_string(payload: &mut Vec<u8>, value: &[u8]) {
    let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
    payload.extend_from_slice(&length.to_be_bytes());
    payload.extend_from_slice(value);
}

fn response_id(payload: &[u8], field: &'static str) -> Result<u32, SftpError> {
    Cursor::new(payload).u32(field).map_err(Into::into)
}

fn expect_request_id(
    cursor: &mut Cursor<'_>,
    expected: u32,
    operation: &'static str,
) -> Result<(), SftpError> {
    let actual = cursor.u32("response request ID")?;
    if actual != expected {
        return Err(SftpProtocolError::MismatchedRequestId { expected, actual }.into());
    }
    let _ = operation;
    Ok(())
}

fn parse_name_packet(
    payload: &[u8],
    request_id: u32,
    operation: &'static str,
) -> Result<Vec<RemoteDirEntry>, SftpError> {
    let mut cursor = Cursor::new(payload);
    expect_request_id(&mut cursor, request_id, operation)?;
    let count = cursor.u32("NAME count")?;
    let count_usize =
        usize::try_from(count).map_err(|_| SftpProtocolError::Allocation { size: usize::MAX })?;
    if count_usize > cursor.remaining() / 12 {
        return Err(SftpProtocolError::Truncated {
            field: "NAME entries",
        }
        .into());
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count_usize)
        .map_err(|_| SftpProtocolError::Allocation { size: count_usize })?;
    for _ in 0..count {
        let path = SftpLocation::new(copy_utf8(cursor.bytes("NAME filename")?, "NAME filename")?)?;
        let long_name = copy_utf8(cursor.bytes("NAME longname")?, "NAME longname")?;
        let attributes = parse_attributes(&mut cursor)?;
        entries.push(RemoteDirEntry {
            path,
            long_name,
            attributes,
        });
    }
    cursor.finish("NAME")?;
    Ok(entries)
}

fn parse_attributes(cursor: &mut Cursor<'_>) -> Result<SftpAttributes, SftpError> {
    let flags = cursor.u32("ATTRS flags")?;
    if flags & !KNOWN_ATTRIBUTE_FLAGS != 0 {
        return Err(SftpProtocolError::UnknownAttributeFlags { flags }.into());
    }
    let size = if flags & SSH_FILEXFER_ATTR_SIZE != 0 {
        Some(cursor.u64("ATTRS size")?)
    } else {
        None
    };
    let uid_gid = if flags & SSH_FILEXFER_ATTR_UIDGID != 0 {
        Some((cursor.u32("ATTRS uid")?, cursor.u32("ATTRS gid")?))
    } else {
        None
    };
    let permissions = if flags & SSH_FILEXFER_ATTR_PERMISSIONS != 0 {
        Some(cursor.u32("ATTRS permissions")?)
    } else {
        None
    };
    let access_modify_time = if flags & SSH_FILEXFER_ATTR_ACMODTIME != 0 {
        Some((
            cursor.u32("ATTRS access time")?,
            cursor.u32("ATTRS modification time")?,
        ))
    } else {
        None
    };
    let extended = if flags & SSH_FILEXFER_ATTR_EXTENDED != 0 {
        let count = cursor.u32("ATTRS extended count")?;
        let count_usize = usize::try_from(count)
            .map_err(|_| SftpProtocolError::Allocation { size: usize::MAX })?;
        if count_usize > cursor.remaining() / 8 {
            return Err(SftpProtocolError::Truncated {
                field: "ATTRS extended values",
            }
            .into());
        }
        let mut values = Vec::new();
        values
            .try_reserve_exact(count_usize)
            .map_err(|_| SftpProtocolError::Allocation { size: count_usize })?;
        for _ in 0..count {
            values.push(SftpExtendedAttribute {
                name: copy_bytes(cursor.bytes("ATTRS extended type")?)?,
                value: copy_bytes(cursor.bytes("ATTRS extended data")?)?,
            });
        }
        values
    } else {
        Vec::new()
    };
    Ok(SftpAttributes {
        size,
        uid_gid,
        permissions,
        access_modify_time,
        extended,
    })
}

fn copy_data_response(
    payload: &[u8],
    request_id: u32,
    requested: usize,
    destination: &mut [u8],
) -> Result<usize, SftpError> {
    let mut cursor = Cursor::new(payload);
    expect_request_id(&mut cursor, request_id, "READ")?;
    let data = cursor.bytes("DATA")?;
    cursor.finish("DATA")?;
    if data.len() > requested {
        return Err(SftpProtocolError::DataTooLong {
            requested,
            actual: data.len(),
        }
        .into());
    }
    destination[..data.len()].copy_from_slice(data);
    Ok(data.len())
}

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, SftpError> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| SftpProtocolError::Allocation { size: bytes.len() })?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

fn copy_utf8(bytes: &[u8], field: &'static str) -> Result<String, SftpError> {
    String::from_utf8(copy_bytes(bytes)?)
        .map_err(|_| SftpProtocolError::InvalidUtf8 { field }.into())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, SftpProtocolError> {
        let bytes = self.take(4, field)?;
        Ok(u32::from_be_bytes(
            bytes.try_into().expect("fixed u32 length"),
        ))
    }

    fn u64(&mut self, field: &'static str) -> Result<u64, SftpProtocolError> {
        let bytes = self.take(8, field)?;
        Ok(u64::from_be_bytes(
            bytes.try_into().expect("fixed u64 length"),
        ))
    }

    fn bytes(&mut self, field: &'static str) -> Result<&'a [u8], SftpProtocolError> {
        let length = self.u32(field)?;
        let length = usize::try_from(length).map_err(|_| SftpProtocolError::Truncated { field })?;
        self.take(length, field)
    }

    fn take(&mut self, length: usize, field: &'static str) -> Result<&'a [u8], SftpProtocolError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(SftpProtocolError::Truncated { field })?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(SftpProtocolError::Truncated { field })?;
        self.position = end;
        Ok(bytes)
    }

    fn finish(&self, context: &'static str) -> Result<(), SftpProtocolError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(SftpProtocolError::TrailingData { context })
        }
    }
}

#[cfg(test)]
impl SftpSession {
    pub(crate) fn with_test_transport(stream: std::net::TcpStream) -> Result<Self, SftpError> {
        let reader = stream
            .try_clone()
            .map_err(|source| SftpError::io("clone test SFTP stream", source))?;
        let mut session = Self {
            transport: Transport::Test {
                reader: Some(reader),
                writer: Some(stream),
            },
            next_request_id: Some(1),
        };
        let result = session.initialize_inner();
        session.finish(result)?;
        Ok(session)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
