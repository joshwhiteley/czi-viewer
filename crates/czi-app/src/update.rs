//! Bounded, authenticated preview-update core.
//!
//! This module is deliberately independent of viewer and dataset state. The parent app may spawn
//! [`UpdateWorker`], poll [`UpdateEvent`] values, and submit only the explicitly named
//! [`UpdateWorker::install_after_confirmation`] command after user confirmation.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

const RELEASES_URL: &str =
    "https://api.github.com/repos/joshwhiteley/czi-viewer/releases?per_page=20";
const USER_AGENT: &str = "CZI-Viewer-Updater";
const PUBLIC_KEY: [u8; 32] = [
    0xb3, 0xf3, 0x2b, 0x2a, 0x26, 0xc3, 0x34, 0xf6, 0x95, 0x6e, 0xc7, 0x3a, 0x57, 0x2b, 0x14, 0x22,
    0x3f, 0x54, 0xf1, 0xc8, 0x81, 0x1a, 0x9a, 0x65, 0x9c, 0xe2, 0xd9, 0xcf, 0x87, 0xd0, 0xbe, 0x3c,
];

const AUTO_INTERVAL_SECS: u64 = 24 * 60 * 60;
const AUTO_POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
const CLAIM_STALE_AFTER: Duration = Duration::from_secs(60);
const API_BYTES: usize = 256 * 1024;
const MANIFEST_BYTES: usize = 8 * 1024;
const SIGNATURE_BYTES: usize = 64;
const STATE_BYTES: u64 = 1024;
const RECEIPT_BYTES: u64 = 2 * 1024;
const MAX_RELEASES: usize = 20;
const MAX_ASSETS: usize = 64;
const MAX_REDIRECTS: usize = 3;
const MAX_DMG_BYTES: u64 = 1024 * 1024 * 1024;
const IO_BUFFER_BYTES: usize = 64 * 1024;
const COMMAND_CAPACITY: usize = 4;
const EVENT_CAPACITY: usize = 8;
const NETWORK_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const HELPER_WAIT_LIMIT: Duration = Duration::from_secs(120);
const SYSTEM_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
const COPY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SETTINGS_SEQUENCE_START: u64 = 1;
const HELPER_FLAG: &str = "--czi-apply-verified-update";
const PRODUCT: &str = "CZI Viewer";
const BUNDLE_IDENTIFIER: &str = "io.github.joshwhiteley.czi-viewer";
const TARGET: &str = "aarch64-apple-darwin";
const MANIFEST_SCHEMA: u32 = 1;
const CHANNEL: &str = "preview";

static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(SETTINGS_SEQUENCE_START);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedUpdate {
    version: Version,
    tag: String,
    dmg_name: String,
    dmg_url: Url,
    dmg_size: u64,
    dmg_sha256: [u8; 32],
    minimum_macos: Version,
}

impl VerifiedUpdate {
    pub(crate) fn version(&self) -> &Version {
        &self.version
    }

    pub(crate) fn release_page_url(&self) -> String {
        format!(
            "https://github.com/joshwhiteley/czi-viewer/releases/tag/{}",
            self.tag
        )
    }
}

pub(crate) fn current_application_bundle() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let bundle = executable.parent()?.parent()?.parent()?;
    (bundle == Path::new("/Applications/CZI Viewer.app")).then(|| bundle.to_path_buf())
}

#[derive(Debug)]
pub(crate) enum UpdateEvent {
    UpdateAvailable {
        update: Box<VerifiedUpdate>,
        automatic: bool,
    },
    ManualUpToDate,
    ManualError(String),
    InstallPrepared,
    InstallReadyToClose,
    InstallError(String),
}

#[derive(Debug)]
enum WorkerCommand {
    ManualCheck,
    InstallConfirmed {
        update: Box<VerifiedUpdate>,
        current_bundle: PathBuf,
    },
    AuthorizeInstallHandoff,
    AcknowledgeStartup,
    Cancel,
}

/// A bounded background worker. It never receives viewer, CZI, SSH, or user-profile data.
pub(crate) struct UpdateWorker {
    commands: Option<SyncSender<WorkerCommand>>,
    events: Receiver<UpdateEvent>,
    cancellation: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl UpdateWorker {
    /// Spawn the worker and perform the due automatic check entirely off the UI thread.
    pub(crate) fn spawn() -> Self {
        Self::spawn_with(Box::new(HttpTransport::new()), default_state_path())
    }

    fn spawn_with(transport: Box<dyn Transport>, state_path: Result<PathBuf, String>) -> Self {
        let (commands, command_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (event_tx, events) = mpsc::sync_channel(EVENT_CAPACITY);
        let cancellation = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_cancellation = Arc::clone(&cancellation);
        let worker_stopping = Arc::clone(&stopping);
        let join = thread::Builder::new()
            .name(String::from("czi-update-worker"))
            .spawn(move || {
                worker_loop(
                    transport.as_ref(),
                    &state_path,
                    &command_rx,
                    &event_tx,
                    &worker_cancellation,
                    &worker_stopping,
                );
            })
            .expect("start update worker");
        Self {
            commands: Some(commands),
            events,
            cancellation,
            stopping,
            join: Some(join),
        }
    }

    /// Request a user-visible check. Manual checks bypass the automatic cadence.
    pub(crate) fn check_now(&self) -> Result<(), String> {
        self.send(WorkerCommand::ManualCheck)
    }

    /// Download and prepare an update only after the parent has obtained explicit confirmation.
    pub(crate) fn install_after_confirmation(
        &self,
        update: VerifiedUpdate,
        current_bundle: PathBuf,
    ) -> Result<(), String> {
        self.send(WorkerCommand::InstallConfirmed {
            update: Box::new(update),
            current_bundle,
        })
    }

    pub(crate) fn cancel(&self) -> Result<(), String> {
        self.cancellation.store(true, Ordering::Release);
        self.send(WorkerCommand::Cancel)
    }

    pub(crate) fn authorize_install_handoff(&self) -> Result<(), String> {
        self.send(WorkerCommand::AuthorizeInstallHandoff)
    }

    pub(crate) fn acknowledge_startup(&self) -> Result<(), String> {
        self.send(WorkerCommand::AcknowledgeStartup)
    }

    pub(crate) fn try_recv(&self) -> Option<UpdateEvent> {
        self.events.try_recv().ok()
    }

    fn send(&self, command: WorkerCommand) -> Result<(), String> {
        self.commands
            .as_ref()
            .ok_or_else(|| String::from("Update worker is shut down."))?
            .try_send(command)
            .map_err(|error| format!("Update command queue is unavailable: {error}"))
    }
}

impl Drop for UpdateWorker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.cancellation.store(true, Ordering::Release);
        self.commands.take();
        // Dropping JoinHandle detaches. Never block the UI on network or a macOS system tool.
        self.join.take();
    }
}

fn worker_loop(
    transport: &dyn Transport,
    state_path: &Result<PathBuf, String>,
    commands: &Receiver<WorkerCommand>,
    events: &SyncSender<UpdateEvent>,
    cancellation: &AtomicBool,
    stopping: &AtomicBool,
) {
    let mut pending_apply = None;
    if !run_due_automatic_check(transport, state_path, events, cancellation, stopping) {
        return;
    }

    while !stopping.load(Ordering::Acquire) {
        let command = match commands.recv_timeout(AUTO_POLL_INTERVAL) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !run_due_automatic_check(transport, state_path, events, cancellation, stopping) {
                    return;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if stopping.load(Ordering::Acquire) {
            return;
        }
        match command {
            WorkerCommand::ManualCheck => {
                cancellation.store(false, Ordering::Release);
                let event = match check_for_update(transport, cancellation) {
                    Ok(Some(update)) => UpdateEvent::UpdateAvailable {
                        update: Box::new(update),
                        automatic: false,
                    },
                    Ok(None) => UpdateEvent::ManualUpToDate,
                    Err(error) => UpdateEvent::ManualError(error),
                };
                if !emit(events, event) {
                    return;
                }
            }
            WorkerCommand::InstallConfirmed {
                update,
                current_bundle,
            } => {
                cancellation.store(false, Ordering::Release);
                let event = match prepare_install(transport, &update, &current_bundle, cancellation)
                {
                    Ok(plan) => {
                        pending_apply = Some(plan);
                        UpdateEvent::InstallPrepared
                    }
                    Err(error) => UpdateEvent::InstallError(error),
                };
                if !emit(events, event) {
                    return;
                }
            }
            WorkerCommand::AuthorizeInstallHandoff => {
                let Some(plan) = pending_apply.take() else {
                    continue;
                };
                let event = if cancellation.load(Ordering::Acquire) {
                    UpdateEvent::InstallError(String::from("Update operation was cancelled."))
                } else {
                    match plan.spawn_helper() {
                        Ok(()) => UpdateEvent::InstallReadyToClose,
                        Err(error) => UpdateEvent::InstallError(error),
                    }
                };
                if !emit(events, event) {
                    return;
                }
            }
            WorkerCommand::AcknowledgeStartup => {
                #[cfg(target_os = "macos")]
                let _ = cleanup_backups_after_successful_start();
            }
            WorkerCommand::Cancel => {
                cancellation.store(true, Ordering::Release);
                pending_apply = None;
            }
        }
    }
}

fn run_due_automatic_check(
    transport: &dyn Transport,
    state_path: &Result<PathBuf, String>,
    events: &SyncSender<UpdateEvent>,
    cancellation: &AtomicBool,
    stopping: &AtomicBool,
) -> bool {
    if let Ok(path) = state_path.as_deref()
        && let Ok(now) = unix_now()
        && claim_automatic_attempt(path, now).unwrap_or(false)
        && !stopping.load(Ordering::Acquire)
    {
        cancellation.store(false, Ordering::Release);
        // Automatic failures and an up-to-date result are intentionally silent.
        if let Ok(Some(update)) = check_for_update(transport, cancellation) {
            return emit(
                events,
                UpdateEvent::UpdateAvailable {
                    update: Box::new(update),
                    automatic: true,
                },
            );
        }
    }
    true
}

fn emit(events: &SyncSender<UpdateEvent>, event: UpdateEvent) -> bool {
    events.send(event).is_ok()
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Clone, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SignedManifest {
    schema: u32,
    channel: String,
    version: String,
    tag: String,
    target: String,
    minimum_macos: String,
    bundle_identifier: String,
    dmg_name: String,
    dmg_size: u64,
    dmg_sha256: String,
}

fn check_for_update(
    transport: &dyn Transport,
    cancellation: &AtomicBool,
) -> Result<Option<VerifiedUpdate>, String> {
    check_cancelled(cancellation)?;
    let releases_bytes = transport.get_bounded(RELEASES_URL, API_BYTES, cancellation)?;
    let releases: Vec<GithubRelease> = serde_json::from_slice(&releases_bytes)
        .map_err(|error| format!("GitHub release response is invalid: {error}"))?;
    if releases.len() > MAX_RELEASES {
        return Err(format!(
            "GitHub returned more than the {MAX_RELEASES}-release bound."
        ));
    }

    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("Current application version is invalid: {error}"))?;
    let running_macos = running_macos_version()?;
    let mut candidates = release_candidates(releases, &current)?;
    candidates.sort_by(|left, right| right.version.cmp(&left.version));

    for candidate in candidates {
        check_cancelled(cancellation)?;
        let manifest_bytes = transport.get_bounded(
            candidate.manifest_url.as_str(),
            MANIFEST_BYTES,
            cancellation,
        )?;
        let signature_bytes = transport.get_bounded(
            candidate.signature_url.as_str(),
            SIGNATURE_BYTES,
            cancellation,
        )?;
        let verified = verify_manifest(
            &manifest_bytes,
            &signature_bytes,
            &candidate,
            &current,
            &running_macos,
            &PUBLIC_KEY,
        );
        if let Ok(update) = verified {
            return Ok(Some(update));
        }
    }
    Ok(None)
}

struct ReleaseCandidate {
    version: Version,
    tag: String,
    manifest_url: Url,
    signature_url: Url,
    dmg: GithubAsset,
}

fn release_candidates(
    releases: Vec<GithubRelease>,
    current: &Version,
) -> Result<Vec<ReleaseCandidate>, String> {
    let mut candidates = Vec::new();
    for release in releases {
        if release.assets.len() > MAX_ASSETS {
            return Err(format!(
                "Release {} exceeds the {MAX_ASSETS}-asset bound.",
                release.tag_name
            ));
        }
        if release.draft || !release.prerelease {
            continue;
        }
        let Some(version_text) = release.tag_name.strip_prefix("preview-v") else {
            continue;
        };
        let Ok(version) = Version::parse(version_text) else {
            continue;
        };
        if version <= *current || release.tag_name != format!("preview-v{version}") {
            continue;
        }
        let stem = format!("CZI-Viewer-{version}-{TARGET}-preview");
        let manifest_name = format!("{stem}-update.json");
        let signature_name = format!("{manifest_name}.sig");
        let dmg_name = format!("{stem}.dmg");
        let manifest = exactly_one_asset(&release.assets, &manifest_name)?;
        let signature = exactly_one_asset(&release.assets, &signature_name)?;
        let dmg = exactly_one_asset(&release.assets, &dmg_name)?;
        if signature.size != SIGNATURE_BYTES as u64
            || manifest.size > MANIFEST_BYTES as u64
            || dmg.size == 0
            || dmg.size > MAX_DMG_BYTES
        {
            continue;
        }
        candidates.push(ReleaseCandidate {
            version,
            tag: release.tag_name,
            manifest_url: validate_release_asset_url(&manifest.browser_download_url)?,
            signature_url: validate_release_asset_url(&signature.browser_download_url)?,
            dmg: dmg.clone(),
        });
    }
    Ok(candidates)
}

fn exactly_one_asset<'a>(assets: &'a [GithubAsset], name: &str) -> Result<&'a GithubAsset, String> {
    let mut matches = assets.iter().filter(|asset| asset.name == name);
    let Some(asset) = matches.next() else {
        return Err(format!("Release is missing required asset {name}."));
    };
    if matches.next().is_some() {
        return Err(format!("Release contains duplicate asset {name}."));
    }
    Ok(asset)
}

fn verify_manifest(
    bytes: &[u8],
    signature_bytes: &[u8],
    candidate: &ReleaseCandidate,
    current: &Version,
    running_macos: &Version,
    public_key: &[u8; 32],
) -> Result<VerifiedUpdate, String> {
    if bytes.is_empty() || bytes.len() > MANIFEST_BYTES {
        return Err(String::from("Update manifest has an invalid size."));
    }
    if signature_bytes.len() != SIGNATURE_BYTES {
        return Err(String::from("Update signature must be exactly 64 bytes."));
    }
    let key = VerifyingKey::from_bytes(public_key)
        .map_err(|error| format!("Embedded update key is invalid: {error}"))?;
    let signature = Signature::from_slice(signature_bytes)
        .map_err(|error| format!("Update signature is invalid: {error}"))?;
    key.verify_strict(bytes, &signature)
        .map_err(|_| String::from("Update manifest signature did not verify."))?;

    let manifest: SignedManifest = serde_json::from_slice(bytes)
        .map_err(|error| format!("Signed update manifest is invalid: {error}"))?;
    let version = Version::parse(&manifest.version)
        .map_err(|error| format!("Signed update version is invalid: {error}"))?;
    let minimum_macos = normalize_os_version(&manifest.minimum_macos)
        .map_err(|error| format!("Signed minimum macOS version is invalid: {error}"))?;
    let expected_tag = format!("preview-v{version}");
    let expected_dmg = format!("CZI-Viewer-{version}-{TARGET}-preview.dmg");
    if manifest.schema != MANIFEST_SCHEMA
        || manifest.channel != CHANNEL
        || manifest.target != TARGET
        || manifest.bundle_identifier != BUNDLE_IDENTIFIER
        || manifest.tag != expected_tag
        || candidate.tag != expected_tag
        || candidate.version != version
        || manifest.dmg_name != expected_dmg
        || candidate.dmg.name != expected_dmg
        || manifest.dmg_size != candidate.dmg.size
        || manifest.dmg_size == 0
        || manifest.dmg_size > MAX_DMG_BYTES
        || version <= *current
        || minimum_macos > *running_macos
    {
        return Err(String::from(
            "Signed update manifest is not a newer compatible preview for this application.",
        ));
    }
    let dmg_sha256 = decode_sha256(&manifest.dmg_sha256)?;
    let dmg_url = validate_release_asset_url(&candidate.dmg.browser_download_url)?;
    Ok(VerifiedUpdate {
        version,
        tag: expected_tag,
        dmg_name: expected_dmg,
        dmg_url,
        dmg_size: manifest.dmg_size,
        dmg_sha256,
        minimum_macos,
    })
}

fn decode_sha256(text: &str) -> Result<[u8; 32], String> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(String::from(
            "DMG SHA-256 must be exactly 64 hexadecimal characters.",
        ));
    }
    let mut output = [0_u8; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(output)
}

fn hex_nibble(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(String::from("Invalid hexadecimal digit.")),
    }
}

trait Transport: Send {
    fn get_bounded(
        &self,
        url: &str,
        maximum: usize,
        cancellation: &AtomicBool,
    ) -> Result<Vec<u8>, String>;

    fn download_verified(
        &self,
        update: &VerifiedUpdate,
        destination: &Path,
        cancellation: &AtomicBool,
    ) -> Result<(), String>;
}

struct HttpTransport {
    agent: ureq::Agent,
}

impl HttpTransport {
    fn new() -> Self {
        let tls = ureq::native_tls::TlsConnector::new()
            .expect("initialize the macOS native TLS connector");
        let agent = ureq::AgentBuilder::new()
            .tls_connector(Arc::new(tls))
            .redirects(0)
            .user_agent(USER_AGENT)
            .timeout(NETWORK_TIMEOUT)
            .max_idle_connections(2)
            .max_idle_connections_per_host(1)
            .build();
        Self { agent }
    }

    fn response(&self, initial: &Url, timeout: Duration) -> Result<ureq::Response, String> {
        let mut current = initial.clone();
        for redirect in 0..=MAX_REDIRECTS {
            validate_http_url(&current, redirect > 0)?;
            let response = self
                .agent
                .get(current.as_str())
                .set("Accept", "application/vnd.github+json")
                .set("Accept-Encoding", "identity")
                .timeout(timeout)
                .call()
                .map_err(|error| format!("Update request failed: {error}"))?;
            let status = response.status();
            if (300..400).contains(&status) {
                if redirect == MAX_REDIRECTS {
                    return Err(String::from("Update request exceeded the redirect bound."));
                }
                let location = response
                    .header("location")
                    .ok_or_else(|| String::from("Update redirect omitted its location."))?
                    .to_owned();
                current = current
                    .join(&location)
                    .map_err(|error| format!("Update redirect is invalid: {error}"))?;
                continue;
            }
            if status != 200 {
                return Err(format!("Update request returned HTTP {status}."));
            }
            return Ok(response);
        }
        Err(String::from("Update request exceeded the redirect bound."))
    }
}

impl Transport for HttpTransport {
    fn get_bounded(
        &self,
        url: &str,
        maximum: usize,
        cancellation: &AtomicBool,
    ) -> Result<Vec<u8>, String> {
        check_cancelled(cancellation)?;
        let parsed = Url::parse(url).map_err(|error| format!("Update URL is invalid: {error}"))?;
        validate_http_url(&parsed, false)?;
        let response = self.response(&parsed, NETWORK_TIMEOUT)?;
        if let Some(length) = content_length(&response)?
            && length > maximum as u64
        {
            return Err(format!("Update response exceeds the {maximum}-byte bound."));
        }
        let mut reader = response.into_reader().take(maximum as u64 + 1);
        let mut bytes = Vec::with_capacity(maximum.min(16 * 1024));
        read_with_cancellation(&mut reader, &mut bytes, cancellation)?;
        if bytes.len() > maximum {
            return Err(format!("Update response exceeds the {maximum}-byte bound."));
        }
        Ok(bytes)
    }

    fn download_verified(
        &self,
        update: &VerifiedUpdate,
        destination: &Path,
        cancellation: &AtomicBool,
    ) -> Result<(), String> {
        if update.dmg_size == 0 || update.dmg_size > MAX_DMG_BYTES {
            return Err(String::from(
                "Signed DMG size is outside the download bound.",
            ));
        }
        check_cancelled(cancellation)?;
        let response = self.response(&update.dmg_url, DOWNLOAD_TIMEOUT)?;
        if let Some(length) = content_length(&response)?
            && length != update.dmg_size
        {
            return Err(String::from(
                "Downloaded DMG Content-Length does not match its manifest.",
            ));
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination)
            .map_err(|error| format!("Could not create private update download: {error}"))?;
        let mut reader = response.into_reader();
        let result = stream_and_verify(
            &mut reader,
            &mut file,
            update.dmg_size,
            &update.dmg_sha256,
            cancellation,
        )
        .and_then(|()| {
            file.sync_all()
                .map_err(|error| format!("Could not sync the verified DMG: {error}"))
        });
        if result.is_err() {
            let _ = fs::remove_file(destination);
        }
        result
    }
}

fn content_length(response: &ureq::Response) -> Result<Option<u64>, String> {
    response
        .header("content-length")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| String::from("Content-Length header is invalid."))
        })
        .transpose()
}

fn read_with_cancellation(
    reader: &mut dyn Read,
    output: &mut Vec<u8>,
    cancellation: &AtomicBool,
) -> Result<(), String> {
    let mut buffer = vec![0_u8; IO_BUFFER_BYTES];
    loop {
        check_cancelled(cancellation)?;
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("Could not read update response: {error}"))?;
        if count == 0 {
            return Ok(());
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

fn stream_and_verify(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    expected_size: u64,
    expected_hash: &[u8; 32],
    cancellation: &AtomicBool,
) -> Result<(), String> {
    let mut hasher = Sha256::new();
    let mut received = 0_u64;
    let mut buffer = vec![0_u8; IO_BUFFER_BYTES];
    loop {
        check_cancelled(cancellation)?;
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("Could not read downloaded DMG: {error}"))?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(count as u64)
            .ok_or_else(|| String::from("Downloaded DMG size overflowed."))?;
        if received > expected_size || received > MAX_DMG_BYTES {
            return Err(String::from(
                "Downloaded DMG contains trailing or excess bytes.",
            ));
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|error| format!("Could not write downloaded DMG: {error}"))?;
        hasher.update(&buffer[..count]);
    }
    if received != expected_size {
        return Err(String::from("Downloaded DMG ended before its signed size."));
    }
    let actual: [u8; 32] = hasher.finalize().into();
    if actual != *expected_hash {
        return Err(String::from(
            "Downloaded DMG SHA-256 did not match its manifest.",
        ));
    }
    Ok(())
}

fn validate_release_asset_url(text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|error| format!("Release asset URL is invalid: {error}"))?;
    validate_http_url(&url, false)?;
    if url.host_str() != Some("github.com")
        || !url
            .path()
            .starts_with("/joshwhiteley/czi-viewer/releases/download/")
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(String::from(
            "Release asset URL is outside the fixed repository.",
        ));
    }
    Ok(url)
}

fn validate_http_url(url: &Url, redirected: bool) -> Result<(), String> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.fragment().is_some()
    {
        return Err(String::from(
            "Update URL must be credential-free HTTPS on port 443.",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| String::from("Update URL has no host."))?;
    let allowed = match host {
        "api.github.com" | "github.com" => true,
        "release-assets.githubusercontent.com" | "objects.githubusercontent.com" => redirected,
        _ => false,
    };
    if !allowed {
        return Err(format!("Update URL host is not allowed: {host}"));
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UpdateState {
    last_automatic_attempt_unix: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UpdateReceipt {
    installed_version: String,
    backup_bundle: String,
}

fn default_state_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        String::from("Could not locate the macOS home directory for update state.")
    })?;
    if !home.is_absolute() {
        return Err(String::from(
            "The macOS home directory for update state is not absolute.",
        ));
    }
    Ok(home
        .join("Library")
        .join("Application Support")
        .join(PRODUCT)
        .join("update-state.json"))
}

fn default_receipt_path() -> Result<PathBuf, String> {
    Ok(default_state_path()?
        .parent()
        .ok_or_else(|| String::from("Update state path has no parent."))?
        .join("pending-update.json"))
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| String::from("System clock is earlier than the Unix epoch."))
}

fn automatic_check_is_due(path: &Path, now: u64) -> Result<bool, String> {
    let Some(previous) = load_attempt(path)? else {
        return Ok(true);
    };
    if previous > now {
        return Ok(false);
    }
    Ok(now - previous >= AUTO_INTERVAL_SECS)
}

fn create_automatic_claim(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

fn claim_automatic_attempt(path: &Path, now: u64) -> Result<bool, String> {
    let directory = path
        .parent()
        .ok_or_else(|| String::from("Update state path has no parent directory."))?;
    create_private_directory(directory)?;
    let claim = path.with_extension("automatic-check.lock");
    let claim_file = match create_automatic_claim(&claim) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&claim)
                .map_err(|error| format!("Could not inspect automatic update claim: {error}"))?;
            let stale = metadata.file_type().is_file()
                && metadata
                    .modified()
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age >= CLAIM_STALE_AFTER);
            if !stale {
                return Ok(false);
            }
            fs::remove_file(&claim)
                .map_err(|error| format!("Could not reclaim stale update claim: {error}"))?;
            create_automatic_claim(&claim)
                .map_err(|error| format!("Could not claim automatic update check: {error}"))?
        }
        Err(error) => return Err(format!("Could not claim automatic update check: {error}")),
    };
    let cleanup = FileGuard::new(claim.clone());
    claim_file
        .sync_all()
        .map_err(|error| format!("Could not sync automatic update claim: {error}"))?;
    if !automatic_check_is_due(path, now)? {
        return Ok(false);
    }
    save_attempt(path, now)?;
    drop(cleanup);
    sync_directory(directory)?;
    Ok(true)
}

fn load_attempt(path: &Path) -> Result<Option<u64>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not inspect update state: {error}")),
    };
    if !metadata.file_type().is_file() {
        return Err(String::from("Update state must be a regular file."));
    }
    if metadata.len() > STATE_BYTES {
        return Err(format!(
            "Update state exceeds the {STATE_BYTES}-byte bound."
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    File::open(path)
        .and_then(|file| file.take(STATE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("Could not read update state: {error}"))?;
    if bytes.len() as u64 > STATE_BYTES {
        return Err(format!(
            "Update state exceeds the {STATE_BYTES}-byte bound."
        ));
    }
    let state: UpdateState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Could not parse update state: {error}"))?;
    Ok(Some(state.last_automatic_attempt_unix))
}

fn save_attempt(path: &Path, timestamp: u64) -> Result<(), String> {
    let encoded = serde_json::to_vec(&UpdateState {
        last_automatic_attempt_unix: timestamp,
    })
    .map_err(|error| format!("Could not encode update state: {error}"))?;
    if encoded.len() as u64 > STATE_BYTES {
        return Err(String::from("Encoded update state exceeds its bound."));
    }
    let directory = path
        .parent()
        .ok_or_else(|| String::from("Update state path has no parent directory."))?;
    create_private_directory(directory)?;
    let temporary = unique_sibling(path, "state-tmp");
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("Could not create private update state: {error}"))?;
        file.write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("Could not write update state: {error}"))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not atomically replace update state: {error}"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not make update state private: {error}"))?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create private update directory: {error}"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect private update directory: {error}"))?;
    if !metadata.file_type().is_dir() {
        return Err(String::from("Private update directory is not a directory."));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("Could not protect private update directory: {error}"))
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not sync update directory: {error}"))
}

fn unique_sibling(path: &Path, label: &str) -> PathBuf {
    let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().unwrap_or_else(|| OsStr::new("update"));
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".{label}-{}-{sequence}", std::process::id()));
    path.with_file_name(temporary)
}

fn running_macos_version() -> Result<Version, String> {
    #[cfg(target_os = "macos")]
    {
        let output = system_command("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
            .map_err(|error| format!("Could not query macOS version: {error}"))?;
        if !output.status.success() || output.stdout.len() > 64 {
            return Err(String::from("Could not query a bounded macOS version."));
        }
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| String::from("macOS version is not UTF-8."))?
            .trim();
        normalize_os_version(text)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(String::from("Updates are supported only on macOS."))
    }
}

fn normalize_os_version(text: &str) -> Result<Version, String> {
    let components = text.split('.').count();
    let normalized = match components {
        1 => format!("{text}.0.0"),
        2 => format!("{text}.0"),
        _ => text.to_owned(),
    };
    Version::parse(&normalized).map_err(|error| format!("macOS version is invalid: {error}"))
}

fn check_cancelled(cancellation: &AtomicBool) -> Result<(), String> {
    if cancellation.load(Ordering::Acquire) {
        Err(String::from("Update operation was cancelled."))
    } else {
        Ok(())
    }
}

fn system_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn system_status(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn bounded_status(
    command: &mut Command,
    timeout: Duration,
    cancellation: Option<&AtomicBool>,
) -> Result<ExitStatus, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start bounded system tool: {error}"))?;
    let start = std::time::Instant::now();
    loop {
        if cancellation.is_some_and(|token| token.load(Ordering::Acquire)) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(String::from("Update operation was cancelled."));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("Could not wait for bounded system tool: {error}"))?
        {
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(String::from("A macOS update tool exceeded its time bound."));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[derive(Debug)]
pub(crate) struct ApplyPlan {
    current_bundle: PathBuf,
    staged_bundle: PathBuf,
    backup_bundle: PathBuf,
    receipt_path: PathBuf,
    expected_version: Version,
    expected_minimum_macos: Version,
    handed_off: bool,
}

impl ApplyPlan {
    /// Spawn the rollback helper. The parent must close promptly after this succeeds.
    pub(crate) fn spawn_helper(mut self) -> Result<(), String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("Could not locate updater executable: {error}"))?;
        let mut command = Command::new(executable);
        command
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .arg(HELPER_FLAG)
            .arg(std::process::id().to_string())
            .arg(&self.current_bundle)
            .arg(&self.staged_bundle)
            .arg(&self.backup_bundle)
            .arg(self.expected_version.to_string())
            .arg(self.expected_minimum_macos.to_string())
            .arg(&self.receipt_path);
        command
            .spawn()
            .map_err(|error| format!("Could not start update replacement helper: {error}"))?;
        self.handed_off = true;
        Ok(())
    }
}

impl Drop for ApplyPlan {
    fn drop(&mut self) {
        if !self.handed_off {
            let _ = fs::remove_dir_all(&self.staged_bundle);
        }
    }
}

/// Dispatch the hidden rollback helper before the parent starts egui.
pub(crate) fn run_update_helper_if_requested() -> Result<bool, String> {
    let arguments: Vec<OsString> = std::env::args_os().collect();
    run_update_helper_args(&arguments)
}

fn run_update_helper_args(arguments: &[OsString]) -> Result<bool, String> {
    if arguments.get(1).and_then(|value| value.to_str()) != Some(HELPER_FLAG) {
        return Ok(false);
    }
    if arguments.len() != 9 {
        return Err(String::from(
            "Update helper received an invalid argument count.",
        ));
    }
    let parent = arguments[2]
        .to_str()
        .ok_or_else(|| String::from("Update helper parent PID is invalid."))?
        .parse::<u32>()
        .map_err(|_| String::from("Update helper parent PID is invalid."))?;
    let current = PathBuf::from(&arguments[3]);
    let staged = PathBuf::from(&arguments[4]);
    let backup = PathBuf::from(&arguments[5]);
    let expected = arguments[6]
        .to_str()
        .ok_or_else(|| String::from("Update helper version is invalid."))
        .and_then(|text| {
            Version::parse(text).map_err(|_| String::from("Update helper version is invalid."))
        })?;
    let expected_minimum = arguments[7]
        .to_str()
        .ok_or_else(|| String::from("Update helper minimum macOS version is invalid."))
        .and_then(normalize_os_version)?;
    let receipt = PathBuf::from(&arguments[8]);
    validate_apply_paths(&current, &staged, &backup)?;
    validate_receipt_path(&receipt)?;
    let _staged_cleanup = PathGuard::new(staged.clone());
    wait_for_parent(parent)?;
    if let Err(error) = validate_bundle(&staged, &expected, &expected_minimum) {
        let _ = request_app_launch(&current);
        return Err(error);
    }
    apply_replacement(&current, &staged, &backup, &receipt, &expected)?;
    Ok(true)
}

fn prepare_install(
    transport: &dyn Transport,
    update: &VerifiedUpdate,
    current_bundle: &Path,
    cancellation: &AtomicBool,
) -> Result<ApplyPlan, String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (transport, update, current_bundle, cancellation);
        return Err(String::from("DMG installation is supported only on macOS."));
    }
    #[cfg(target_os = "macos")]
    {
        validate_current_bundle_target(current_bundle)?;
        validate_bundle(
            current_bundle,
            &Version::parse(env!("CARGO_PKG_VERSION"))
                .map_err(|error| format!("Current application version is invalid: {error}"))?,
            &normalize_os_version("12.3")?,
        )?;
        let support = default_state_path()?
            .parent()
            .ok_or_else(|| String::from("Update support path has no parent."))?
            .join("Updates");
        create_private_directory(&support)?;
        let work = support.join(format!(
            "install-{}-{}",
            std::process::id(),
            PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&work)
            .map_err(|error| format!("Could not create private update workspace: {error}"))?;
        fs::set_permissions(&work, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect update workspace: {error}"))?;
        let _cleanup = WorkspaceGuard::new(work.clone());
        let dmg = work.join(&update.dmg_name);
        transport.download_verified(update, &dmg, cancellation)?;
        apply_quarantine(&dmg)?;
        check_cancelled(cancellation)?;
        let mount = work.join("mount");
        fs::create_dir(&mount)
            .map_err(|error| format!("Could not create DMG mount point: {error}"))?;
        let mut mounted = MountedDmg::attach(&dmg, &mount, cancellation)?;
        check_cancelled(cancellation)?;
        let source = mount.join(format!("{PRODUCT}.app"));
        validate_bundle(&source, &update.version, &update.minimum_macos)?;
        reject_extra_top_level_apps(&mount, &source)?;
        check_cancelled(cancellation)?;

        let staged = unique_sibling(current_bundle, "staged");
        let backup = unique_sibling(current_bundle, "backup");
        let receipt = default_receipt_path()?;
        ensure_absent(&staged)?;
        ensure_absent(&backup)?;
        ensure_absent(&receipt)?;
        let staging_cleanup = PathGuard::new(staged.clone());
        let mut copy_command = system_status("/usr/bin/ditto");
        copy_command.arg(&source).arg(&staged);
        let copy = bounded_status(&mut copy_command, COPY_TIMEOUT, Some(cancellation))
            .map_err(|error| format!("Could not stage verified application: {error}"))?;
        mounted.detach()?;
        check_cancelled(cancellation)?;
        if !copy.success() {
            let _ = fs::remove_dir_all(&staged);
            return Err(String::from("Could not stage verified application."));
        }
        apply_quarantine(&staged)?;
        validate_bundle(&staged, &update.version, &update.minimum_macos)?;
        check_cancelled(cancellation)?;
        staging_cleanup.keep();
        Ok(ApplyPlan {
            current_bundle: current_bundle.to_path_buf(),
            staged_bundle: staged,
            backup_bundle: backup,
            receipt_path: receipt,
            expected_version: update.version.clone(),
            expected_minimum_macos: update.minimum_macos.clone(),
            handed_off: false,
        })
    }
}

#[cfg(target_os = "macos")]
fn apply_quarantine(path: &Path) -> Result<(), String> {
    let timestamp = unix_now()?;
    let value = format!("0081;{timestamp:x};CZI Viewer;");
    let mut command = system_status("/usr/bin/xattr");
    command
        .args(["-w", "com.apple.quarantine", &value])
        .arg(path);
    let status = bounded_status(&mut command, SYSTEM_TOOL_TIMEOUT, None)
        .map_err(|error| format!("Could not preserve Gatekeeper quarantine: {error}"))?;
    if !status.success() {
        return Err(String::from(
            "Could not preserve Gatekeeper quarantine on the update.",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
struct MountedDmg {
    mount: PathBuf,
    attached: bool,
}

#[cfg(target_os = "macos")]
impl MountedDmg {
    fn attach(dmg: &Path, mount: &Path, cancellation: &AtomicBool) -> Result<Self, String> {
        let mut command = system_status("/usr/bin/hdiutil");
        command
            .args([
                "attach",
                "-quiet",
                "-readonly",
                "-nobrowse",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(mount)
            .arg(dmg);
        let status = bounded_status(&mut command, SYSTEM_TOOL_TIMEOUT, Some(cancellation))
            .map_err(|error| format!("Could not mount verified DMG: {error}"))?;
        if !status.success() {
            return Err(String::from("Could not mount verified DMG read-only."));
        }
        Ok(Self {
            mount: mount.to_path_buf(),
            attached: true,
        })
    }

    fn detach(&mut self) -> Result<(), String> {
        if !self.attached {
            return Ok(());
        }
        let mut command = system_status("/usr/bin/hdiutil");
        command.args(["detach", "-quiet"]).arg(&self.mount);
        let status = bounded_status(&mut command, SYSTEM_TOOL_TIMEOUT, None)
            .map_err(|error| format!("Could not detach verified DMG: {error}"))?;
        if !status.success() {
            return Err(String::from("Could not detach verified DMG."));
        }
        self.attached = false;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for MountedDmg {
    fn drop(&mut self) {
        let _ = self.detach();
    }
}

struct FileGuard {
    path: PathBuf,
}

impl FileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct PathGuard {
    path: PathBuf,
    remove: AtomicBool,
}

type WorkspaceGuard = PathGuard;

impl PathGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            remove: AtomicBool::new(true),
        }
    }

    fn keep(&self) {
        self.remove.store(false, Ordering::Release);
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        if self.remove.load(Ordering::Acquire) {
            let _ = fs::remove_dir_all(&self.path);
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn validate_current_bundle_target(path: &Path) -> Result<(), String> {
    if path != Path::new("/Applications/CZI Viewer.app") {
        return Err(String::from(
            "Automatic installation requires the current app at /Applications/CZI Viewer.app.",
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect current application bundle: {error}"))?;
    if !metadata.file_type().is_dir() {
        return Err(String::from(
            "Current application bundle must be a non-symlink directory.",
        ));
    }
    Ok(())
}

fn validate_bundle(
    path: &Path,
    expected_version: &Version,
    expected_minimum_macos: &Version,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect application bundle: {error}"))?;
    if !metadata.file_type().is_dir() {
        return Err(String::from(
            "Application bundle must be a non-symlink directory.",
        ));
    }
    reject_escaping_symlinks(path)?;
    #[cfg(target_os = "macos")]
    {
        let plist = path.join("Contents/Info.plist");
        require_plist_value(&plist, "CFBundleIdentifier", BUNDLE_IDENTIFIER)?;
        require_plist_value(&plist, "CFBundleExecutable", "czi-viewer")?;
        require_plist_value(&plist, "CFBundlePackageType", "APPL")?;
        require_plist_value(
            &plist,
            "CFBundleShortVersionString",
            &expected_version.to_string(),
        )?;
        require_plist_value(&plist, "CFBundleVersion", &expected_version.to_string())?;
        let minimum = command_text(
            system_command("/usr/bin/plutil")
                .args(["-extract", "LSMinimumSystemVersion", "raw", "-o", "-"])
                .arg(&plist),
            "Could not inspect application minimum macOS version.",
            64,
        )?;
        if normalize_os_version(minimum.trim())? != *expected_minimum_macos {
            return Err(String::from(
                "Application minimum macOS version differs from its signed manifest.",
            ));
        }
        let binary = path.join("Contents/MacOS/czi-viewer");
        let binary_metadata = fs::symlink_metadata(&binary)
            .map_err(|error| format!("Could not inspect application executable: {error}"))?;
        if !binary_metadata.file_type().is_file()
            || binary_metadata.permissions().mode() & 0o111 == 0
        {
            return Err(String::from(
                "Application executable must be a regular executable file.",
            ));
        }
        let architecture = command_text(
            system_command("/usr/bin/lipo").arg("-archs").arg(&binary),
            "Could not inspect application architecture.",
            64,
        )?;
        if architecture.trim() != "arm64" {
            return Err(String::from("Application executable is not exactly arm64."));
        }
        let mut command = system_status("/usr/bin/codesign");
        command.args(["--verify", "--deep", "--strict"]).arg(path);
        let status = bounded_status(&mut command, SYSTEM_TOOL_TIMEOUT, None)
            .map_err(|error| format!("Could not verify application signature: {error}"))?;
        if !status.success() {
            return Err(String::from("Application has an invalid code signature."));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (expected_version, expected_minimum_macos);
    Ok(())
}

#[cfg(target_os = "macos")]
fn require_plist_value(plist: &Path, key: &str, expected: &str) -> Result<(), String> {
    let value = command_text(
        system_command("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(plist),
        "Could not inspect application Info.plist.",
        512,
    )?;
    if value.trim() != expected {
        return Err(format!("Application Info.plist has an unexpected {key}."));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn command_text(command: &mut Command, failure: &str, maximum: usize) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{failure} {error}"))?;
    if !output.status.success() || output.stdout.len() > maximum {
        return Err(failure.to_owned());
    }
    String::from_utf8(output.stdout).map_err(|_| failure.to_owned())
}

fn reject_escaping_symlinks(root: &Path) -> Result<(), String> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("Could not resolve application bundle: {error}"))?;
    let mut pending = vec![root.to_path_buf()];
    let mut entries = 0_usize;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("Could not inspect application bundle entries: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("Could not inspect bundle entry: {error}"))?;
            entries += 1;
            if entries > 200_000 {
                return Err(String::from("Application bundle exceeds the entry bound."));
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("Could not inspect bundle entry: {error}"))?;
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path)
                    .map_err(|error| format!("Could not read bundle symlink: {error}"))?;
                if target.is_absolute()
                    || target.components().any(|part| part == Component::ParentDir)
                {
                    return Err(String::from(
                        "Application bundle contains an unsafe symlink.",
                    ));
                }
                let resolved = fs::canonicalize(&path)
                    .map_err(|error| format!("Could not resolve bundle symlink: {error}"))?;
                if !resolved.starts_with(&canonical_root) {
                    return Err(String::from(
                        "Application bundle symlink escapes the bundle.",
                    ));
                }
            } else if metadata.file_type().is_dir() {
                pending.push(path);
            } else if !metadata.file_type().is_file() {
                return Err(String::from("Application bundle contains a special file."));
            }
        }
    }
    Ok(())
}

fn reject_extra_top_level_apps(mount: &Path, expected: &Path) -> Result<(), String> {
    let mut apps = 0_usize;
    for entry in
        fs::read_dir(mount).map_err(|error| format!("Could not inspect mounted DMG: {error}"))?
    {
        let path = entry
            .map_err(|error| format!("Could not inspect mounted DMG entry: {error}"))?
            .path();
        if path.extension() == Some(OsStr::new("app")) {
            apps += 1;
            if path != expected {
                return Err(String::from(
                    "DMG contains an unexpected application bundle.",
                ));
            }
        }
    }
    if apps != 1 {
        return Err(String::from(
            "DMG must contain exactly one application bundle.",
        ));
    }
    Ok(())
}

fn ensure_absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(format!(
            "Update staging path already exists: {}",
            path.display()
        )),
        Err(error) => Err(format!("Could not inspect update staging path: {error}")),
    }
}

fn validate_apply_paths(current: &Path, staged: &Path, backup: &Path) -> Result<(), String> {
    validate_current_bundle_target(current)?;
    let parent = current
        .parent()
        .ok_or_else(|| String::from("Current application has no parent directory."))?;
    for path in [staged, backup] {
        if !path.is_absolute() || path.parent() != Some(parent) || path == current {
            return Err(String::from(
                "Update helper path is outside the application directory.",
            ));
        }
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| String::from("Update helper path name is invalid."))?;
        if !name.starts_with(".CZI Viewer.app.") || name.contains('/') {
            return Err(String::from("Update helper path name is invalid."));
        }
    }
    ensure_absent(backup)?;
    Ok(())
}

fn validate_receipt_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path.file_name() != Some(OsStr::new("pending-update.json"))
        || path
            .components()
            .any(|component| component == Component::ParentDir)
        || !path
            .to_string_lossy()
            .ends_with("/Library/Application Support/CZI Viewer/pending-update.json")
    {
        return Err(String::from("Update receipt path is invalid."));
    }
    Ok(())
}

fn wait_for_parent(parent: u32) -> Result<(), String> {
    let start = std::time::Instant::now();
    while start.elapsed() < HELPER_WAIT_LIMIT {
        let mut command = system_status("/bin/kill");
        command.args(["-0", &parent.to_string()]);
        let status = bounded_status(&mut command, Duration::from_secs(5), None)
            .map_err(|error| format!("Could not check parent process: {error}"))?;
        if !status.success() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(String::from(
        "Timed out waiting for the application to close.",
    ))
}

fn apply_replacement(
    current: &Path,
    staged: &Path,
    backup: &Path,
    receipt: &Path,
    installed_version: &Version,
) -> Result<(), String> {
    fs::rename(current, backup)
        .map_err(|error| format!("Could not preserve current application: {error}"))?;
    if let Err(error) = fs::rename(staged, current) {
        let rollback = fs::rename(backup, current);
        return match rollback {
            Ok(()) => {
                let _ = request_app_launch(current);
                Err(format!(
                    "Could not install update; restored current application: {error}"
                ))
            }
            Err(rollback_error) => Err(format!(
                "Could not install update ({error}) or restore current application ({rollback_error})."
            )),
        };
    }
    if let Err(error) = save_update_receipt(receipt, backup, installed_version) {
        let failed_update = unique_sibling(current, "failed");
        let rollback =
            fs::rename(current, &failed_update).and_then(|()| fs::rename(backup, current));
        return match rollback {
            Ok(()) => {
                let _ = fs::remove_dir_all(&failed_update);
                let _ = request_app_launch(current);
                Err(format!(
                    "Could not record update recovery state; restored the previous application: {error}"
                ))
            }
            Err(rollback_error) => {
                let restore_update = fs::rename(&failed_update, current);
                Err(match restore_update {
                    Ok(()) => format!(
                        "Could not record update recovery state ({error}) or restore the previous application ({rollback_error}); preserved the verified update."
                    ),
                    Err(restore_error) => format!(
                        "Could not record update recovery state ({error}), restore the previous application ({rollback_error}), or restore the verified update ({restore_error}); both bundles were preserved."
                    ),
                })
            }
        };
    }
    let launch = request_app_launch(current).map_err(io::Error::other);
    match launch {
        // LaunchServices acceptance is not a startup health acknowledgement. Retain the backup
        // for recovery until a future parent integration can acknowledge a healthy new launch.
        Ok(status) if status.success() => Ok(()),
        failed => {
            let failed_update = unique_sibling(current, "failed");
            let rollback =
                fs::rename(current, &failed_update).and_then(|()| fs::rename(backup, current));
            let _ = fs::remove_file(receipt);
            match rollback {
                Ok(()) => {
                    let _ = fs::remove_dir_all(&failed_update);
                    let _ = request_app_launch(current);
                    match failed {
                        Ok(_) => Err(String::from(
                            "macOS rejected the relaunch request; restored the previous application.",
                        )),
                        Err(error) => Err(format!(
                            "Could not relaunch the update; restored the previous application: {error}"
                        )),
                    }
                }
                Err(error) => {
                    let restore_update = fs::rename(&failed_update, current);
                    Err(match restore_update {
                        Ok(()) => format!(
                            "Could not relaunch the update or restore the previous application ({error}); preserved the verified update."
                        ),
                        Err(restore_error) => format!(
                            "Could not relaunch the update, restore the previous application ({error}), or restore the verified update ({restore_error}); both bundles were preserved."
                        ),
                    })
                }
            }
        }
    }
}

fn request_app_launch(path: &Path) -> Result<ExitStatus, String> {
    let mut command = system_status("/usr/bin/open");
    command.arg(path);
    bounded_status(&mut command, SYSTEM_TOOL_TIMEOUT, None)
        .map_err(|error| format!("Could not ask macOS to launch the application: {error}"))
}

fn save_update_receipt(
    path: &Path,
    backup: &Path,
    installed_version: &Version,
) -> Result<(), String> {
    validate_receipt_path(path)?;
    let backup_text = backup
        .to_str()
        .ok_or_else(|| String::from("Application backup path is not valid UTF-8."))?;
    let encoded = serde_json::to_vec(&UpdateReceipt {
        installed_version: installed_version.to_string(),
        backup_bundle: backup_text.to_owned(),
    })
    .map_err(|error| format!("Could not encode update receipt: {error}"))?;
    if encoded.len() as u64 > RECEIPT_BYTES {
        return Err(String::from("Update receipt exceeds its bound."));
    }
    let directory = path
        .parent()
        .ok_or_else(|| String::from("Update receipt path has no parent."))?;
    create_private_directory(directory)?;
    ensure_absent(path)?;
    let temporary = unique_sibling(path, "receipt-tmp");
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("Could not create private update receipt: {error}"))?;
        file.write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("Could not write update receipt: {error}"))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not atomically save update receipt: {error}"))?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn load_update_receipt(path: &Path) -> Result<Option<UpdateReceipt>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not inspect update receipt: {error}")),
    };
    if !metadata.file_type().is_file() || metadata.len() > RECEIPT_BYTES {
        return Err(String::from(
            "Update receipt is not a bounded regular file.",
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    File::open(path)
        .and_then(|file| file.take(RECEIPT_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("Could not read update receipt: {error}"))?;
    if bytes.len() as u64 > RECEIPT_BYTES {
        return Err(String::from("Update receipt exceeds its bound."));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Could not parse update receipt: {error}"))
}

#[cfg(target_os = "macos")]
fn cleanup_backups_after_successful_start() -> Result<(), String> {
    let Some(current) = current_application_bundle() else {
        return Ok(());
    };
    let receipt_path = default_receipt_path()?;
    let Some(receipt) = load_update_receipt(&receipt_path)? else {
        return Ok(());
    };
    let version = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("Current application version is invalid: {error}"))?;
    if receipt.installed_version != version.to_string() {
        return Err(String::from(
            "Update receipt does not acknowledge the running version.",
        ));
    }
    validate_bundle(&current, &version, &normalize_os_version("12.3")?)?;
    let backup = PathBuf::from(receipt.backup_bundle);
    let parent = current
        .parent()
        .ok_or_else(|| String::from("Current application has no parent directory."))?;
    let name = backup
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| String::from("Application backup name is invalid."))?;
    if !backup.is_absolute()
        || backup.parent() != Some(parent)
        || !name.starts_with(".CZI Viewer.app.backup-")
    {
        return Err(String::from(
            "Update receipt contains an invalid backup path.",
        ));
    }
    match fs::symlink_metadata(&backup) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::remove_dir_all(&backup).map_err(|error| {
                format!("Could not remove acknowledged application backup: {error}")
            })?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        _ => {
            return Err(String::from(
                "Acknowledged application backup is not a directory.",
            ));
        }
    }
    fs::remove_file(&receipt_path)
        .map_err(|error| format!("Could not remove consumed update receipt: {error}"))?;
    sync_directory(
        receipt_path
            .parent()
            .ok_or_else(|| String::from("Update receipt path has no parent."))?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const TEST_SECRET: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];

    fn test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "czi-update-{name}-{}-{}",
            std::process::id(),
            PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("test directory");
        path
    }

    fn manifest(version: &str, hash: &str, size: u64) -> Vec<u8> {
        serde_json::to_vec(&SignedManifest {
            schema: MANIFEST_SCHEMA,
            channel: CHANNEL.to_owned(),
            version: version.to_owned(),
            tag: format!("preview-v{version}"),
            target: TARGET.to_owned(),
            minimum_macos: String::from("12.3.0"),
            bundle_identifier: BUNDLE_IDENTIFIER.to_owned(),
            dmg_name: format!("CZI-Viewer-{version}-{TARGET}-preview.dmg"),
            dmg_size: size,
            dmg_sha256: hash.to_owned(),
        })
        .expect("manifest")
    }

    fn candidate(version: &str, size: u64) -> ReleaseCandidate {
        let name = format!("CZI-Viewer-{version}-{TARGET}-preview.dmg");
        ReleaseCandidate {
            version: Version::parse(version).expect("version"),
            tag: format!("preview-v{version}"),
            manifest_url: Url::parse("https://github.com/joshwhiteley/czi-viewer/releases/download/preview-v9.9.9/update.json").expect("url"),
            signature_url: Url::parse("https://github.com/joshwhiteley/czi-viewer/releases/download/preview-v9.9.9/update.sig").expect("url"),
            dmg: GithubAsset {
                name,
                browser_download_url: format!("https://github.com/joshwhiteley/czi-viewer/releases/download/preview-v{version}/CZI-Viewer-{version}-{TARGET}-preview.dmg"),
                size,
            },
        }
    }

    fn sign(bytes: &[u8]) -> ([u8; 32], [u8; 64]) {
        let signing = SigningKey::from_bytes(&TEST_SECRET);
        let signature = signing.sign(bytes).to_bytes();
        (signing.verifying_key().to_bytes(), signature)
    }

    #[test]
    fn release_signer_and_embedded_key_accept_the_canonical_schema() {
        let bytes = b"{\"bundle_identifier\":\"io.github.joshwhiteley.czi-viewer\",\"channel\":\"preview\",\"dmg_name\":\"CZI-Viewer-9.9.9-aarch64-apple-darwin-preview.dmg\",\"dmg_sha256\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"dmg_size\":123,\"minimum_macos\":\"12.3\",\"schema\":1,\"tag\":\"preview-v9.9.9\",\"target\":\"aarch64-apple-darwin\",\"version\":\"9.9.9\"}\n";
        let signature = decode_signature_hex(
            "7bcd787bc99aa0caaff715d73801908db39b7a4599a919d79df32b9c67fc7978860c68b154c9a0e5546cc0ceecb82073dd2e78bb4499fa7d07e22ff189f80500",
        );
        let update = verify_manifest(
            bytes,
            &signature,
            &candidate("9.9.9", 123),
            &Version::parse("0.1.2").expect("current version"),
            &Version::parse("99.0.0").expect("macOS version"),
            &PUBLIC_KEY,
        )
        .expect("release signer and embedded public key must agree");
        assert_eq!(update.version(), &Version::parse("9.9.9").unwrap());
    }

    #[test]
    #[ignore = "contacts the fixed public GitHub Releases endpoint"]
    fn live_github_update_check_uses_system_tls() {
        let cancellation = AtomicBool::new(false);
        check_for_update(&HttpTransport::new(), &cancellation)
            .expect("fixed GitHub update endpoint should be reachable");
    }

    fn decode_signature_hex(text: &str) -> [u8; 64] {
        let mut signature = [0_u8; 64];
        for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
            signature[index] = (hex_nibble(pair[0]).unwrap() << 4) | hex_nibble(pair[1]).unwrap();
        }
        signature
    }

    #[test]
    fn embedded_public_key_matches_requested_hex() {
        assert_eq!(
            PUBLIC_KEY,
            decode_sha256("b3f32b2a26c334f6956ec73a572b14223f54f1c8811a9a659ce2d9cf87d0be3c")
                .expect("key hex")
        );
    }

    #[test]
    fn exact_manifest_signature_and_compatibility_are_required() {
        let bytes = manifest(
            "0.1.3",
            "0000000000000000000000000000000000000000000000000000000000000000",
            10,
        );
        let (key, signature) = sign(&bytes);
        let update = verify_manifest(
            &bytes,
            &signature,
            &candidate("0.1.3", 10),
            &Version::parse("0.1.2").expect("current"),
            &Version::parse("14.0.0").expect("macos"),
            &key,
        )
        .expect("verified");
        assert_eq!(update.version(), &Version::parse("0.1.3").expect("version"));

        let mut changed = bytes.clone();
        changed[0] ^= 1;
        assert!(
            verify_manifest(
                &changed,
                &signature,
                &candidate("0.1.3", 10),
                &Version::parse("0.1.2").expect("current"),
                &Version::parse("14.0.0").expect("macos"),
                &key,
            )
            .is_err()
        );
        assert!(
            verify_manifest(
                &bytes,
                &signature[..63],
                &candidate("0.1.3", 10),
                &Version::parse("0.1.2").expect("current"),
                &Version::parse("14.0.0").expect("macos"),
                &key,
            )
            .is_err()
        );
    }

    #[test]
    fn manifest_rejects_unknown_fields_downgrade_and_newer_macos() {
        let mut bytes = manifest(
            "0.1.3",
            "0000000000000000000000000000000000000000000000000000000000000000",
            10,
        );
        bytes.pop();
        bytes.extend_from_slice(b",\"extra\":true}");
        let (key, signature) = sign(&bytes);
        assert!(
            verify_manifest(
                &bytes,
                &signature,
                &candidate("0.1.3", 10),
                &Version::parse("0.1.2").expect("current"),
                &Version::parse("14.0.0").expect("macos"),
                &key,
            )
            .is_err()
        );

        let old = manifest(
            "0.1.2",
            "0000000000000000000000000000000000000000000000000000000000000000",
            10,
        );
        let (key, signature) = sign(&old);
        assert!(
            verify_manifest(
                &old,
                &signature,
                &candidate("0.1.2", 10),
                &Version::parse("0.1.2").expect("current"),
                &Version::parse("14.0.0").expect("macos"),
                &key,
            )
            .is_err()
        );

        let bytes = manifest(
            "0.1.3",
            "0000000000000000000000000000000000000000000000000000000000000000",
            10,
        );
        let (key, signature) = sign(&bytes);
        assert!(
            verify_manifest(
                &bytes,
                &signature,
                &candidate("0.1.3", 10),
                &Version::parse("0.1.2").expect("current"),
                &Version::parse("11.0.0").expect("macos"),
                &key,
            )
            .is_err()
        );
    }

    #[test]
    fn cadence_is_private_atomic_and_manual_policy_is_separate() {
        let root = test_dir("state");
        let path = root.join("Application Support/CZI Viewer/update-state.json");
        assert!(automatic_check_is_due(&path, 100_000).expect("first due"));
        save_attempt(&path, 100_000).expect("save");
        assert!(!automatic_check_is_due(&path, 100_000 + AUTO_INTERVAL_SECS - 1).expect("not due"));
        assert!(automatic_check_is_due(&path, 100_000 + AUTO_INTERVAL_SECS).expect("due"));
        assert!(!automatic_check_is_due(&path, 99_999).expect("future suppressed"));
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().expect("parent"))
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn state_rejects_symlinks_corruption_and_oversize() {
        let root = test_dir("bad-state");
        let path = root.join("state.json");
        fs::write(&path, b"not-json").expect("corrupt");
        assert!(automatic_check_is_due(&path, 1).is_err());
        fs::write(
            &path,
            vec![b'x'; usize::try_from(STATE_BYTES).expect("bound") + 1],
        )
        .expect("oversize");
        assert!(automatic_check_is_due(&path, 1).is_err());
        fs::remove_file(&path).expect("remove");
        std::os::unix::fs::symlink(root.join("missing"), &path).expect("symlink");
        assert!(automatic_check_is_due(&path, 1).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn update_receipt_is_private_bounded_and_names_one_exact_backup() {
        let root = test_dir("receipt");
        let receipt = root
            .join("Library/Application Support/CZI Viewer")
            .join("pending-update.json");
        let backup = Path::new("/Applications/.CZI Viewer.app.backup-123-1");
        let version = Version::parse("1.2.3").unwrap();
        save_update_receipt(&receipt, backup, &version).expect("save receipt");
        let loaded = load_update_receipt(&receipt)
            .expect("load receipt")
            .expect("receipt");
        assert_eq!(loaded.installed_version, "1.2.3");
        assert_eq!(loaded.backup_bundle, backup.to_str().unwrap());
        assert_eq!(
            fs::metadata(&receipt).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::write(&receipt, vec![b'x'; RECEIPT_BYTES as usize + 1]).unwrap();
        assert!(
            load_update_receipt(&receipt)
                .unwrap_err()
                .contains("bounded")
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn urls_are_fixed_https_and_redirect_hosts_are_narrow() {
        assert!(
            validate_release_asset_url(
                "https://github.com/joshwhiteley/czi-viewer/releases/download/preview-v0.1.3/file"
            )
            .is_ok()
        );
        assert!(validate_release_asset_url("https://evil.example/file").is_err());
        assert!(
            validate_http_url(
                &Url::parse("https://release-assets.githubusercontent.com/file").expect("url"),
                true
            )
            .is_ok()
        );
        assert!(
            validate_http_url(
                &Url::parse("https://release-assets.githubusercontent.com/file").expect("url"),
                false
            )
            .is_err()
        );
        assert!(
            validate_http_url(&Url::parse("http://github.com/file").expect("url"), true).is_err()
        );
        assert!(
            validate_http_url(
                &Url::parse("https://user@github.com/file").expect("url"),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn streaming_enforces_exact_size_hash_and_cancellation() {
        let data = b"verified dmg bytes";
        let hash: [u8; 32] = Sha256::digest(data).into();
        let cancellation = AtomicBool::new(false);
        let mut output = Vec::new();
        stream_and_verify(
            &mut data.as_slice(),
            &mut output,
            data.len() as u64,
            &hash,
            &cancellation,
        )
        .expect("verified stream");
        assert_eq!(output, data);
        assert!(
            stream_and_verify(
                &mut data.as_slice(),
                &mut Vec::new(),
                data.len() as u64 - 1,
                &hash,
                &cancellation,
            )
            .is_err()
        );
        cancellation.store(true, Ordering::Release);
        assert!(
            stream_and_verify(
                &mut data.as_slice(),
                &mut Vec::new(),
                data.len() as u64,
                &hash,
                &cancellation,
            )
            .is_err()
        );
    }

    struct FakeTransport {
        responses: Mutex<VecDeque<(String, Vec<u8>)>>,
        requests: Mutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new(responses: Vec<(String, Vec<u8>)>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl Transport for FakeTransport {
        fn get_bounded(
            &self,
            url: &str,
            maximum: usize,
            cancellation: &AtomicBool,
        ) -> Result<Vec<u8>, String> {
            check_cancelled(cancellation)?;
            self.requests.lock().expect("requests").push(url.to_owned());
            let (expected, bytes) = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .ok_or_else(|| String::from("unexpected request"))?;
            if expected != url || bytes.len() > maximum {
                return Err(String::from("fake request mismatch or bound exceeded"));
            }
            Ok(bytes)
        }

        fn download_verified(
            &self,
            _update: &VerifiedUpdate,
            _destination: &Path,
            _cancellation: &AtomicBool,
        ) -> Result<(), String> {
            Err(String::from("not used"))
        }
    }

    #[test]
    fn fake_transport_observes_only_fixed_discovery_and_asset_urls() {
        let version = "0.1.3";
        let stem = format!("CZI-Viewer-{version}-{TARGET}-preview");
        let manifest_url = format!(
            "https://github.com/joshwhiteley/czi-viewer/releases/download/preview-v{version}/{stem}-update.json"
        );
        let signature_url = format!("{manifest_url}.sig");
        let dmg_url = format!(
            "https://github.com/joshwhiteley/czi-viewer/releases/download/preview-v{version}/{stem}.dmg"
        );
        let manifest = manifest(
            version,
            "0000000000000000000000000000000000000000000000000000000000000000",
            10,
        );
        let (_key, signature) = sign(&manifest);
        // The embedded production key intentionally does not match this test signature, so the
        // check safely returns no update after making only the expected fixed requests.
        let releases = serde_json::json!([{
            "tag_name": format!("preview-v{version}"),
            "draft": false,
            "prerelease": true,
            "assets": [
                {"name": format!("{stem}-update.json"), "browser_download_url": manifest_url, "size": manifest.len()},
                {"name": format!("{stem}-update.json.sig"), "browser_download_url": signature_url, "size": 64},
                {"name": format!("{stem}.dmg"), "browser_download_url": dmg_url, "size": 10}
            ]
        }]);
        let transport = FakeTransport::new(vec![
            (
                RELEASES_URL.to_owned(),
                serde_json::to_vec(&releases).expect("releases"),
            ),
            (manifest_url.clone(), manifest),
            (signature_url.clone(), signature.to_vec()),
        ]);
        let cancellation = AtomicBool::new(false);
        let result = check_for_update(&transport, &cancellation).expect("check");
        assert!(result.is_none());
        assert_eq!(
            transport.requests.lock().expect("requests").as_slice(),
            [RELEASES_URL, manifest_url.as_str(), signature_url.as_str()]
        );
    }

    #[test]
    fn automatic_attempt_is_persisted_before_failure_and_manual_bypasses_cadence() {
        struct FailingTransport {
            calls: Arc<AtomicU64>,
        }

        impl Transport for FailingTransport {
            fn get_bounded(
                &self,
                _url: &str,
                _maximum: usize,
                _cancellation: &AtomicBool,
            ) -> Result<Vec<u8>, String> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Err(String::from("offline"))
            }

            fn download_verified(
                &self,
                _update: &VerifiedUpdate,
                _destination: &Path,
                _cancellation: &AtomicBool,
            ) -> Result<(), String> {
                Err(String::from("not used"))
            }
        }

        let root = test_dir("worker-cadence");
        let state = root.join("support/update-state.json");
        let calls = Arc::new(AtomicU64::new(0));
        let worker = UpdateWorker::spawn_with(
            Box::new(FailingTransport {
                calls: Arc::clone(&calls),
            }),
            Ok(state.clone()),
        );
        for _ in 0..100 {
            if calls.load(Ordering::Relaxed) == 1 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(load_attempt(&state).expect("saved attempt").is_some());
        assert!(
            worker.try_recv().is_none(),
            "automatic failure must be silent"
        );

        worker.check_now().expect("manual check");
        let mut manual_error = false;
        for _ in 0..100 {
            if matches!(worker.try_recv(), Some(UpdateEvent::ManualError(_))) {
                manual_error = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(manual_error);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        drop(worker);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn bundle_walk_rejects_escaping_symlinks_and_special_top_level_apps() {
        let root = test_dir("bundle-links");
        let bundle = root.join("CZI Viewer.app");
        fs::create_dir(&bundle).expect("bundle");
        fs::write(bundle.join("inside"), b"ok").expect("inside");
        std::os::unix::fs::symlink("inside", bundle.join("safe-link")).expect("safe link");
        assert!(reject_escaping_symlinks(&bundle).is_ok());
        std::os::unix::fs::symlink("../outside", bundle.join("unsafe-link")).expect("unsafe link");
        assert!(reject_escaping_symlinks(&bundle).is_err());

        let other = root.join("Other.app");
        fs::create_dir(&other).expect("other app");
        assert!(reject_extra_top_level_apps(&root, &bundle).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn helper_dispatch_ignores_normal_arguments_and_rejects_malformed_helper_mode() {
        assert!(!run_update_helper_args(&[OsString::from("viewer")]).expect("normal mode"));
        assert!(
            run_update_helper_args(&[OsString::from("viewer"), OsString::from(HELPER_FLAG),])
                .is_err()
        );
    }

    #[test]
    fn release_filter_rejects_duplicates_and_non_preview_releases() {
        let version = "0.1.3";
        let stem = format!("CZI-Viewer-{version}-{TARGET}-preview");
        let asset = |name: String| GithubAsset {
            browser_download_url: format!(
                "https://github.com/joshwhiteley/czi-viewer/releases/download/preview-v{version}/{name}"
            ),
            name,
            size: 64,
        };
        let assets = vec![
            asset(format!("{stem}-update.json")),
            asset(format!("{stem}-update.json.sig")),
            asset(format!("{stem}.dmg")),
        ];
        let stable = GithubRelease {
            tag_name: format!("preview-v{version}"),
            draft: false,
            prerelease: false,
            assets: assets.clone(),
        };
        assert!(
            release_candidates(vec![stable], &Version::parse("0.1.2").expect("current"))
                .expect("filter")
                .is_empty()
        );

        let mut duplicates = assets.clone();
        duplicates.push(asset(format!("{stem}.dmg")));
        let duplicate_release = GithubRelease {
            tag_name: format!("preview-v{version}"),
            draft: false,
            prerelease: true,
            assets: duplicates,
        };
        assert!(
            release_candidates(
                vec![duplicate_release],
                &Version::parse("0.1.2").expect("current")
            )
            .is_err()
        );
    }
}
