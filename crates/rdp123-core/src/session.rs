//! The RDP session engine.
//!
//! A session runs on its own OS thread with a single-threaded Tokio runtime.
//! It drives the IronRDP connect sequence, then an active loop that decodes
//! server graphics into the shared framebuffer and forwards input, resize and
//! clipboard traffic. It talks to the UI through a command channel (in) and an
//! event callback (out); it never touches AppKit.

#![allow(clippy::too_many_arguments)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;
use std::{io, net::IpAddr};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::oneshot;
use zeroize::Zeroize;

use ironrdp::cliprdr::backend::CliprdrBackend;
use ironrdp::cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags,
    FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
    FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp::cliprdr::{Client, CliprdrClient, CliprdrSvcMessages};
use ironrdp::connector::connection_activation::{
    ConnectionActivationFactory, ConnectionActivationSequence, ConnectionActivationState,
};
use ironrdp::connector::sspi::{generator::NetworkRequest, ErrorKind as SspiErrorKind};
use ironrdp::connector::{
    BitmapConfig, ClientConnector, Config, ConnectionResult, ConnectorError, ConnectorErrorExt,
    ConnectorErrorKind, ConnectorResult, Credentials, DesktopSize, ServerName,
};
use ironrdp::core::WriteBuf;
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::input::{
    synchronize_event, Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations,
};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::geometry::Rectangle as _;
use ironrdp::pdu::input::fast_path::FastPathInputEvent;
use ironrdp::pdu::rdp::capability_sets::{client_codecs_capabilities, MajorPlatformType};
use ironrdp::pdu::rdp::client_info::{CompressionType, PerformanceFlags, TimezoneInfo};
use ironrdp::pdu::PduResult;
use ironrdp::rdpdr::{NoopRdpdrBackend, Rdpdr};
use ironrdp::rdpsnd::client::Rdpsnd;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::{
    connect_begin, connect_finalize, mark_as_upgraded, single_sequence_step_read,
    split_tokio_framed, FramedWrite as _, NetworkClient, TokioFramed,
};

use crate::framebuffer::SharedFramebuffer;
use crate::gfx::GfxEvent;
use crate::keymap;
use crate::profile::{AudioMode, AuthenticationMode, ClipboardMode, GraphicsMode};
use crate::rdsaad::RdsAadClient;

/// The pixel format shared by the decoded image, the framebuffer and CoreGraphics.
const PIXEL_FORMAT: PixelFormat = PixelFormat::BgrX32;

/// How long to let the writer flush a graceful shutdown before giving up.
const SHUTDOWN_FLUSH: std::time::Duration = std::time::Duration::from_millis(500);

/// `Disconnected` reason for a normal server-side session end (logoff). The UI
/// treats this as expected and shows no error dialog.
pub const REMOTE_ENDED: &str = "the remote session ended";

type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;
type SessionFramed = TokioFramed<TlsStream>;
type SessionReader = TokioFramed<tokio::io::ReadHalf<TlsStream>>;
type SessionWriter = TokioFramed<tokio::io::WriteHalf<TlsStream>>;
type OutSender = Sender<Vec<u8>>;
type EventCb = Box<dyn Fn(SessionEvent) + Send>;
const COMMAND_QUEUE_CAPACITY: usize = 256;
const CLIPBOARD_QUEUE_CAPACITY: usize = 32;
const OUTPUT_QUEUE_CAPACITY: usize = 64;
const MAX_CLIPBOARD_TEXT_BYTES: usize = 16 * 1024 * 1024;
const CLIPBOARD_RETRY_DELAYS: [Duration; 6] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
/// IronRDP requires callers to drive CLIPRDR lock and request timeouts.
const CLIPBOARD_TIMEOUT_TICK: Duration = Duration::from_secs(5);
/// Stop reserving the dictated clipboard if the remote application never
/// requests its data after the injected Ctrl+V.
const EXTERNAL_PASTE_DATA_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on the number of files offered to the remote clipboard in one copy
/// (folders are walked recursively; a runaway selection is truncated).
const MAX_CLIPBOARD_FILES: usize = 4096;
/// Cap on a single FileContents chunk we are willing to serve.
const MAX_FILE_CHUNK_BYTES: u32 = 16 * 1024 * 1024;
/// Keep local file reads off the RDP decode loop while bounding disk work
/// across all open sessions. Responses wait for queue capacity instead of
/// being dropped when Windows pipelines multiple requests.
static LOCAL_FILE_READ_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
/// Idle keep-alive: how long a session must be free of real user input before
/// an invisible F15 tap is injected. 30 s sits well under the one-minute
/// minimum of a Windows idle-session policy, so the keep-alive always wins the
/// race against the timeout (and the remote lock screen).
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// Set-1 scancode for F15 (not extended). No mainstream application reacts to
/// it, so the injected tap resets the remote idle timer with no visible effect.
const KEEP_ALIVE_SCANCODE: u8 = 0x66;
/// Set-1 scancodes used to insert clipboard text into Windows after CLIPRDR
/// confirms that the remote side accepted the matching clipboard generation.
const LEFT_CTRL_SCANCODE: u8 = 0x1d;
const V_SCANCODE: u8 = 0x2f;

/// What the local (macOS) clipboard currently offers to the remote session.
#[derive(Debug, Default)]
enum LocalClip {
    #[default]
    Empty,
    Text(String),
    Files(Vec<LocalClipFile>),
}

#[derive(Debug)]
enum LocalClipboardOffer {
    Text(Vec<ClipboardFormat>),
    Files(Vec<FileDescriptor>),
}

/// Clipboard contents plus the acknowledgement state for the current CLIPRDR
/// connection. Windows may transiently reject a FormatList, so an offer is not
/// considered delivered until its matching response is `Ok`.
#[derive(Debug, Default)]
struct LocalClipboardState {
    clip: LocalClip,
    /// Latest ordinary macOS clipboard update observed while a confirmed STT
    /// paste still needs the dictated text to remain active remotely.
    deferred_clip: Option<LocalClip>,
    generation: u64,
    /// CLIPRDR FormatListResponse carries no correlation id. Track only the
    /// newest offer: Windows can omit the initialization response and reply
    /// only after a later format list replaces it.
    in_flight: Option<u64>,
    /// File offers require a ready CLIPRDR channel, while IronRDP requires the
    /// first (initialization) offer to use `initiate_copy`. Keep those phases
    /// separate so a file already on the macOS clipboard cannot abort setup.
    ready: bool,
    bootstrap_in_flight: bool,
    accepted_generation: Option<u64>,
    retry_attempts: usize,
    pending_paste_generation: Option<u64>,
    serving_paste_generation: Option<u64>,
    requested_paste_generation: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalClipboardOfferResult {
    None,
    Retry { generation: u64, delay: Duration },
    RetryInitialization { delay: Duration },
    PasteReady { generation: u64 },
    AdvertiseCurrent { generation: u64 },
}

impl LocalClipboardState {
    fn replace(&mut self, clip: LocalClip) -> u64 {
        if self.protects_current_paste() {
            self.deferred_clip = Some(clip);
            return self.generation;
        }
        self.deferred_clip = None;
        self.replace_now(clip)
    }

    fn replace_now(&mut self, clip: LocalClip) -> u64 {
        self.clip = clip;
        self.generation = self.generation.wrapping_add(1);
        self.accepted_generation = None;
        self.retry_attempts = 0;
        self.pending_paste_generation = None;
        self.serving_paste_generation = None;
        self.requested_paste_generation = None;
        self.generation
    }

    fn replace_for_paste(&mut self, text: String) -> u64 {
        // An explicit Paste action must remain repeatable even when the new STT
        // result is byte-for-byte identical to the previous one.
        self.pending_paste_generation = None;
        let generation = self.replace_now(LocalClip::Text(text));
        self.pending_paste_generation = Some(generation);
        generation
    }

    fn reset_connection(&mut self) {
        self.in_flight = None;
        self.ready = false;
        self.bootstrap_in_flight = false;
        self.accepted_generation = None;
        self.retry_attempts = 0;
        self.pending_paste_generation = None;
        self.serving_paste_generation = None;
        self.requested_paste_generation = None;
        if let Some(clip) = self.deferred_clip.take() {
            self.replace_now(clip);
        }
    }

    /// Build the mandatory first FormatList. Text can be tracked as the real
    /// clipboard offer immediately; files need a harmless text bootstrap and
    /// are advertised only after IronRDP reports the channel ready.
    fn begin_initial_offer(&mut self) -> Option<Vec<ClipboardFormat>> {
        if self.ready || self.bootstrap_in_flight || self.in_flight.is_some() {
            return None;
        }
        if matches!(self.clip, LocalClip::Files(_)) {
            self.bootstrap_in_flight = true;
        } else {
            self.in_flight = Some(self.generation);
        }
        Some(vec![ClipboardFormat::new(
            ClipboardFormatId::CF_UNICODETEXT,
        )])
    }

    fn mark_ready(&mut self) {
        self.ready = true;
    }

    fn is_ready(&self) -> bool {
        self.ready
    }

    fn begin_offer(&mut self, generation: u64) -> Option<LocalClipboardOffer> {
        if generation != self.generation
            || self.accepted_generation == Some(generation)
            || self.in_flight == Some(generation)
        {
            return None;
        }
        let offer = match &self.clip {
            LocalClip::Files(files) => LocalClipboardOffer::Files(to_file_descriptors(files)),
            LocalClip::Empty | LocalClip::Text(_) => {
                LocalClipboardOffer::Text(vec![ClipboardFormat::new(
                    ClipboardFormatId::CF_UNICODETEXT,
                )])
            }
        };
        self.in_flight = Some(generation);
        Some(offer)
    }

    fn complete_offer(&mut self, ok: bool) -> LocalClipboardOfferResult {
        if self.bootstrap_in_flight {
            self.bootstrap_in_flight = false;
            if ok {
                self.retry_attempts = 0;
                return LocalClipboardOfferResult::AdvertiseCurrent {
                    generation: self.generation,
                };
            }
            let Some(delay) = CLIPBOARD_RETRY_DELAYS.get(self.retry_attempts).copied() else {
                tracing::warn!(
                    "clipboard: Windows repeatedly rejected CLIPRDR initialization; \
                     clipboard redirection is unavailable for this connection"
                );
                return LocalClipboardOfferResult::None;
            };
            self.retry_attempts += 1;
            return LocalClipboardOfferResult::RetryInitialization { delay };
        }
        let Some(generation) = self.in_flight.take() else {
            tracing::warn!("clipboard: received a FormatList response with no offer in flight");
            return LocalClipboardOfferResult::None;
        };
        if generation != self.generation {
            return LocalClipboardOfferResult::AdvertiseCurrent {
                generation: self.generation,
            };
        }
        if ok {
            self.accepted_generation = Some(generation);
            self.retry_attempts = 0;
            if self.pending_paste_generation == Some(generation) {
                return LocalClipboardOfferResult::PasteReady { generation };
            }
            return LocalClipboardOfferResult::None;
        }
        let Some(delay) = CLIPBOARD_RETRY_DELAYS.get(self.retry_attempts).copied() else {
            tracing::warn!(
                "clipboard: Windows repeatedly rejected the current clipboard offer; \
                 waiting for the next local clipboard change"
            );
            self.pending_paste_generation = None;
            if let Some(generation) = self.apply_deferred_clip() {
                return LocalClipboardOfferResult::AdvertiseCurrent { generation };
            }
            return LocalClipboardOfferResult::None;
        };
        self.retry_attempts += 1;
        LocalClipboardOfferResult::Retry { generation, delay }
    }

    fn consume_confirmed_paste(&mut self, generation: u64) -> bool {
        let confirmed = self.generation == generation
            && self.accepted_generation == Some(generation)
            && self.pending_paste_generation == Some(generation);
        if confirmed {
            self.pending_paste_generation = None;
            self.serving_paste_generation = Some(generation);
            self.requested_paste_generation = None;
        }
        confirmed
    }

    fn protects_current_paste(&self) -> bool {
        let current = Some(self.generation);
        self.pending_paste_generation == current || self.serving_paste_generation == current
    }

    fn begin_paste_data_response(&mut self) -> Option<u64> {
        let generation =
            (self.serving_paste_generation == Some(self.generation)).then_some(self.generation)?;
        self.requested_paste_generation = Some(generation);
        Some(generation)
    }

    fn finish_serving_paste(&mut self, generation: u64) -> Option<u64> {
        if self.serving_paste_generation != Some(generation) || self.generation != generation {
            return None;
        }
        self.serving_paste_generation = None;
        self.requested_paste_generation = None;
        self.apply_deferred_clip()
    }

    fn expire_unrequested_paste(&mut self, generation: u64) -> Option<u64> {
        if self.requested_paste_generation == Some(generation) {
            return None;
        }
        self.finish_serving_paste(generation)
    }

    fn apply_deferred_clip(&mut self) -> Option<u64> {
        let clip = self.deferred_clip.take()?;
        Some(self.replace_now(clip))
    }
}

/// One local file (or directory) offered to the remote clipboard.
#[derive(Debug, Clone)]
struct LocalClipFile {
    /// Absolute path on disk.
    path: std::path::PathBuf,
    /// Wire name relative to the copied selection, `\`-separated.
    wire_name: String,
    size: u64,
    is_dir: bool,
}

type LocalClipState = Arc<Mutex<LocalClipboardState>>;

/// Walk the copied selection into the flat, relative-path list the Windows
/// clipboard expects. Unreadable entries and non-UTF-8 names are skipped.
fn collect_clipboard_files(roots: &[std::path::PathBuf]) -> Vec<LocalClipFile> {
    fn visit(path: &std::path::Path, wire_name: String, out: &mut Vec<LocalClipFile>) {
        if out.len() >= MAX_CLIPBOARD_FILES {
            return;
        }
        let Ok(metadata) = std::fs::metadata(path) else {
            tracing::warn!("clipboard: skipping unreadable {}", path.display());
            return;
        };
        if metadata.is_dir() {
            out.push(LocalClipFile {
                path: path.to_path_buf(),
                wire_name: wire_name.clone(),
                size: 0,
                is_dir: true,
            });
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let child = entry.path();
                let Some(name) = child.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                visit(&child, format!("{wire_name}\\{name}"), out);
            }
        } else if metadata.is_file() {
            out.push(LocalClipFile {
                path: path.to_path_buf(),
                wire_name,
                size: metadata.len(),
                is_dir: false,
            });
        }
    }

    let mut out = Vec::new();
    for root in roots {
        let Some(name) = root.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        visit(root, name.to_string(), &mut out);
    }
    if out.len() >= MAX_CLIPBOARD_FILES {
        tracing::warn!("clipboard: selection truncated at {MAX_CLIPBOARD_FILES} files");
    }
    out
}

/// Build the CLIPRDR descriptors for the offered files (wire name is split
/// into `relative_path` + `name` as the encoder expects).
fn to_file_descriptors(files: &[LocalClipFile]) -> Vec<FileDescriptor> {
    files
        .iter()
        .map(|file| {
            let (relative_path, name) = match file.wire_name.rsplit_once('\\') {
                Some((path, name)) => (Some(path), name),
                None => (None, file.wire_name.as_str()),
            };
            let mut descriptor = FileDescriptor::new(name);
            if let Some(path) = relative_path {
                descriptor = descriptor.with_relative_path(path);
            }
            if file.is_dir {
                descriptor = descriptor.with_attributes(ClipboardFileAttributes::DIRECTORY);
            } else {
                descriptor = descriptor
                    .with_attributes(ClipboardFileAttributes::NORMAL)
                    .with_file_size(file.size);
            }
            descriptor
        })
        .collect()
}

/// Chunk size for pulling remote files (one outstanding request at a time).
const FETCH_CHUNK_BYTES: u32 = 1024 * 1024;

/// A file entry on the remote clipboard (from `FileGroupDescriptorW`).
#[derive(Debug, Clone)]
struct RemoteFileEntry {
    /// `\`-separated name relative to the copied selection.
    wire_name: String,
    size: Option<u64>,
    is_dir: bool,
}

#[derive(Debug)]
struct PlannedEntry {
    /// Index into the remote file list (CLIPRDR `lindex`).
    index: i32,
    dest: std::path::PathBuf,
    size: Option<u64>,
    is_dir: bool,
}

#[derive(Debug)]
struct CurrentFetchFile {
    file: std::fs::File,
    index: i32,
    /// Total size; not yet known while a SIZE request is outstanding.
    size: u64,
    needs_size: bool,
    offset: u64,
}

/// One remote clipboard offer being materialized for Finder.
#[derive(Debug)]
struct FetchJob {
    queue: std::collections::VecDeque<PlannedEntry>,
    current: Option<CurrentFetchFile>,
    cache_dir: std::path::PathBuf,
    top_level_paths: Vec<std::path::PathBuf>,
    keep_cache: bool,
}

impl Drop for FetchJob {
    fn drop(&mut self) {
        if !self.keep_cache {
            let _ = std::fs::remove_dir_all(&self.cache_dir);
        }
    }
}

/// State of the remote clipboard's file offer and the fetch pipeline.
#[derive(Debug)]
struct RemoteClipboard {
    files: Vec<RemoteFileEntry>,
    data_id: Option<u32>,
    jobs: std::collections::VecDeque<FetchJob>,
    next_stream_id: u32,
    /// Outstanding request: (stream id, was a SIZE request, requested bytes).
    outstanding: Option<(u32, bool, u32)>,
}

impl Default for RemoteClipboard {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            data_id: None,
            jobs: std::collections::VecDeque::new(),
            // Some Windows clipboard owners treat zero as an uninitialized
            // correlation ID even though the field is formally an unsigned
            // integer. Start at one and skip zero after wrapping.
            next_stream_id: 1,
            outstanding: None,
        }
    }
}

#[derive(Debug)]
enum RemoteFetchAction {
    Idle,
    Request(FileContentsRequest),
    Ready(Vec<std::path::PathBuf>),
}

/// Reject a server response that exceeds either the requested range or the
/// remaining size advertised for the remote file.
fn validate_remote_file_range(
    requested_size: u32,
    remaining_size: u64,
    response_len: usize,
) -> Result<(), String> {
    let response_len = u64::try_from(response_len)
        .map_err(|_| "remote file response length does not fit in u64".to_string())?;
    if response_len > u64::from(requested_size) {
        return Err(format!(
            "remote returned {response_len} bytes for a {requested_size}-byte request"
        ));
    }
    if response_len > remaining_size {
        return Err(format!(
            "remote returned {response_len} bytes with only {remaining_size} bytes remaining"
        ));
    }
    Ok(())
}

/// Plan one top-level item and its descendants into the local cache. Wire
/// names containing `..` components are rejected so a hostile server cannot
/// escape the cache directory.
fn plan_fetch_entries(
    remote: &RemoteClipboard,
    name: &str,
    dest: &std::path::Path,
) -> Result<std::collections::VecDeque<PlannedEntry>, String> {
    let prefix = format!("{name}\\");
    let mut queue = std::collections::VecDeque::new();
    let mut found_root = false;
    for (index, entry) in remote.files.iter().enumerate() {
        let relative: Option<std::path::PathBuf> = if entry.wire_name == name {
            found_root = true;
            Some(std::path::PathBuf::new())
        } else {
            entry
                .wire_name
                .strip_prefix(&prefix)
                .map(|sub| sub.split('\\').collect::<std::path::PathBuf>())
        };
        let Some(relative) = relative else { continue };
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
            && !relative.as_os_str().is_empty()
        {
            tracing::warn!(
                "clipboard: skipping suspicious remote path {:?}",
                entry.wire_name
            );
            continue;
        }
        let dest_path = if relative.as_os_str().is_empty() {
            dest.to_path_buf()
        } else {
            dest.join(relative)
        };
        queue.push_back(PlannedEntry {
            index: i32::try_from(index).unwrap_or(i32::MAX),
            dest: dest_path,
            size: entry.size,
            is_dir: entry.is_dir,
        });
    }
    if !found_root {
        return Err(format!("'{name}' is no longer on the remote clipboard"));
    }
    Ok(queue)
}

fn create_remote_clipboard_cache_dir() -> Result<std::path::PathBuf, String> {
    let root = std::env::temp_dir().join("RDP123").join("Clipboard");
    std::fs::create_dir_all(&root).map_err(|error| format!("creating clipboard cache: {error}"))?;
    let cache_dir = root.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&cache_dir)
        .map_err(|error| format!("creating clipboard cache: {error}"))?;
    Ok(cache_dir)
}

fn remote_top_level_destination(
    cache_dir: &std::path::Path,
    name: &str,
) -> Result<std::path::PathBuf, String> {
    let path = std::path::Path::new(name);
    let mut components = path.components();
    let is_single_normal_component = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    if !is_single_normal_component {
        return Err(format!("invalid remote clipboard file name: '{name}'"));
    }
    Ok(cache_dir.join(path))
}

/// Build a single cache job for the whole copied selection. Publishing only
/// after this job completes keeps Finder from seeing partially downloaded
/// files or folders.
fn plan_remote_clipboard_cache(
    remote: &RemoteClipboard,
    names: &[String],
    cache_dir: std::path::PathBuf,
) -> Result<FetchJob, String> {
    let mut queue = std::collections::VecDeque::new();
    let mut top_level_paths = Vec::with_capacity(names.len());
    for name in names {
        let dest = match remote_top_level_destination(&cache_dir, name) {
            Ok(dest) => dest,
            Err(reason) => {
                let _ = std::fs::remove_dir_all(&cache_dir);
                return Err(reason);
            }
        };
        let entries = match plan_fetch_entries(remote, name, &dest) {
            Ok(entries) => entries,
            Err(reason) => {
                let _ = std::fs::remove_dir_all(&cache_dir);
                return Err(reason);
            }
        };
        queue.extend(entries);
        top_level_paths.push(dest);
    }
    Ok(FetchJob {
        queue,
        current: None,
        cache_dir,
        top_level_paths,
        keep_cache: false,
    })
}

/// Drive the fetch pipeline until it needs the next FileContents response
/// (or all jobs are drained). Creates directories and files locally and
/// issues at most one outstanding request.
async fn advance_remote_fetch(
    remote: &mut RemoteClipboard,
    active_stage: &mut ActiveStage,
    out_tx: &OutSender,
    event_cb: &EventCb,
) -> Result<(), String> {
    loop {
        match next_remote_fetch_action(remote)? {
            RemoteFetchAction::Idle => return Ok(()),
            RemoteFetchAction::Ready(paths) => {
                event_cb(SessionEvent::ClipboardFilesReady(paths));
            }
            RemoteFetchAction::Request(request) => {
                return send_cliprdr(active_stage, out_tx, |c| c.request_file_contents(request))
                    .await
                    .map_err(|error| format!("requesting remote file data: {error:#}"));
            }
        }
    }
}

/// Advance local cache preparation until network input is needed. Keeping the
/// state transition separate from the RDP transport makes the full file
/// download path deterministic and testable.
fn next_remote_fetch_action(remote: &mut RemoteClipboard) -> Result<RemoteFetchAction, String> {
    loop {
        if remote.outstanding.is_some() {
            return Ok(RemoteFetchAction::Idle);
        }
        let data_id = remote.data_id;
        let Some(job) = remote.jobs.front_mut() else {
            return Ok(RemoteFetchAction::Idle);
        };

        if let Some(current) = &job.current {
            let (flags, requested, was_size) = if current.needs_size {
                (FileContentsFlags::SIZE, 8, true)
            } else {
                let remaining = current.size.saturating_sub(current.offset);
                let chunk = u32::try_from(remaining.min(u64::from(FETCH_CHUNK_BYTES)))
                    .unwrap_or(FETCH_CHUNK_BYTES);
                (FileContentsFlags::RANGE, chunk, false)
            };
            let stream_id = remote.next_stream_id.max(1);
            remote.next_stream_id = stream_id.wrapping_add(1).max(1);
            let request = FileContentsRequest {
                stream_id,
                index: current.index,
                flags,
                position: current.offset,
                requested_size: requested,
                data_id,
            };
            remote.outstanding = Some((stream_id, was_size, requested));
            return Ok(RemoteFetchAction::Request(request));
        }

        match job.queue.pop_front() {
            Some(entry) if entry.is_dir => {
                std::fs::create_dir_all(&entry.dest)
                    .map_err(|e| format!("creating {}: {e}", entry.dest.display()))?;
            }
            Some(entry) => {
                if let Some(parent) = entry.dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("creating {}: {e}", parent.display()))?;
                }
                let file = std::fs::File::create(&entry.dest)
                    .map_err(|e| format!("creating {}: {e}", entry.dest.display()))?;
                if entry.size == Some(0) {
                    continue; // empty file, nothing to fetch
                }
                job.current = Some(CurrentFetchFile {
                    file,
                    index: entry.index,
                    size: entry.size.unwrap_or(0),
                    needs_size: entry.size.is_none(),
                    offset: 0,
                });
            }
            None => {
                let Some(mut job) = remote.jobs.pop_front() else {
                    return Ok(RemoteFetchAction::Idle);
                };
                job.keep_cache = true;
                return Ok(RemoteFetchAction::Ready(job.top_level_paths.clone()));
            }
        }
    }
}

fn fail_front_job(remote: &mut RemoteClipboard, reason: String, event_cb: &EventCb) {
    remote.outstanding = None;
    if remote.jobs.pop_front().is_some() {
        tracing::warn!("clipboard: file fetch failed: {reason}");
        event_cb(SessionEvent::ClipboardFilesFailed(reason));
    }
}

/// Apply one FileContents response to the fetch pipeline.
async fn handle_remote_file_contents(
    remote: &mut RemoteClipboard,
    stream_id: u32,
    data: Option<Vec<u8>>,
    active_stage: &mut ActiveStage,
    out_tx: &OutSender,
    event_cb: &EventCb,
) {
    if let Err(reason) = apply_remote_file_contents(remote, stream_id, data) {
        fail_front_job(remote, reason, event_cb);
        return;
    }
    if let Err(reason) = advance_remote_fetch(remote, active_stage, out_tx, event_cb).await {
        fail_front_job(remote, reason, event_cb);
    }
}

fn apply_remote_file_contents(
    remote: &mut RemoteClipboard,
    stream_id: u32,
    data: Option<Vec<u8>>,
) -> Result<(), String> {
    use std::io::Write as _;

    let Some((expected_id, was_size, requested_size)) = remote.outstanding else {
        return Ok(()); // stale response after a failed/cancelled job
    };
    if stream_id != expected_id {
        return Ok(());
    }
    remote.outstanding = None;

    let bytes = data.ok_or("the remote refused the transfer")?;
    let job = remote.jobs.front_mut().ok_or("no active transfer")?;
    let current = job.current.as_mut().ok_or("no file in progress")?;
    if was_size {
        let size: [u8; 8] = bytes
            .get(..8)
            .and_then(|b| b.try_into().ok())
            .ok_or("malformed size response")?;
        current.size = u64::from_le_bytes(size);
        current.needs_size = false;
        if current.size == 0 {
            job.current = None;
        }
    } else {
        if bytes.is_empty() {
            return Err("transfer ended early".to_string());
        }
        validate_remote_file_range(
            requested_size,
            current.size.saturating_sub(current.offset),
            bytes.len(),
        )?;
        current
            .file
            .write_all(&bytes)
            .map_err(|e| format!("writing file: {e}"))?;
        current.offset += bytes.len() as u64;
        if current.offset >= current.size {
            job.current = None;
        }
    }
    Ok(())
}

/// A logical mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

/// UI-originated input, already translated into remote pixel coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key {
        keycode: u16,
        down: bool,
    },
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: PointerButton,
        down: bool,
        x: u16,
        y: u16,
    },
    Wheel {
        delta: i16,
        horizontal: bool,
    },
}

/// Commands the UI sends into a running session.
#[derive(Debug)]
pub enum SessionCommand {
    Input(Vec<InputEvent>),
    Resize {
        width: u16,
        height: u16,
        scale: Option<u32>,
    },
    LocalClipboard(String),
    /// Synchronize text from an external macOS STT tool, then paste it at the
    /// active remote Windows caret once CLIPRDR acknowledges the offer.
    PasteLocalClipboard(String),
    /// The user copied files in Finder; offer them to the remote clipboard.
    LocalClipboardFiles(Vec<std::path::PathBuf>),
    ReleaseAllKeys,
    Shutdown,
}

/// Events the session emits back to the UI. Delivered on the session thread —
/// the UI callback is responsible for hopping to the main thread.
pub enum SessionEvent {
    Connected {
        width: u16,
        height: u16,
    },
    FrameUpdated {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Resized {
        width: u16,
        height: u16,
    },
    /// A new pointer shape from the server, decoded to straight-alpha RGBA.
    /// Coordinates are in remote pixels.
    PointerBitmap {
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        rgba: Vec<u8>,
    },
    /// The server asks for the default (arrow) pointer.
    PointerDefault,
    /// The server hides the pointer (e.g. touch input or full-screen video).
    PointerHidden,
    ClipboardText(String),
    /// Remote files are being materialized in a private local cache before
    /// Finder receives real file URLs.
    ClipboardFilesPreparing {
        count: usize,
    },
    /// Fully downloaded top-level files ready to publish on the macOS
    /// pasteboard. Paths remain valid independently of the RDP session.
    ClipboardFilesReady(Vec<std::path::PathBuf>),
    /// Preparing the remote clipboard files failed; no partial selection is
    /// published to Finder.
    ClipboardFilesFailed(String),
    /// Ask the user to trust a server key fingerprint. `is_change` is true when
    /// a *different* key was previously pinned. Reply true to proceed.
    CertificateApproval {
        fingerprint: String,
        is_change: bool,
        reply: oneshot::Sender<bool>,
    },
    /// Open the Microsoft login page and return its final OAuth redirect URL.
    EntraSignIn {
        authorization_url: String,
        redirect_uri: String,
        reply: oneshot::Sender<std::result::Result<String, String>>,
    },
    /// The user accepted `fingerprint`; the app should persist it.
    CertTrusted {
        fingerprint: String,
    },
    /// The connection dropped and another reconnect attempt is scheduled.
    Reconnecting {
        attempt: u32,
        max_attempts: u32,
    },
    /// Automatic reconnect was enabled, but all attempts have failed.
    ReconnectFailed {
        reason: String,
    },
    Disconnected {
        reason: String,
    },
    Error(String),
}

/// Everything needed to open a connection. The password is held only for the
/// duration of the connect; callers should source it from the Keychain.
pub struct SessionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    pub authentication: AuthenticationMode,
    pub width: u16,
    pub height: u16,
    pub scale: Option<u32>,
    pub expected_fingerprint: Option<String>,
    // --- RDP options ---
    pub color_depth: u32,
    /// FastPath transport compression (RDP 6.1 XCRUSH). EGFX/ZGFX and graphics
    /// codec compression remain protocol-managed independently.
    pub compression: bool,
    pub clipboard: ClipboardMode,
    /// Where remote audio plays (local playback, discarded, or left remote).
    pub audio: AudioMode,
    /// Graphics pipeline: EGFX (H.264/RemoteFX Progressive) or legacy bitmaps.
    pub graphics: GraphicsMode,
    /// EGFX only: offer AVC444. When false the server falls back to AVC420.
    pub avc444: bool,
    /// When false, the remote stays at a fixed resolution (window resizes just scale it).
    pub dynamic_resolution: bool,
    pub reconnect: bool,
    pub reconnect_per_minute: u32,
    /// Global setting: ⌘ sends Alt and ⌥ sends the Windows key.
    pub swap_cmd_alt: bool,
    /// Wake-on-LAN MAC address; a magic packet is broadcast before connecting.
    pub wake_mac: Option<String>,
    /// Keep the remote session awake: while the user is idle, tap an invisible
    /// F15 every [`KEEP_ALIVE_INTERVAL`] so idle-disconnect policies and the
    /// remote lock screen never trigger.
    pub keep_alive: bool,
}

impl Drop for SessionConfig {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

/// Handle to a running session held by the UI.
#[derive(Clone)]
pub struct SessionHandle {
    command_tx: Sender<SessionCommand>,
    framebuffer: Arc<SharedFramebuffer>,
    pending: Arc<PendingCommands>,
}

#[derive(Debug, Default)]
struct PendingValue<T> {
    value: Option<T>,
    queued: bool,
}

#[derive(Debug, Default)]
struct PendingCommands {
    mouse_move: Mutex<PendingValue<(u16, u16)>>,
    resize: Mutex<PendingValue<(u16, u16, Option<u32>)>>,
    release_all_keys: AtomicBool,
    shutdown: AtomicBool,
}

impl SessionHandle {
    /// Send a command; ignored if the session has already ended.
    pub fn command(&self, cmd: SessionCommand) {
        let _ = self.try_command(cmd);
    }

    /// Try to queue a command. Clipboard polling uses the return value to keep
    /// an unsent pasteboard change pending instead of silently losing it.
    pub fn try_command(&self, cmd: SessionCommand) -> bool {
        if let SessionCommand::Input(events) = &cmd {
            if let [InputEvent::MouseMove { x, y }] = events.as_slice() {
                return self.queue_mouse_move(*x, *y);
            }
        }
        match cmd {
            SessionCommand::Resize {
                width,
                height,
                scale,
            } => self.queue_resize(width, height, scale),
            SessionCommand::ReleaseAllKeys => {
                self.pending.release_all_keys.store(true, Ordering::Release);
                let _ = self.command_tx.try_send(SessionCommand::ReleaseAllKeys);
                true
            }
            SessionCommand::Shutdown => {
                self.pending.shutdown.store(true, Ordering::Release);
                let _ = self.command_tx.try_send(SessionCommand::Shutdown);
                true
            }
            command => {
                if let Err(error) = self.command_tx.try_send(command) {
                    tracing::warn!("session command queue is full or closed: {error}");
                    false
                } else {
                    true
                }
            }
        }
    }

    pub fn framebuffer(&self) -> Arc<SharedFramebuffer> {
        self.framebuffer.clone()
    }

    fn queue_mouse_move(&self, x: u16, y: u16) -> bool {
        let should_signal = {
            let mut pending = self.pending.mouse_move.lock().unwrap();
            pending.value = Some((x, y));
            if pending.queued {
                false
            } else {
                pending.queued = true;
                true
            }
        };
        if !should_signal {
            return true;
        }
        let queued = self
            .command_tx
            .try_send(SessionCommand::Input(vec![InputEvent::MouseMove { x, y }]))
            .is_ok();
        if !queued {
            self.pending.mouse_move.lock().unwrap().queued = false;
        }
        queued
    }

    fn queue_resize(&self, width: u16, height: u16, scale: Option<u32>) -> bool {
        let should_signal = {
            let mut pending = self.pending.resize.lock().unwrap();
            pending.value = Some((width, height, scale));
            if pending.queued {
                false
            } else {
                pending.queued = true;
                true
            }
        };
        if !should_signal {
            return true;
        }
        let queued = self
            .command_tx
            .try_send(SessionCommand::Resize {
                width,
                height,
                scale,
            })
            .is_ok();
        if !queued {
            self.pending.resize.lock().unwrap().queued = false;
        }
        queued
    }
}

/// Start a session on a dedicated thread and return a handle immediately.
pub fn spawn(config: SessionConfig, event_cb: EventCb) -> SessionHandle {
    let framebuffer = SharedFramebuffer::new();
    let (command_tx, command_rx) = channel(COMMAND_QUEUE_CAPACITY);
    let fb = framebuffer.clone();
    let pending = Arc::new(PendingCommands::default());
    let thread_pending = pending.clone();

    std::thread::Builder::new()
        .name("rdp-session".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    event_cb(SessionEvent::Error(format!("runtime: {e}")));
                    return;
                }
            };
            rt.block_on(run(config, fb, command_rx, thread_pending, event_cb));
        })
        .expect("spawn session thread");

    SessionHandle {
        command_tx,
        framebuffer,
        pending,
    }
}

/// Signals produced by the clipboard backend, drained by the session loop.
enum ClipSignal {
    /// Send the mandatory non-file FormatList that completes CLIPRDR setup.
    InitializeClipboard,
    /// Advertise the current local clipboard generation. Stale retries are
    /// ignored when a newer macOS copy has already replaced it.
    AdvertiseLocal {
        generation: u64,
    },
    /// Windows accepted the clipboard text prepared for an external STT
    /// insertion, so it is now safe to inject remote Ctrl+V.
    PasteAccepted {
        generation: u64,
    },
    /// Windows did not request the dictated clipboard data after Ctrl+V.
    PasteDataTimedOut {
        generation: u64,
    },
    /// Serve one FileContents chunk or size to the remote.
    SubmitFileContents(FileContentsResponse<'static>),
    /// The remote clipboard's file list arrived (parsed FileGroupDescriptorW).
    RemoteFileList {
        files: Vec<RemoteFileEntry>,
        data_id: Option<u32>,
    },
    /// One FileContents response for our fetch pipeline (`None` = error).
    RemoteFileContents {
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    InitiatePaste(ClipboardFormatId),
    SubmitData {
        response: OwnedFormatDataResponse,
        paste_generation: Option<u64>,
    },
    RemoteText(String),
}

/// Give up after this many consecutive failed reconnect attempts.
const MAX_RECONNECT_FAILURES: u32 = 20;
/// Attempts for the very first connect. A few retries let a just-granted macOS
/// Local Network permission (or a transient blip) succeed instead of failing.
const MAX_INITIAL_FAILURES: u32 = 4;
/// Initial attempts when Wake-on-LAN is configured: the host may need to boot
/// or resume, which takes far longer than a permission blip.
const MAX_INITIAL_FAILURES_WOL: u32 = 20;
/// Delay between initial-connect retries.
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Bound the TCP connect so a Local-Network-blocked LAN connect fails fast with
/// a clear message rather than hanging on the long OS default.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

/// Why a single session attempt ended.
enum SessionEnd {
    /// User closed the window (or dropped the handle) — do not reconnect.
    UserQuit,
    /// Connection dropped — reconnect if enabled.
    Disconnected(String),
}

/// What the reconnect loop should do after one attempt.
enum Outcome {
    Stop,
    Fail {
        reason: String,
        event: TerminalEvent,
    },
    /// Wait `delay`, then try again. A reconnect attempt is announced when
    /// `reconnect_attempt` is present; initial-connect retries stay silent.
    Retry {
        delay: Duration,
        reconnect_attempt: Option<u32>,
    },
}

#[derive(Clone, Copy)]
enum TerminalEvent {
    Error,
    Disconnected,
    ReconnectFailed,
}

enum ConnectFailure {
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

impl ConnectFailure {
    fn retryable(error: impl Into<anyhow::Error>) -> Self {
        Self::Retryable(error.into())
    }

    fn fatal(error: impl Into<anyhow::Error>) -> Self {
        Self::Fatal(error.into())
    }

    fn into_parts(self) -> (anyhow::Error, bool) {
        match self {
            Self::Retryable(error) => (error, true),
            Self::Fatal(error) => (error, false),
        }
    }
}

/// Delay between reconnect attempts, derived from the per-minute cap.
fn reconnect_delay(config: &SessionConfig) -> Duration {
    let secs = (60 / u64::from(config.reconnect_per_minute.max(1))).max(1);
    Duration::from_secs(secs)
}

async fn run(
    mut config: SessionConfig,
    framebuffer: Arc<SharedFramebuffer>,
    mut command_rx: Receiver<SessionCommand>,
    pending: Arc<PendingCommands>,
    event_cb: EventCb,
) {
    ensure_crypto_provider();
    let mut rdsaad_client = if config.authentication == AuthenticationMode::EntraWeb {
        match RdsAadClient::new(&config.host) {
            Ok(client) => Some(client),
            Err(error) => {
                event_cb(SessionEvent::Error(format!("{error:#}")));
                return;
            }
        }
    } else {
        None
    };
    let local_clip: LocalClipState = Arc::new(Mutex::new(LocalClipboardState::default()));
    // One audio player for the whole session (reconnects reuse it). Missing
    // audio is never fatal — the session simply runs silent.
    let audio = if config.audio == AudioMode::ThisComputer {
        match crate::audio::AudioPlayer::start() {
            Ok(player) => Some(player),
            Err(e) => {
                tracing::warn!("audio playback unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    // Keys trusted during this session so retries never re-prompt for the cert.
    let mut session_trusted: Option<String> = config.expected_fingerprint.clone();
    let mut connected_once = false;
    let mut failures: u32 = 0;
    // Wake-on-LAN: parsed once; a packet goes out before every initial attempt
    // (a machine that is already awake simply ignores it).
    let wake_mac = config.wake_mac.as_deref().and_then(crate::wol::parse_mac);
    let max_initial_failures = if wake_mac.is_some() {
        MAX_INITIAL_FAILURES_WOL
    } else {
        MAX_INITIAL_FAILURES
    };

    loop {
        if !connected_once {
            if let Some(mac) = wake_mac {
                match crate::wol::send_magic_packet(mac) {
                    Ok(()) => {
                        if failures == 0 {
                            tracing::info!("wol: magic packet sent");
                        }
                    }
                    Err(e) => tracing::warn!("wol: sending the magic packet failed: {e}"),
                }
            }
        }
        let (clip_tx, clip_rx) = channel::<ClipSignal>(CLIPBOARD_QUEUE_CAPACITY);
        let (gfx_tx, gfx_rx) = tokio::sync::mpsc::unbounded_channel::<GfxEvent>();
        let outcome = match connect(
            &config,
            &mut rdsaad_client,
            &mut session_trusted,
            local_clip.clone(),
            clip_tx,
            gfx_tx,
            audio.as_ref(),
            &framebuffer,
            &event_cb,
            connected_once && config.reconnect,
        )
        .await
        {
            Ok((connection_result, framed)) => {
                connected_once = true;
                failures = 0;
                if audio.is_some() {
                    // Joined channel = the host accepted audio redirection; a
                    // missing join means it is disabled server-side (GPO or a
                    // stopped Windows Audio service), not a client problem.
                    match connection_result
                        .static_channels
                        .get_channel_id_by_type::<Rdpsnd>()
                    {
                        Some(id) => tracing::info!("rdpsnd: channel joined (id {id})"),
                        None => tracing::warn!(
                            "rdpsnd: the server did not join the audio channel — audio \
                             redirection is disabled on the host"
                        ),
                    }
                }
                if !config.reconnect {
                    config.password.zeroize();
                }
                match run_session(
                    connection_result,
                    framed,
                    &config,
                    &framebuffer,
                    &mut command_rx,
                    &pending,
                    clip_rx,
                    gfx_rx,
                    &local_clip,
                    &event_cb,
                )
                .await
                {
                    SessionEnd::UserQuit => Outcome::Stop,
                    SessionEnd::Disconnected(reason) => {
                        if config.reconnect {
                            Outcome::Retry {
                                delay: reconnect_delay(&config),
                                reconnect_attempt: Some(1),
                            }
                        } else {
                            Outcome::Fail {
                                reason,
                                event: TerminalEvent::Disconnected,
                            }
                        }
                    }
                }
            }
            Err(failure) => {
                let (error, retryable) = failure.into_parts();
                // `{error:#}` keeps the underlying io/TLS/auth cause.
                let reason = format!("{error:#}");
                if !connected_once && retryable {
                    // Only TCP reachability failures are retried. Authentication,
                    // certificate and protocol failures must never be repeated,
                    // because repeated bad credentials can lock a domain account.
                    failures += 1;
                    if failures >= max_initial_failures {
                        Outcome::Fail {
                            reason,
                            event: TerminalEvent::Error,
                        }
                    } else {
                        Outcome::Retry {
                            delay: INITIAL_RETRY_DELAY,
                            reconnect_attempt: None,
                        }
                    }
                } else if !connected_once {
                    Outcome::Fail {
                        reason,
                        event: TerminalEvent::Error,
                    }
                } else if !config.reconnect {
                    Outcome::Fail {
                        reason,
                        event: TerminalEvent::Disconnected,
                    }
                } else if !retryable {
                    Outcome::Fail {
                        reason,
                        event: TerminalEvent::ReconnectFailed,
                    }
                } else {
                    failures += 1;
                    if failures >= MAX_RECONNECT_FAILURES {
                        Outcome::Fail {
                            reason,
                            event: TerminalEvent::ReconnectFailed,
                        }
                    } else {
                        Outcome::Retry {
                            delay: reconnect_delay(&config),
                            reconnect_attempt: Some(failures + 1),
                        }
                    }
                }
            }
        };

        match outcome {
            Outcome::Stop => break,
            Outcome::Fail { reason, event } => {
                let reason = user_facing_disconnect_reason(&reason);
                match event {
                    TerminalEvent::Error => event_cb(SessionEvent::Error(reason)),
                    TerminalEvent::Disconnected => event_cb(SessionEvent::Disconnected { reason }),
                    TerminalEvent::ReconnectFailed => {
                        event_cb(SessionEvent::ReconnectFailed { reason })
                    }
                }
                break;
            }
            Outcome::Retry {
                delay,
                reconnect_attempt,
            } => {
                if drain_should_stop(&mut command_rx, &pending, &mut config, &local_clip) {
                    break;
                }
                if let Some(attempt) = reconnect_attempt {
                    event_cb(SessionEvent::Reconnecting {
                        attempt,
                        max_attempts: MAX_RECONNECT_FAILURES,
                    });
                }
                if wait_for_retry(delay, &mut command_rx, &pending, &mut config, &local_clip).await
                {
                    break;
                }
            }
        }
    }
}

/// A resize observed while disconnected becomes the size of the next connect,
/// so the session comes back matching the current window.
fn absorb_offline_command(
    command: Option<SessionCommand>,
    config: &mut SessionConfig,
    local_clip: &LocalClipState,
) {
    match command {
        Some(SessionCommand::Resize {
            width,
            height,
            scale,
        }) => {
            config.width = width;
            config.height = height;
            config.scale = scale;
        }
        Some(SessionCommand::LocalClipboard(text)) if text.len() <= MAX_CLIPBOARD_TEXT_BYTES => {
            local_clip.lock().unwrap().replace(LocalClip::Text(text));
        }
        Some(SessionCommand::PasteLocalClipboard(text))
            if text.len() <= MAX_CLIPBOARD_TEXT_BYTES =>
        {
            // Keep the dictated text available after reconnecting, but never
            // perform a delayed paste into a potentially different remote caret.
            local_clip.lock().unwrap().replace(LocalClip::Text(text));
        }
        Some(SessionCommand::LocalClipboardFiles(paths)) => {
            let files = collect_clipboard_files(&paths);
            if !files.is_empty() {
                local_clip.lock().unwrap().replace(LocalClip::Files(files));
            }
        }
        _ => {}
    }
}

async fn wait_for_retry(
    delay: Duration,
    command_rx: &mut Receiver<SessionCommand>,
    pending: &PendingCommands,
    config: &mut SessionConfig,
    local_clip: &LocalClipState,
) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if pending.shutdown.swap(false, Ordering::AcqRel) {
            return true;
        }
        pending.release_all_keys.store(false, Ordering::Release);
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return false,
            command = command_rx.recv() => match command {
                Some(SessionCommand::Shutdown) | None => return true,
                Some(command) => {
                    absorb_offline_command(
                        resolve_pending_command(command, pending),
                        config,
                        local_clip,
                    );
                }
            }
        }
    }
}

/// Drain any pending commands between reconnects. Returns true if the session
/// should stop (the window was closed, so a Shutdown is queued or all senders
/// are gone).
fn drain_should_stop(
    command_rx: &mut Receiver<SessionCommand>,
    pending: &PendingCommands,
    config: &mut SessionConfig,
    local_clip: &LocalClipState,
) -> bool {
    use tokio::sync::mpsc::error::TryRecvError;
    if pending.shutdown.swap(false, Ordering::AcqRel) {
        return true;
    }
    loop {
        match command_rx.try_recv() {
            Ok(SessionCommand::Shutdown) => return true,
            Ok(command) => {
                absorb_offline_command(
                    resolve_pending_command(command, pending),
                    config,
                    local_clip,
                );
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    connection_result: ConnectionResult,
    framed: SessionFramed,
    config: &SessionConfig,
    framebuffer: &SharedFramebuffer,
    command_rx: &mut Receiver<SessionCommand>,
    pending: &PendingCommands,
    mut clip_rx: Receiver<ClipSignal>,
    mut gfx_rx: tokio::sync::mpsc::UnboundedReceiver<GfxEvent>,
    local_clip: &LocalClipState,
    event_cb: &EventCb,
) -> SessionEnd {
    let width = connection_result.desktop_size.width;
    let height = connection_result.desktop_size.height;
    if width > crate::profile::MAX_REMOTE_DIMENSION || height > crate::profile::MAX_REMOTE_DIMENSION
    {
        return SessionEnd::Disconnected(format!(
            "server negotiated an unreasonable desktop size {width}x{height}"
        ));
    }
    framebuffer.resize(width, height);
    let mut image = DecodedImage::new(PIXEL_FORMAT, width, height);
    let activation_factory = connection_result.activation_factory;
    let mut active_stage = ActiveStageBuilder {
        static_channels: connection_result.static_channels,
        user_channel_id: connection_result.user_channel_id,
        io_channel_id: connection_result.io_channel_id,
        message_channel_id: connection_result.message_channel_id,
        share_id: connection_result.share_id,
        compression_type: connection_result.compression_type,
        enable_server_pointer: connection_result.enable_server_pointer,
        pointer_software_rendering: connection_result.pointer_software_rendering,
    }
    .build();
    let mut input_db = Database::new();

    // Split the stream so a write blocked on TCP backpressure can never stall
    // reads: reads run here, writes drain on a dedicated task.
    let (mut reader, writer) = split_tokio_framed(framed);
    let (out_tx, out_rx) = channel::<Vec<u8>>(OUTPUT_QUEUE_CAPACITY);
    let mut writer_task = tokio::spawn(writer_loop(writer, out_rx));
    if let Err(error) =
        synchronize_initial_keyboard_state(&mut active_stage, &mut image, &out_tx).await
    {
        drop(out_tx);
        writer_task.abort();
        return SessionEnd::Disconnected(format!("{error:#}"));
    }
    event_cb(SessionEvent::Connected { width, height });
    // With clipboard disabled the sender is dropped at connect; stop polling the
    // closed channel or the select loop would spin at 100% CPU. Same for gfx.
    let mut clip_open = true;
    let mut gfx_open = true;
    let mut remote_clip = RemoteClipboard::default();
    let mut clipboard_timeout_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + CLIPBOARD_TIMEOUT_TICK,
        CLIPBOARD_TIMEOUT_TICK,
    );
    clipboard_timeout_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Idle keep-alive bookkeeping: `last_input` is pushed forward by every real
    // user command, so the timer arm below only fires after a full idle
    // interval; `keys_down` suppresses the tap while a key is held so the
    // injected F15 can never merge with a modifier the user is pressing.
    let mut last_input = tokio::time::Instant::now();
    let mut keys_down: u32 = 0;

    let end = loop {
        if pending.shutdown.swap(false, Ordering::AcqRel) {
            let _ = handle_command(
                SessionCommand::Shutdown,
                config,
                &mut reader,
                &out_tx,
                &mut active_stage,
                &mut image,
                &activation_factory,
                &mut input_db,
                framebuffer,
                local_clip,
                &mut remote_clip,
                event_cb,
            )
            .await;
            break SessionEnd::UserQuit;
        }
        if pending.release_all_keys.swap(false, Ordering::AcqRel) {
            // This path (not the command channel) usually wins the race, so
            // clear the held-key counter here too or the keep-alive could stay
            // suppressed after focus is lost while a key was held.
            keys_down = 0;
            let fp_events = input_db.release_all();
            if !fp_events.is_empty() {
                match active_stage.process_fastpath_input(&mut image, &fp_events) {
                    Ok(outputs) => match drain_outputs(
                        outputs,
                        &mut reader,
                        &out_tx,
                        &mut active_stage,
                        &activation_factory,
                        &mut image,
                        framebuffer,
                        event_cb,
                    )
                    .await
                    {
                        Ok(true) => {
                            break SessionEnd::Disconnected(REMOTE_ENDED.to_string());
                        }
                        Ok(false) => {}
                        Err(error) => break SessionEnd::Disconnected(format!("{error:#}")),
                    },
                    Err(error) => break SessionEnd::Disconnected(format!("{error:#}")),
                }
            }
        }
        tokio::select! {
            cmd = command_rx.recv() => {
                let Some(cmd) = cmd else { break SessionEnd::UserQuit };
                let Some(cmd) = resolve_pending_command(cmd, pending) else {
                    continue;
                };
                if matches!(cmd, SessionCommand::Input(_)) {
                    last_input = tokio::time::Instant::now();
                }
                keys_down = update_keys_down(&cmd, keys_down);
                match handle_command(
                    cmd, config, &mut reader, &out_tx, &mut active_stage, &mut image,
                    &activation_factory, &mut input_db, framebuffer, local_clip,
                    &mut remote_clip, event_cb,
                ).await {
                    Ok(true) => break SessionEnd::UserQuit,
                    Ok(false) => {}
                    Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                }
            }

            sig = clip_rx.recv(), if clip_open => {
                match sig {
                    Some(sig) => {
                        let is_external_paste = matches!(sig, ClipSignal::PasteAccepted { .. });
                        match handle_clip_signal(
                            sig,
                            &mut reader,
                            &out_tx,
                            &mut active_stage,
                            &mut image,
                            &activation_factory,
                            &mut input_db,
                            framebuffer,
                            local_clip,
                            &mut remote_clip,
                            event_cb,
                        ).await {
                            Ok(true) => break SessionEnd::Disconnected(REMOTE_ENDED.to_string()),
                            Ok(false) => {
                                if is_external_paste {
                                    last_input = tokio::time::Instant::now();
                                    keys_down = 0;
                                }
                            }
                            Err(e) => tracing::warn!("clipboard: {e}"),
                        }
                    }
                    None => clip_open = false,
                }
            }

            _ = clipboard_timeout_tick.tick(), if config.clipboard.enabled() => {
                if let Err(error) = send_cliprdr(&mut active_stage, &out_tx, |clipboard| {
                    clipboard.drive_timeouts()
                }).await {
                    tracing::warn!("clipboard: timeout maintenance failed: {error:#}");
                }
            }

            ev = gfx_rx.recv(), if gfx_open => {
                match ev {
                    // The compositor already wrote the pixels; just repaint.
                    Some(GfxEvent::Updated) => event_cb(SessionEvent::FrameUpdated {
                        x: 0, y: 0, width: 0, height: 0,
                    }),
                    Some(GfxEvent::Resized { width, height }) => {
                        event_cb(SessionEvent::Resized { width, height });
                    }
                    None => gfx_open = false,
                }
            }

            // Idle keep-alive. Disabled unless enabled and no key is held; the
            // sleep target moves with `last_input`, so any real input reschedules
            // it and the tap only fires after a full idle interval.
            _ = tokio::time::sleep_until(last_input + KEEP_ALIVE_INTERVAL),
                if config.keep_alive && keys_down == 0 =>
            {
                match send_keepalive_tap(
                    &mut reader, &out_tx, &mut active_stage, &mut image,
                    &activation_factory, &mut input_db, framebuffer, event_cb,
                ).await {
                    Ok(true) => break SessionEnd::Disconnected(REMOTE_ENDED.to_string()),
                    Ok(false) => {}
                    Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                }
                last_input = tokio::time::Instant::now();
            }

            pdu = reader.read_pdu() => {
                match pdu {
                    Ok((action, payload)) => match active_stage.process(&mut image, action, &payload) {
                        Ok(outputs) => match drain_outputs(
                            outputs,
                            &mut reader,
                            &out_tx,
                            &mut active_stage,
                            &activation_factory,
                            &mut image,
                            framebuffer,
                            event_cb,
                        ).await {
                            Ok(true) => break SessionEnd::Disconnected(REMOTE_ENDED.to_string()),
                            Ok(false) => {}
                            Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                        },
                        Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                    },
                    Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                }
            }
        }
    };

    // Close the queue so the writer drains and exits. On a clean quit, wait for
    // the drain (bounded) so the graceful-shutdown PDU is flushed; then abort so
    // a task blocked on a wedged socket can never leak.
    drop(out_tx);
    if matches!(end, SessionEnd::UserQuit) {
        let _ = tokio::time::timeout(SHUTDOWN_FLUSH, &mut writer_task).await;
    }
    writer_task.abort();
    end
}

fn resolve_pending_command(
    command: SessionCommand,
    pending: &PendingCommands,
) -> Option<SessionCommand> {
    match command {
        SessionCommand::ReleaseAllKeys => pending
            .release_all_keys
            .swap(false, Ordering::AcqRel)
            .then_some(SessionCommand::ReleaseAllKeys),
        SessionCommand::Shutdown => pending
            .shutdown
            .swap(false, Ordering::AcqRel)
            .then_some(SessionCommand::Shutdown),
        SessionCommand::Input(events)
            if matches!(events.as_slice(), [InputEvent::MouseMove { .. }]) =>
        {
            let mut slot = pending.mouse_move.lock().unwrap();
            slot.queued = false;
            slot.value
                .take()
                .map(|(x, y)| SessionCommand::Input(vec![InputEvent::MouseMove { x, y }]))
        }
        SessionCommand::Resize { .. } => {
            let mut slot = pending.resize.lock().unwrap();
            slot.queued = false;
            slot.value
                .take()
                .map(|(width, height, scale)| SessionCommand::Resize {
                    width,
                    height,
                    scale,
                })
        }
        command => Some(command),
    }
}

/// Drain queued frames to the socket. Runs concurrently with the read loop so a
/// blocked write never stalls reads.
async fn writer_loop(mut writer: SessionWriter, mut out_rx: Receiver<Vec<u8>>) {
    while let Some(bytes) = out_rx.recv().await {
        if let Err(e) = writer.write_all(&bytes).await {
            tracing::debug!("write failed, stopping writer: {e}");
            break;
        }
    }
}

async fn connect(
    config: &SessionConfig,
    rdsaad_client: &mut Option<RdsAadClient>,
    session_trusted: &mut Option<String>,
    local_clip: LocalClipState,
    clip_tx: Sender<ClipSignal>,
    gfx_tx: tokio::sync::mpsc::UnboundedSender<GfxEvent>,
    audio: Option<&crate::audio::AudioPlayer>,
    framebuffer: &Arc<SharedFramebuffer>,
    event_cb: &EventCb,
    retry_authentication_during_reconnect: bool,
) -> std::result::Result<(ConnectionResult, SessionFramed), ConnectFailure> {
    let tcp = connect_tcp(&config.host, config.port).await?;
    tcp.set_nodelay(true).ok();
    let client_addr = tcp
        .local_addr()
        .context("resolving local address")
        .map_err(ConnectFailure::fatal)?;

    let display_control = DisplayControlClient::new(|_caps| Ok(Vec::new()));
    let mut drdynvc = DrdynvcClient::new().with_dynamic_channel(display_control);
    if let Some(player) = audio {
        // Windows 7+ prefers audio over the dynamic channel when the client
        // supports DVC; without this listener the session stays silent.
        drdynvc =
            drdynvc.with_dynamic_channel(crate::audio::RdpsndDvcChannel::new(player.handler()));
    }
    if config.graphics == GraphicsMode::Egfx {
        // H.264 decodes with VideoToolbox (the hardware decoder where there
        // is one), switching to OpenH264 if VideoToolbox can't decode the
        // stream. `RDP123_H264=openh264` uses OpenH264 from the start.
        let decoder: Box<dyn ironrdp_egfx::decode::H264Decoder> =
            if std::env::var_os("RDP123_H264").is_some_and(|v| v == "openh264") {
                tracing::info!("egfx: H.264 decoding with OpenH264 (RDP123_H264)");
                Box::new(
                    ironrdp_egfx::decode::OpenH264Decoder::new().expect("bundled OpenH264 decoder"),
                )
            } else {
                Box::new(crate::videotoolbox::VideoToolboxDecoder::new())
            };
        let decoder = Some(decoder);
        let handler = crate::gfx::GfxHandler::new(framebuffer.clone(), gfx_tx, config.avc444);
        drdynvc = drdynvc.with_dynamic_channel(ironrdp_egfx::client::GraphicsPipelineClient::new(
            Box::new(handler),
            decoder,
        ));
    }

    let mut connector = ClientConnector::new(build_config(config, audio.is_some()), client_addr)
        .with_static_channel(drdynvc);

    if let Some(player) = audio {
        connector.attach_static_channel(Rdpsnd::new(Box::new(player.handler())));
        // mstsc always announces device redirection; some servers gate parts
        // of channel setup (audio among them) on its presence. No devices are
        // shared — the Noop backend only completes the core handshake.
        connector
            .attach_static_channel(Rdpdr::new(Box::new(NoopRdpdrBackend), "RDP123".to_owned()));
    }

    // Only register clipboard redirection when it is enabled.
    if config.clipboard.enabled() {
        local_clip.lock().unwrap().reset_connection();
        let backend = MacClipboardBackend {
            tx: clip_tx,
            local_clip,
            tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            mode: config.clipboard,
        };
        connector.attach_static_channel(CliprdrClient::new(Box::new(backend)));
    } else {
        drop((clip_tx, local_clip));
    }

    let mut framed = TokioFramed::new(tcp);

    // 1. Pre-TLS X.224 negotiation.
    let should_upgrade = connect_begin(&mut framed, &mut connector)
        .await
        .map_err(|error| connector_failure(error, retry_authentication_during_reconnect))?;

    // 2. TLS handshake. CA/hostname verification is intentionally replaced by
    // TOFU below, but the server must still prove possession of the certificate
    // private key by signing the TLS handshake.
    let (initial_stream, leftover) = framed.into_inner();
    let (mut tls_stream, server_public_key) = crate::tls::upgrade(initial_stream, &config.host)
        .await
        .context("TLS handshake")
        .map_err(ConnectFailure::fatal)?;

    // 3. Trust-on-first-use gate — before any credentials are sent via CredSSP.
    let fingerprint = fingerprint_hex(&server_public_key);
    tofu_gate(session_trusted, &fingerprint, event_cb)
        .await
        .map_err(ConnectFailure::fatal)?;

    // 4. Finalize (MCS, licensing, capabilities, CredSSP/NLA).
    let upgraded = mark_as_upgraded(should_upgrade, &mut connector);
    if connector.should_perform_rdsaad() {
        let client = rdsaad_client
            .as_mut()
            .context("RDS AAD authentication state is missing")
            .map_err(ConnectFailure::fatal)?;
        client
            .authenticate(&mut tls_stream, &config.username, event_cb)
            .await
            .context("Microsoft Entra RDP authentication")
            .map_err(ConnectFailure::fatal)?;
        connector.mark_rdsaad_as_done();
    }
    let mut framed = TokioFramed::new_with_leftover(tls_stream, leftover);
    let mut network_client = NoNetworkClient;
    let result = connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut network_client,
        ServerName::new(&config.host),
        server_public_key,
        None,
    )
    .await
    .map_err(|error| connector_failure(error, retry_authentication_during_reconnect))?;

    Ok((result, framed))
}

enum TcpHostFailure {
    Resolve(io::Error),
    Connect(io::Error),
}

/// Return the Bonjour/mDNS form of a single-label host name. Microsoft clients
/// can resolve these names through local discovery even when macOS DNS cannot;
/// appending `.local` lets the system mDNS resolver provide the same address.
fn mdns_fallback_hostname(host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() || host.contains('.') || host.contains(':') || host.parse::<IpAddr>().is_ok()
    {
        return None;
    }
    Some(format!("{host}.local"))
}

/// Resolve and connect to one host, retaining whether failure happened during
/// name resolution or while opening the socket. The distinction prevents a
/// reachable DNS host with a closed RDP port from being redirected elsewhere.
async fn connect_tcp_host(host: &str, port: u16) -> std::result::Result<TcpStream, TcpHostFailure> {
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(TcpHostFailure::Resolve)?;
    let mut last_error = None;
    let mut found_address = false;

    for address in addresses {
        found_address = true;
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }

    let error = last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            if found_address {
                "could not connect to any resolved address"
            } else {
                "name resolved to no addresses"
            },
        )
    });
    Err(TcpHostFailure::Connect(error))
}

/// Open the TCP socket with a bounded timeout. For a single-label LAN host,
/// fall back to Bonjour/mDNS only when ordinary DNS resolution fails. The
/// configured host remains unchanged for TLS, Entra and CredSSP identity.
async fn connect_tcp(host: &str, port: u16) -> std::result::Result<TcpStream, ConnectFailure> {
    let connect = async {
        match connect_tcp_host(host, port).await {
            Ok(stream) => Ok(stream),
            Err(TcpHostFailure::Resolve(primary_error)) => {
                if let Some(mdns_host) = mdns_fallback_hostname(host) {
                    tracing::info!(
                        host,
                        mdns_host,
                        "ordinary host lookup failed; trying Bonjour/mDNS"
                    );
                    match connect_tcp_host(&mdns_host, port).await {
                        Ok(stream) => Ok(stream),
                        Err(TcpHostFailure::Resolve(mdns_error)) => Err(anyhow!(
                            "could not resolve {host}:{port} ({primary_error}); also tried \
                             {mdns_host}:{port} via Bonjour/mDNS ({mdns_error}). If this host \
                             works in other RDP clients, enable RDP123 under System Settings → \
                             Privacy & Security → Local Network, then try again."
                        )),
                        Err(TcpHostFailure::Connect(mdns_error)) => Err(anyhow!(
                            "resolved {host} as {mdns_host} via Bonjour/mDNS, but could not \
                             reach port {port} ({mdns_error}). Check that RDP is enabled and \
                             RDP123 is allowed under System Settings → Privacy & Security → \
                             Local Network."
                        )),
                    }
                } else {
                    Err(anyhow!(
                        "could not resolve {host}:{port} ({primary_error}). Check the host name \
                         and network connection."
                    ))
                }
            }
            Err(TcpHostFailure::Connect(error)) => Err(anyhow!(
                "could not reach {host}:{port} ({error}). If this host works in other RDP \
                 clients, enable RDP123 under System Settings → Privacy & Security → Local \
                 Network, then try again."
            )),
        }
    };

    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(ConnectFailure::retryable(error)),
        Err(_) => Err(ConnectFailure::retryable(anyhow!(
            "timed out reaching {host}:{port}. Check the host and port, and that RDP123 \
             is allowed under System Settings → Privacy & Security → Local Network."
        ))),
    }
}

async fn tofu_gate(
    session_trusted: &mut Option<String>,
    fingerprint: &str,
    event_cb: &EventCb,
) -> Result<()> {
    if session_trusted.as_deref() == Some(fingerprint) {
        return Ok(());
    }
    let is_change = session_trusted.is_some();
    let (reply, rx) = oneshot::channel();
    event_cb(SessionEvent::CertificateApproval {
        fingerprint: fingerprint.to_string(),
        is_change,
        reply,
    });
    if rx.await.unwrap_or(false) {
        event_cb(SessionEvent::CertTrusted {
            fingerprint: fingerprint.to_string(),
        });
        *session_trusted = Some(fingerprint.to_string());
        Ok(())
    } else {
        Err(anyhow!("server certificate not trusted"))
    }
}

async fn handle_command(
    cmd: SessionCommand,
    config: &SessionConfig,
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    activation_factory: &ConnectionActivationFactory,
    input_db: &mut Database,
    framebuffer: &SharedFramebuffer,
    local_clip: &LocalClipState,
    _remote_clip: &mut RemoteClipboard,
    event_cb: &EventCb,
) -> Result<bool> {
    match cmd {
        SessionCommand::Input(events) => {
            let (operations, last_mouse) = translate_input(events, config.swap_cmd_alt);
            if let Some((x, y)) = last_mouse {
                active_stage.update_mouse_pos(x, y);
            }
            let fp_events = input_db.apply(operations);
            if !fp_events.is_empty() {
                let outputs = active_stage.process_fastpath_input(image, &fp_events)?;
                return drain_outputs(
                    outputs,
                    reader,
                    out_tx,
                    active_stage,
                    activation_factory,
                    image,
                    framebuffer,
                    event_cb,
                )
                .await;
            }
        }
        SessionCommand::Resize {
            width,
            height,
            scale,
        } => {
            // Fixed-resolution sessions keep the remote size; the window just scales it.
            if !config.dynamic_resolution {
                return Ok(false);
            }
            let (w, h) =
                MonitorLayoutEntry::adjust_display_size(u32::from(width), u32::from(height));
            match active_stage.encode_resize(w, h, scale, None) {
                Some(Ok(frame)) => emit(out_tx, frame).await?,
                Some(Err(e)) => tracing::warn!("resize encode failed: {e}"),
                None => tracing::debug!("resize ignored: display control not ready yet"),
            }
        }
        SessionCommand::LocalClipboard(text) => {
            if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                tracing::warn!(
                    "local clipboard text is too large to redirect ({} bytes)",
                    text.len()
                );
                return Ok(false);
            }
            let generation = local_clip.lock().unwrap().replace(LocalClip::Text(text));
            advertise_local_clipboard(generation, local_clip, active_stage, out_tx).await?;
        }
        SessionCommand::PasteLocalClipboard(text) => {
            if !config.clipboard.allow_local_to_remote() {
                tracing::debug!(
                    "external STT paste ignored because local-to-remote clipboard is disabled"
                );
                return Ok(false);
            }
            if text.is_empty() || text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                tracing::warn!(
                    "external STT paste text is empty or too large ({} bytes)",
                    text.len()
                );
                return Ok(false);
            }
            let generation = local_clip.lock().unwrap().replace_for_paste(text);
            advertise_local_clipboard(generation, local_clip, active_stage, out_tx).await?;
        }
        SessionCommand::LocalClipboardFiles(paths) => {
            let files = collect_clipboard_files(&paths);
            if files.is_empty() {
                return Ok(false);
            }
            tracing::debug!("clipboard: offering {} file entries", files.len());
            let generation = local_clip.lock().unwrap().replace(LocalClip::Files(files));
            advertise_local_clipboard(generation, local_clip, active_stage, out_tx).await?;
        }
        SessionCommand::ReleaseAllKeys => {
            let fp_events = input_db.release_all();
            if !fp_events.is_empty() {
                let outputs = active_stage.process_fastpath_input(image, &fp_events)?;
                return drain_outputs(
                    outputs,
                    reader,
                    out_tx,
                    active_stage,
                    activation_factory,
                    image,
                    framebuffer,
                    event_cb,
                )
                .await;
            }
        }
        SessionCommand::Shutdown => {
            if let Ok(outputs) = active_stage.graceful_shutdown() {
                for out in outputs {
                    if let ActiveStageOutput::ResponseFrame(frame) = out {
                        emit(out_tx, frame).await?;
                    }
                }
            }
            return Ok(true);
        }
    }
    Ok(false)
}

async fn handle_clip_signal(
    sig: ClipSignal,
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    activation_factory: &ConnectionActivationFactory,
    input_db: &mut Database,
    framebuffer: &SharedFramebuffer,
    local_clip: &LocalClipState,
    remote_clip: &mut RemoteClipboard,
    event_cb: &EventCb,
) -> Result<bool> {
    match sig {
        ClipSignal::InitializeClipboard => {
            let formats = local_clip.lock().unwrap().begin_initial_offer();
            if let Some(formats) = formats {
                send_cliprdr(active_stage, out_tx, |c| c.initiate_copy(&formats)).await?;
            }
            Ok(false)
        }
        ClipSignal::AdvertiseLocal { generation } => {
            advertise_local_clipboard(generation, local_clip, active_stage, out_tx).await?;
            Ok(false)
        }
        ClipSignal::PasteAccepted { generation } => {
            if !local_clip
                .lock()
                .unwrap()
                .consume_confirmed_paste(generation)
            {
                tracing::debug!(generation, "clipboard: cancelled stale external STT paste");
                return Ok(false);
            }
            tracing::debug!(
                generation,
                "clipboard: remote accepted external STT text; injecting Ctrl+V"
            );
            let ended = send_remote_clipboard_paste(
                reader,
                out_tx,
                active_stage,
                image,
                activation_factory,
                input_db,
                framebuffer,
                event_cb,
            )
            .await?;
            Ok(ended)
        }
        ClipSignal::PasteDataTimedOut { generation } => {
            let deferred_generation = local_clip
                .lock()
                .unwrap()
                .expire_unrequested_paste(generation);
            if let Some(deferred_generation) = deferred_generation {
                tracing::debug!(
                    generation,
                    "clipboard: external STT data request timed out; restoring deferred clipboard"
                );
                advertise_local_clipboard(deferred_generation, local_clip, active_stage, out_tx)
                    .await?;
            }
            Ok(false)
        }
        ClipSignal::SubmitFileContents(response) => {
            send_cliprdr(active_stage, out_tx, |c| c.submit_file_contents(response)).await?;
            Ok(false)
        }
        ClipSignal::InitiatePaste(format) => {
            send_cliprdr(active_stage, out_tx, |c| c.initiate_paste(format)).await?;
            Ok(false)
        }
        ClipSignal::SubmitData {
            response,
            paste_generation,
        } => {
            send_cliprdr(active_stage, out_tx, |c| c.submit_format_data(response)).await?;
            if let Some(generation) = paste_generation {
                let deferred_generation =
                    local_clip.lock().unwrap().finish_serving_paste(generation);
                if let Some(deferred_generation) = deferred_generation {
                    advertise_local_clipboard(
                        deferred_generation,
                        local_clip,
                        active_stage,
                        out_tx,
                    )
                    .await?;
                }
            }
            Ok(false)
        }
        ClipSignal::RemoteText(text) => {
            event_cb(SessionEvent::ClipboardText(text));
            Ok(false)
        }
        ClipSignal::RemoteFileList { files, data_id } => {
            // A newer Windows clipboard offer supersedes any partial cache
            // transfer. Dropping unfinished jobs removes their private cache.
            remote_clip.jobs.clear();
            remote_clip.outstanding = None;
            remote_clip.files = files;
            remote_clip.data_id = data_id;
            let names: Vec<String> = remote_clip
                .files
                .iter()
                .filter(|f| !f.wire_name.contains('\\'))
                .map(|f| f.wire_name.clone())
                .collect();
            if names.is_empty() {
                return Ok(false);
            }

            event_cb(SessionEvent::ClipboardFilesPreparing { count: names.len() });
            let planned = create_remote_clipboard_cache_dir()
                .and_then(|cache_dir| plan_remote_clipboard_cache(remote_clip, &names, cache_dir));
            match planned {
                Ok(job) => {
                    remote_clip.jobs.push_back(job);
                    if let Err(reason) =
                        advance_remote_fetch(remote_clip, active_stage, out_tx, event_cb).await
                    {
                        fail_front_job(remote_clip, reason, event_cb);
                    }
                }
                Err(reason) => {
                    tracing::warn!("clipboard: could not prepare remote files: {reason}");
                    event_cb(SessionEvent::ClipboardFilesFailed(reason));
                }
            }
            Ok(false)
        }
        ClipSignal::RemoteFileContents { stream_id, data } => {
            handle_remote_file_contents(
                remote_clip,
                stream_id,
                data,
                active_stage,
                out_tx,
                event_cb,
            )
            .await;
            Ok(false)
        }
    }
}

async fn advertise_local_clipboard(
    generation: u64,
    local_clip: &LocalClipState,
    active_stage: &mut ActiveStage,
    out_tx: &OutSender,
) -> Result<()> {
    let offer = {
        let mut state = local_clip.lock().unwrap();
        if !state.is_ready() {
            return Ok(());
        }
        state.begin_offer(generation)
    };
    match offer {
        Some(LocalClipboardOffer::Text(formats)) => {
            send_cliprdr(active_stage, out_tx, |c| c.initiate_copy(&formats)).await
        }
        Some(LocalClipboardOffer::Files(descriptors)) => {
            send_cliprdr(active_stage, out_tx, |c| c.initiate_file_copy(descriptors)).await
        }
        None => Ok(()),
    }
}

/// Queue a frame for the writer task with bounded backpressure.
async fn emit(out_tx: &OutSender, bytes: Vec<u8>) -> Result<()> {
    out_tx
        .send(bytes)
        .await
        .map_err(|_| anyhow!("session writer stopped"))
}

/// Call a `CliprdrClient` method (if the channel exists), then encode and queue
/// the resulting messages.
async fn send_cliprdr<F>(active_stage: &mut ActiveStage, out_tx: &OutSender, make: F) -> Result<()>
where
    F: FnOnce(&mut CliprdrClient) -> PduResult<CliprdrSvcMessages<Client>>,
{
    let produced = active_stage
        .get_svc_processor_mut::<CliprdrClient>()
        .map(make);
    if let Some(result) = produced {
        let messages = result.map_err(|e| anyhow!("cliprdr: {e}"))?;
        let bytes = active_stage
            .process_svc_processor_messages(messages)
            .map_err(|e| anyhow!("cliprdr encode: {e}"))?;
        emit(out_tx, bytes).await?;
    }
    Ok(())
}

/// Apply every `ActiveStageOutput`. Returns `Ok(true)` when the session should end.
async fn drain_outputs(
    outputs: Vec<ActiveStageOutput>,
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    activation_factory: &ConnectionActivationFactory,
    image: &mut DecodedImage,
    framebuffer: &SharedFramebuffer,
    event_cb: &EventCb,
) -> Result<bool> {
    for output in outputs {
        match output {
            ActiveStageOutput::ResponseFrame(frame) => emit(out_tx, frame).await?,
            ActiveStageOutput::GraphicsUpdate(region) => {
                let width = region.width();
                let height = region.height();
                framebuffer.blit_rect(image.data(), region.left, region.top, width, height);
                event_cb(SessionEvent::FrameUpdated {
                    x: region.left,
                    y: region.top,
                    width,
                    height,
                });
            }
            ActiveStageOutput::DeactivateAll => {
                reactivate(
                    activation_factory.create(),
                    reader,
                    out_tx,
                    active_stage,
                    image,
                    framebuffer,
                    event_cb,
                )
                .await?;
            }
            ActiveStageOutput::Terminate(_reason) => return Ok(true),
            // Pointer shapes are mirrored onto the native macOS cursor so the
            // remote shape (resize arrows, I-beam, hand) shows without the
            // laggy server-composited cursor.
            ActiveStageOutput::PointerBitmap(pointer) => {
                event_cb(SessionEvent::PointerBitmap {
                    width: pointer.width,
                    height: pointer.height,
                    hotspot_x: pointer.hotspot_x,
                    hotspot_y: pointer.hotspot_y,
                    rgba: pointer.bitmap_data.clone(),
                });
            }
            ActiveStageOutput::PointerDefault => event_cb(SessionEvent::PointerDefault),
            ActiveStageOutput::PointerHidden => event_cb(SessionEvent::PointerHidden),
            // Server-initiated pointer warps are not applied to the local
            // mouse; the multitransport/autodetect paths are unused.
            ActiveStageOutput::PointerPosition { .. }
            | ActiveStageOutput::MultitransportRequest(_)
            | ActiveStageOutput::AutoDetect(_) => {}
        }
    }
    Ok(false)
}

/// Drive a Deactivation-Reactivation sequence (e.g. a server-side resolution
/// change) to completion, then re-size local state to the new desktop.
async fn reactivate(
    mut cas: ConnectionActivationSequence,
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    framebuffer: &SharedFramebuffer,
    event_cb: &EventCb,
) -> Result<()> {
    let (size, share_id, enable_server_pointer) = loop {
        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            enable_server_pointer,
            ..
        } = cas.connection_activation_state()
        {
            break (desktop_size, share_id, enable_server_pointer);
        }
        // Read + step on the read half; queue any response for the writer task.
        let mut buf = WriteBuf::new();
        single_sequence_step_read(reader, &mut cas, &mut buf)
            .await
            .map_err(connector_err)?;
        if !buf.filled().is_empty() {
            emit(out_tx, buf.filled().to_vec()).await?;
        }
    };

    // Never allocate for an absurd server-announced size (a hostile or buggy
    // server could otherwise request a multi-gigabyte framebuffer).
    if size.width > crate::profile::MAX_REMOTE_DIMENSION
        || size.height > crate::profile::MAX_REMOTE_DIMENSION
    {
        return Err(anyhow!(
            "server requested an unreasonable desktop size {}x{}",
            size.width,
            size.height
        ));
    }
    *image = DecodedImage::new(PIXEL_FORMAT, size.width, size.height);
    framebuffer.resize(size.width, size.height);
    active_stage.set_share_id(share_id);
    active_stage.set_enable_server_pointer(enable_server_pointer);
    synchronize_initial_keyboard_state(active_stage, image, out_tx).await?;
    event_cb(SessionEvent::Resized {
        width: size.width,
        height: size.height,
    });
    Ok(())
}

/// Set a deterministic remote toggle-key state. A synchronize event is
/// idempotent, unlike pressing Num Lock, which could turn an already-on state
/// back off.
fn initial_keyboard_sync_event() -> FastPathInputEvent {
    synchronize_event(false, true, false, false)
}

/// Send Num Lock = on after each activation (initial connect, reconnect, or a
/// server-initiated Deactivation-Reactivation cycle).
async fn synchronize_initial_keyboard_state(
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    out_tx: &OutSender,
) -> Result<()> {
    let outputs = active_stage.process_fastpath_input(image, &[initial_keyboard_sync_event()])?;
    for output in outputs {
        match output {
            ActiveStageOutput::ResponseFrame(frame) => emit(out_tx, frame).await?,
            _ => {
                return Err(anyhow!(
                    "unexpected session output while synchronizing keyboard state"
                ));
            }
        }
    }
    Ok(())
}

/// Fold a command's key events into the held-key counter that gates the idle
/// keep-alive. Key-down bumps it, key-up drops it (saturating at zero), and a
/// full release resets it; everything else leaves the count untouched. While
/// the count is above zero the keep-alive is suppressed so an injected F15
/// cannot combine with a modifier the user is holding.
fn update_keys_down(cmd: &SessionCommand, keys_down: u32) -> u32 {
    match cmd {
        SessionCommand::Input(events) => {
            let mut n = i64::from(keys_down);
            for ev in events {
                if let InputEvent::Key { down, .. } = ev {
                    n += if *down { 1 } else { -1 };
                }
            }
            n.max(0) as u32
        }
        SessionCommand::ReleaseAllKeys => 0,
        _ => keys_down,
    }
}

/// Inject a single invisible F15 tap (down+up) straight into the FastPath input
/// stream, bypassing the mac→scancode keymap. Returns `Ok(true)` if flushing the
/// tap revealed that the remote had already ended. Used by the idle keep-alive.
#[allow(clippy::too_many_arguments)]
async fn send_keepalive_tap(
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    activation_factory: &ConnectionActivationFactory,
    input_db: &mut Database,
    framebuffer: &SharedFramebuffer,
    event_cb: &EventCb,
) -> Result<bool> {
    let scan = Scancode::from_u8(false, KEEP_ALIVE_SCANCODE);
    let fp_events = input_db.apply(vec![
        Operation::KeyPressed(scan),
        Operation::KeyReleased(scan),
    ]);
    if fp_events.is_empty() {
        return Ok(false);
    }
    let outputs = active_stage.process_fastpath_input(image, &fp_events)?;
    drain_outputs(
        outputs,
        reader,
        out_tx,
        active_stage,
        activation_factory,
        image,
        framebuffer,
        event_cb,
    )
    .await
}

fn remote_clipboard_paste_operations() -> [Operation; 4] {
    let ctrl = Scancode::from_u8(false, LEFT_CTRL_SCANCODE);
    let v = Scancode::from_u8(false, V_SCANCODE);
    [
        Operation::KeyPressed(ctrl),
        Operation::KeyPressed(v),
        Operation::KeyReleased(v),
        Operation::KeyReleased(ctrl),
    ]
}

fn remote_clipboard_paste_input_events(input_db: &mut Database) -> Vec<FastPathInputEvent> {
    let mut events: Vec<_> = input_db.release_all().into_iter().collect();
    events.extend(input_db.apply(remote_clipboard_paste_operations()));
    events
}

/// Release any physical modifiers still held by the macOS paste shortcut, then
/// inject an isolated remote Ctrl+V. This prevents ⌘ from becoming Win+Ctrl+V.
async fn send_remote_clipboard_paste(
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    activation_factory: &ConnectionActivationFactory,
    input_db: &mut Database,
    framebuffer: &SharedFramebuffer,
    event_cb: &EventCb,
) -> Result<bool> {
    let fp_events = remote_clipboard_paste_input_events(input_db);
    let outputs = active_stage.process_fastpath_input(image, &fp_events)?;
    drain_outputs(
        outputs,
        reader,
        out_tx,
        active_stage,
        activation_factory,
        image,
        framebuffer,
        event_cb,
    )
    .await
}

/// Convert UI input into IronRDP operations, returning the last mouse position seen.
fn translate_input(
    events: Vec<InputEvent>,
    swap_cmd_alt: bool,
) -> (Vec<Operation>, Option<(u16, u16)>) {
    let mut ops = Vec::with_capacity(events.len() + 2);
    let mut last_mouse = None;
    for event in events {
        match event {
            InputEvent::Key { keycode, down } => {
                if let Some(sc) = keymap::mac_keycode_to_scancode(keycode, swap_cmd_alt) {
                    let scan = Scancode::from_u8(sc.extended, sc.code as u8);
                    ops.push(if down {
                        Operation::KeyPressed(scan)
                    } else {
                        Operation::KeyReleased(scan)
                    });
                }
            }
            InputEvent::MouseMove { x, y } => {
                last_mouse = Some((x, y));
                ops.push(Operation::MouseMove(MousePosition { x, y }));
            }
            InputEvent::MouseButton { button, down, x, y } => {
                last_mouse = Some((x, y));
                ops.push(Operation::MouseMove(MousePosition { x, y }));
                let b = match button {
                    PointerButton::Left => MouseButton::Left,
                    PointerButton::Right => MouseButton::Right,
                    PointerButton::Middle => MouseButton::Middle,
                };
                ops.push(if down {
                    Operation::MouseButtonPressed(b)
                } else {
                    Operation::MouseButtonReleased(b)
                });
            }
            InputEvent::Wheel { delta, horizontal } => {
                ops.push(Operation::WheelRotations(WheelRotations {
                    is_vertical: !horizontal,
                    rotation_units: delta,
                }));
            }
        }
    }
    (ops, last_mouse)
}

fn build_config(config: &SessionConfig, audio_active: bool) -> Config {
    let bitmap = Some(BitmapConfig {
        lossy_compression: true,
        color_depth: config.color_depth,
        codecs: client_codecs_capabilities(&[]).expect("default codecs"),
    });
    Config {
        desktop_size: DesktopSize {
            width: config.width,
            height: config.height,
        },
        desktop_scale_factor: config.scale.unwrap_or(100),
        enable_tls: false,
        enable_credssp: config.authentication == AuthenticationMode::Password,
        enable_rdsaad: config.authentication == AuthenticationMode::EntraWeb,
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        client_build: 0,
        client_name: "RDP123".to_string(),
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        ime_file_name: String::new(),
        bitmap,
        dig_product_id: String::new(),
        client_dir: String::new(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        platform: MajorPlatformType::MACINTOSH,
        hardware_id: None,
        request_data: None,
        autologon: false,
        // ThisComputer: announced only when the local output is actually ready
        // (false sets INFO_NOAUDIOPLAYBACK, telling the server to discard).
        // RemoteComputer: don't suppress, but no redirection channel either —
        // IronRDP does not expose INFO_REMOTECONSOLEAUDIO for true console audio.
        enable_audio_playback: match config.audio {
            AudioMode::ThisComputer => audio_active,
            AudioMode::RemoteComputer => true,
            AudioMode::Never => false,
        },
        // From our vendored connector patch: advertises the Graphics Pipeline.
        support_gfx: config.graphics == GraphicsMode::Egfx,
        // Ask the host to render text with ClearType/anti-aliasing; without this
        // fonts come across rough and un-smoothed.
        performance_flags: PerformanceFlags::ENABLE_FONT_SMOOTHING,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: config.compression.then_some(CompressionType::Rdp61),
        // Do not composite the server pointer into the framebuffer: that produces
        // a laggy remote cursor that lingers. Pointer shapes are decoded to RGBA
        // and applied to the native macOS cursor instead (see drain_outputs).
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
    }
}

fn fingerprint_hex(public_key: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    let digest = hasher.finalize();
    let mut out = String::from("sha256:");
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn connector_err(e: ConnectorError) -> anyhow::Error {
    tracing::debug!("RDP connector failure: {}", e.report());
    let message = match e.kind() {
        ConnectorErrorKind::Decode(_) => {
            "The remote computer closed the connection or sent an incomplete RDP response. \
             It may be offline, restarting, or not ready to accept RDP connections yet."
                .to_string()
        }
        ConnectorErrorKind::Credssp(_) if connector_credentials_rejected(e.kind()) => {
            "The remote computer rejected the sign-in. Check the username, password, and account \
             permissions."
                .to_string()
        }
        ConnectorErrorKind::Credssp(_) => {
            "The remote computer is not ready to complete authentication yet.".to_string()
        }
        ConnectorErrorKind::AccessDenied => {
            "The remote computer rejected the sign-in. Check the username, password, and account \
             permissions."
                .to_string()
        }
        ConnectorErrorKind::Negotiation(_) => {
            "The remote computer rejected the requested RDP security protocol.".to_string()
        }
        ConnectorErrorKind::Reason(description) => {
            format!("The remote computer rejected the connection: {description}")
        }
        _ => "The RDP connection could not be established.".to_string(),
    };
    anyhow!(message)
}

fn connector_credentials_rejected(kind: &ConnectorErrorKind) -> bool {
    match kind {
        ConnectorErrorKind::AccessDenied => true,
        ConnectorErrorKind::Credssp(error) => matches!(
            error.error_type,
            SspiErrorKind::LogonDenied
                | SspiErrorKind::UnknownCredentials
                | SspiErrorKind::NoCredentials
                | SspiErrorKind::IncompleteCredentials
                | SspiErrorKind::WrongCredentialHandle
        ),
        _ => false,
    }
}

fn user_facing_disconnect_reason(reason: &str) -> String {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("operation timed out") || lower.contains("connection timed out") {
        return "The remote computer did not respond in time.".to_string();
    }
    if lower.contains("credssp") {
        if lower.contains("logondenied")
            || lower.contains("logon denied")
            || lower.contains("access denied")
            || lower.contains("unknown credentials")
        {
            return "The remote computer rejected the sign-in. Check the username, password, and \
                    account permissions."
                .to_string();
        }
        return "The remote computer is not ready to complete authentication yet.".to_string();
    }
    if lower.contains("decode error") || lower.contains("[connector error @") {
        return "The remote computer closed the connection or sent an incomplete RDP response. \
                It may be offline, restarting, or not ready to accept RDP connections yet."
            .to_string();
    }

    let mut cleaned = reason.to_string();
    while let Some(start) = cleaned.find('[') {
        let Some(relative_end) = cleaned[start..].find(']') else {
            break;
        };
        let end = start + relative_end + 1;
        if !cleaned[start..end].contains(" @ ") {
            break;
        }
        let remove_end = if cleaned.as_bytes().get(end) == Some(&b' ') {
            end + 1
        } else {
            end
        };
        cleaned.replace_range(start..remove_end, "");
    }
    while let Some(start) = cleaned.find("(os error ") {
        let Some(relative_end) = cleaned[start..].find(')') else {
            break;
        };
        let end = start + relative_end + 1;
        let remove_start = if start > 0 && cleaned.as_bytes()[start - 1] == b' ' {
            start - 1
        } else {
            start
        };
        cleaned.replace_range(remove_start..end, "");
    }
    cleaned
}

/// An incomplete RDP or CredSSP response commonly means that a host accepted
/// TCP while Windows was still starting or shutting down. Explicit credential
/// rejection is fatal on the initial connection. After this session has
/// already authenticated successfully, it is safe to treat a short-lived
/// CredSSP rejection during reboot as part of the bounded reconnect loop.
fn connector_failure(
    e: ConnectorError,
    retry_authentication_during_reconnect: bool,
) -> ConnectFailure {
    let retryable = match e.kind() {
        ConnectorErrorKind::Decode(_) => true,
        ConnectorErrorKind::Credssp(_) | ConnectorErrorKind::AccessDenied => {
            retry_authentication_during_reconnect || !connector_credentials_rejected(e.kind())
        }
        _ => false,
    };
    let error = connector_err(e);
    if retryable {
        ConnectFailure::retryable(error)
    } else {
        ConnectFailure::fatal(error)
    }
}

fn ensure_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// CredSSP with password auth (NTLM) needs no out-of-band network access; this
/// stub errors if the connector ever tries a Kerberos KDC request.
#[derive(Debug)]
struct NoNetworkClient;

impl NetworkClient for NoNetworkClient {
    fn send(
        &mut self,
        _request: &NetworkRequest,
    ) -> impl std::future::Future<Output = ConnectorResult<Vec<u8>>> {
        std::future::ready(Err(ConnectorError::general(
            "out-of-band network requests (Kerberos) are not supported",
        )))
    }
}

/// A clipboard backend bridging CLIPRDR to `NSPasteboard` (via the app):
/// Unicode text in both directions plus local files offered to the remote
/// (file streams, MS-RDPECLIP 3.1.1.4).
#[derive(Debug)]
struct MacClipboardBackend {
    tx: Sender<ClipSignal>,
    local_clip: LocalClipState,
    tmp: String,
    mode: ClipboardMode,
}

impl MacClipboardBackend {
    fn queue_signal(&self, signal: ClipSignal) {
        if let Err(error) = self.tx.try_send(signal) {
            tracing::warn!("clipboard: signal queue is full or closed: {error}");
        }
    }

    fn queue_file_contents_response(&self, request: FileContentsRequest) {
        let tx = self.tx.clone();
        let file = local_file_for_request(&self.local_clip, self.mode, &request);
        let stream_id = request.stream_id;
        tokio::spawn(async move {
            let Ok(permit) = LOCAL_FILE_READ_PERMITS.acquire().await else {
                return;
            };
            let response = tokio::task::spawn_blocking(move || {
                local_file_contents_response(file.as_ref(), &request)
            })
            .await
            .unwrap_or_else(|error| {
                tracing::warn!("clipboard: local file read task failed: {error}");
                FileContentsResponse::new_error(stream_id)
            });
            drop(permit);
            if tx
                .send(ClipSignal::SubmitFileContents(response))
                .await
                .is_err()
            {
                tracing::debug!("clipboard: session ended before a file response was sent");
            }
        });
    }
}

/// Snapshot the requested entry while still handling the matching RDP PDU, so
/// asynchronous disk work cannot accidentally resolve the same index against
/// a newer clipboard selection.
fn local_file_for_request(
    local_clip: &LocalClipState,
    mode: ClipboardMode,
    request: &FileContentsRequest,
) -> Option<LocalClipFile> {
    if !mode.allow_local_to_remote() {
        return None;
    }
    let clip = local_clip.lock().unwrap();
    let LocalClip::Files(files) = &clip.clip else {
        return None;
    };
    let file = usize::try_from(request.index)
        .ok()
        .and_then(|index| files.get(index))
        .cloned();
    if file.is_none() {
        tracing::warn!(
            "clipboard: FileContents request for unknown index {}",
            request.index
        );
    }
    file
}

/// Read one requested local file range without holding the clipboard mutex.
/// This runs on Tokio's blocking pool so filesystem latency cannot freeze RDP
/// decoding, input, audio, or keep-alive processing.
fn local_file_contents_response(
    file: Option<&LocalClipFile>,
    request: &FileContentsRequest,
) -> FileContentsResponse<'static> {
    use std::io::{Read as _, Seek as _};

    let error = FileContentsResponse::new_error(request.stream_id);
    let Some(file) = file else { return error };

    if request.flags.contains(FileContentsFlags::SIZE) {
        return FileContentsResponse::new_size_response(request.stream_id, file.size);
    }
    if !request.flags.contains(FileContentsFlags::RANGE) || file.is_dir {
        return error;
    }

    let requested = request.requested_size.min(MAX_FILE_CHUNK_BYTES) as usize;
    let mut open = match std::fs::File::open(&file.path) {
        Ok(open) => open,
        Err(e) => {
            tracing::warn!("clipboard: cannot open {}: {e}", file.path.display());
            return error;
        }
    };
    if let Err(e) = open.seek(std::io::SeekFrom::Start(request.position)) {
        tracing::warn!("clipboard: seek in {} failed: {e}", file.path.display());
        return error;
    }
    let mut data = vec![0u8; requested];
    let mut filled = 0usize;
    while filled < requested {
        match open.read(&mut data[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => {
                tracing::warn!("clipboard: read from {} failed: {e}", file.path.display());
                return error;
            }
        }
    }
    data.truncate(filled);
    FileContentsResponse::new_data_response(request.stream_id, data)
}

impl ironrdp::core::AsAny for MacClipboardBackend {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl CliprdrBackend for MacClipboardBackend {
    fn temporary_directory(&self) -> &str {
        &self.tmp
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // File streams for Finder -> Explorer copies; names stay relative.
        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
    }

    fn on_ready(&mut self) {
        self.local_clip.lock().unwrap().mark_ready();
    }

    fn on_request_format_list(&mut self) {
        // IronRDP requires the initialization FormatList to go through
        // `initiate_copy`; file offers are sent after `on_ready` instead.
        self.queue_signal(ClipSignal::InitializeClipboard);
    }

    fn on_format_list_response(&mut self, ok: bool) {
        let result = self.local_clip.lock().unwrap().complete_offer(ok);
        match result {
            LocalClipboardOfferResult::None => {}
            LocalClipboardOfferResult::PasteReady { generation } => {
                self.queue_signal(ClipSignal::PasteAccepted { generation });
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(EXTERNAL_PASTE_DATA_TIMEOUT).await;
                    if tx
                        .send(ClipSignal::PasteDataTimedOut { generation })
                        .await
                        .is_err()
                    {
                        tracing::debug!("clipboard: session ended before the STT data timeout");
                    }
                });
            }
            LocalClipboardOfferResult::AdvertiseCurrent { generation } => {
                self.queue_signal(ClipSignal::AdvertiseLocal { generation });
            }
            LocalClipboardOfferResult::Retry { generation, delay } => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    if tx
                        .send(ClipSignal::AdvertiseLocal { generation })
                        .await
                        .is_err()
                    {
                        tracing::debug!("clipboard: session ended before a retry was sent");
                    }
                });
            }
            LocalClipboardOfferResult::RetryInitialization { delay } => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    if tx.send(ClipSignal::InitializeClipboard).await.is_err() {
                        tracing::debug!(
                            "clipboard: session ended before initialization could be retried"
                        );
                    }
                });
            }
        }
    }

    fn on_process_negotiated_capabilities(&mut self, _caps: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // Fetch what the remote copied only if remote -> local is allowed.
        // A file copy wins over the file-name text that accompanies it.
        if !self.mode.allow_remote_to_local() {
            return;
        }
        let file_list = available_formats.iter().find(|f| {
            f.name()
                .is_some_and(|name| name.value() == "FileGroupDescriptorW")
        });
        if let Some(format) = file_list {
            self.queue_signal(ClipSignal::InitiatePaste(format.id()));
        } else if available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT)
        {
            self.queue_signal(ClipSignal::InitiatePaste(ClipboardFormatId::CF_UNICODETEXT));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let (response, paste_generation) = if self.mode.allow_local_to_remote()
            && request.format == ClipboardFormatId::CF_UNICODETEXT
        {
            let mut clip = self.local_clip.lock().unwrap();
            let text = match &clip.clip {
                LocalClip::Text(text) => text.clone(),
                _ => String::new(),
            };
            let crlf = normalize_clipboard_to_crlf(&text);
            (
                FormatDataResponse::new_unicode_string(&crlf),
                clip.begin_paste_data_response(),
            )
        } else {
            (FormatDataResponse::new_error(), None)
        };
        self.queue_signal(ClipSignal::SubmitData {
            response,
            paste_generation,
        });
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() || !self.mode.allow_remote_to_local() {
            return;
        }
        if let Ok(text) = response.to_unicode_string() {
            if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                tracing::warn!(
                    "remote clipboard text is too large to accept ({} bytes)",
                    text.len()
                );
                return;
            }
            self.queue_signal(ClipSignal::RemoteText(text.replace("\r\n", "\n")));
        }
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        self.queue_file_contents_response(request);
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let data = if response.is_error() {
            None
        } else {
            Some(response.data().to_vec())
        };
        self.queue_signal(ClipSignal::RemoteFileContents {
            stream_id: response.stream_id(),
            data,
        });
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        if !self.mode.allow_remote_to_local() {
            return;
        }
        let entries: Vec<RemoteFileEntry> = files
            .iter()
            .map(|descriptor| RemoteFileEntry {
                wire_name: match descriptor.relative_path.as_deref() {
                    Some(path) if !path.is_empty() => format!("{path}\\{}", descriptor.name),
                    _ => descriptor.name.clone(),
                },
                size: descriptor.file_size,
                is_dir: descriptor
                    .attributes
                    .is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY)),
            })
            .collect();
        self.queue_signal(ClipSignal::RemoteFileList {
            files: entries,
            data_id: clip_data_id,
        });
    }

    // Lock snapshots for outgoing copies are managed inside ironrdp-cliprdr.
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

fn normalize_clipboard_to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::{
        absorb_offline_command, apply_remote_file_contents, build_config, connector_failure,
        create_remote_clipboard_cache_dir, initial_keyboard_sync_event, mdns_fallback_hostname,
        next_remote_fetch_action, normalize_clipboard_to_crlf, plan_remote_clipboard_cache,
        remote_clipboard_paste_input_events, remote_top_level_destination, resolve_pending_command,
        to_file_descriptors, update_keys_down, user_facing_disconnect_reason,
        validate_remote_file_range, ClipSignal, InputEvent, LocalClip, LocalClipFile,
        LocalClipState, LocalClipboardOfferResult, LocalClipboardState, MacClipboardBackend,
        PendingCommands, RemoteClipboard, RemoteFetchAction, RemoteFileEntry, SessionCommand,
        SessionConfig, SessionHandle, CLIPBOARD_RETRY_DELAYS,
    };
    use crate::profile::{AudioMode, AuthenticationMode, ClipboardMode, GraphicsMode};
    use ironrdp::cliprdr::backend::CliprdrBackend;
    use ironrdp::cliprdr::pdu::{
        ClipboardFormatId, FileContentsFlags, FileContentsRequest, FormatDataRequest,
    };
    use ironrdp::cliprdr::{Cliprdr, CliprdrClient, CliprdrServer, Role};
    use ironrdp::connector::sspi::{Error as SspiError, ErrorKind as SspiErrorKind};
    use ironrdp::connector::{ConnectorError, ConnectorErrorExt};
    use ironrdp::core::{not_enough_bytes_err, DecodeError};
    use ironrdp::input::{Database, Operation, Scancode};
    use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags, SynchronizeFlags};
    use ironrdp::pdu::rdp::client_info::CompressionType;
    use ironrdp::svc::{SvcMessage, SvcProcessor};
    use std::sync::atomic::Ordering;

    fn test_session_config(graphics: GraphicsMode, compression: bool) -> SessionConfig {
        SessionConfig {
            host: "example.test".to_string(),
            port: 3389,
            username: "user".to_string(),
            password: "password".to_string(),
            domain: None,
            authentication: AuthenticationMode::Password,
            width: 1280,
            height: 720,
            scale: Some(100),
            expected_fingerprint: None,
            color_depth: 32,
            compression,
            clipboard: ClipboardMode::Disabled,
            audio: AudioMode::Never,
            graphics,
            avc444: true,
            dynamic_resolution: false,
            reconnect: false,
            reconnect_per_minute: 0,
            swap_cmd_alt: false,
            wake_mac: None,
            keep_alive: false,
        }
    }

    fn key(down: bool) -> InputEvent {
        InputEvent::Key { keycode: 0, down }
    }

    fn deliver_cliprdr<R: Role>(
        messages: Vec<SvcMessage>,
        receiver: &mut Cliprdr<R>,
    ) -> Vec<SvcMessage> {
        let mut replies = Vec::new();
        for message in messages {
            let payload = message.encode_unframed_pdu().unwrap();
            replies.extend(receiver.process(&payload).unwrap());
        }
        replies
    }

    #[test]
    fn clipboard_line_endings_are_normalized_without_doubling_crlf() {
        assert_eq!(
            normalize_clipboard_to_crlf("one\r\ntwo\nthree\rfour"),
            "one\r\ntwo\r\nthree\r\nfour"
        );
    }

    #[test]
    fn file_clipboard_waits_for_cliprdr_initialization() {
        let mut state = LocalClipboardState::default();
        let generation = state.replace(LocalClip::Files(vec![LocalClipFile {
            path: "/tmp/report.txt".into(),
            wire_name: "report.txt".to_string(),
            size: 7,
            is_dir: false,
        }]));

        let formats = state
            .begin_initial_offer()
            .expect("file clipboard needs a generic initialization offer");
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].id(), ClipboardFormatId::CF_UNICODETEXT);
        assert!(!state.is_ready());

        state.mark_ready();
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::AdvertiseCurrent { generation }
        );
        assert!(matches!(
            state.begin_offer(generation),
            Some(super::LocalClipboardOffer::Files(_))
        ));
    }

    #[test]
    fn reconnect_reinitializes_before_readvertising_files() {
        let mut state = LocalClipboardState::default();
        state.mark_ready();
        let generation = state.replace(LocalClip::Files(vec![LocalClipFile {
            path: "/tmp/report.txt".into(),
            wire_name: "report.txt".to_string(),
            size: 7,
            is_dir: false,
        }]));
        assert!(state.begin_offer(generation).is_some());

        state.reset_connection();
        assert!(!state.is_ready());
        assert!(state.begin_initial_offer().is_some());
    }

    #[tokio::test]
    async fn rejected_local_clipboard_offer_is_retried() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut state = LocalClipboardState::default();
        let generation = state.replace(LocalClip::Text("retry me".into()));
        assert!(state.begin_offer(generation).is_some());
        let local_clip: LocalClipState = std::sync::Arc::new(std::sync::Mutex::new(state));
        let mut backend = MacClipboardBackend {
            tx,
            local_clip,
            tmp: "/tmp".to_string(),
            mode: ClipboardMode::Bidirectional,
        };

        backend.on_format_list_response(false);

        let signal = tokio::time::timeout(std::time::Duration::from_millis(250), rx.recv())
            .await
            .expect("a rejected clipboard offer must be retried")
            .expect("clipboard signal channel must remain open");
        assert!(matches!(
            signal,
            ClipSignal::AdvertiseLocal {
                generation: retry_generation
            } if retry_generation == generation
        ));
    }

    #[tokio::test]
    async fn accepted_local_clipboard_offer_is_not_retried() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut state = LocalClipboardState::default();
        let generation = state.replace(LocalClip::Text("accepted".into()));
        assert!(state.begin_offer(generation).is_some());
        let local_clip: LocalClipState = std::sync::Arc::new(std::sync::Mutex::new(state));
        let mut backend = MacClipboardBackend {
            tx,
            local_clip,
            tmp: "/tmp".to_string(),
            mode: ClipboardMode::Bidirectional,
        };

        backend.on_format_list_response(true);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(80), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn pipelined_windows_file_requests_are_never_dropped() {
        let root = std::env::temp_dir().join(format!(
            "rdp123-file-response-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&root, b"abc").unwrap();
        let mut state = LocalClipboardState::default();
        state.replace(LocalClip::Files(vec![LocalClipFile {
            path: root.clone(),
            wire_name: "test.txt".to_string(),
            size: 3,
            is_dir: false,
        }]));
        // A deliberately small queue reproduces Windows pipelining multiple
        // requests before the session loop has drained the first response.
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut backend = MacClipboardBackend {
            tx,
            local_clip: std::sync::Arc::new(std::sync::Mutex::new(state)),
            tmp: "/tmp".to_string(),
            mode: ClipboardMode::Bidirectional,
        };

        for stream_id in 1..=64 {
            backend.on_file_contents_request(FileContentsRequest {
                stream_id,
                index: 0,
                flags: FileContentsFlags::RANGE,
                position: 0,
                requested_size: 1,
                data_id: None,
            });
        }

        let mut received = Vec::new();
        for _ in 0..64 {
            let signal = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .expect("every Windows file request must receive a response")
                .expect("clipboard response channel must remain open");
            let ClipSignal::SubmitFileContents(response) = signal else {
                panic!("expected a file contents response");
            };
            received.push(response.stream_id());
        }
        received.sort_unstable();
        assert_eq!(received, (1..=64).collect::<Vec<_>>());

        let _ = std::fs::remove_file(root);
    }

    #[tokio::test]
    async fn windows_file_copy_reaches_a_complete_local_cache_through_cliprdr() {
        let source = std::env::temp_dir().join(format!(
            "rdp123-remote-source-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&source, b"from windows").unwrap();
        let source_file = LocalClipFile {
            path: source.clone(),
            wire_name: "report.txt".to_string(),
            size: 12,
            is_dir: false,
        };

        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel(8);
        let (server_tx, mut server_rx) = tokio::sync::mpsc::channel(8);
        let client_state: LocalClipState = Default::default();
        let mut server_state = LocalClipboardState::default();
        server_state.replace(LocalClip::Files(vec![source_file.clone()]));
        let server_state: LocalClipState = std::sync::Arc::new(std::sync::Mutex::new(server_state));
        let mut client = CliprdrClient::new(Box::new(MacClipboardBackend {
            tx: client_tx,
            local_clip: client_state.clone(),
            tmp: "/tmp".to_string(),
            mode: ClipboardMode::Bidirectional,
        }));
        let mut server = CliprdrServer::new(Box::new(MacClipboardBackend {
            tx: server_tx,
            local_clip: server_state,
            tmp: "/tmp".to_string(),
            mode: ClipboardMode::Bidirectional,
        }));

        // Complete the real CLIPRDR client/server initialization handshake.
        let replies = deliver_cliprdr(server.start().unwrap(), &mut client);
        assert!(replies.is_empty());
        assert!(matches!(
            client_rx.recv().await,
            Some(ClipSignal::InitializeClipboard)
        ));
        let initial_formats = client_state.lock().unwrap().begin_initial_offer().unwrap();
        let initial: Vec<_> = client.initiate_copy(&initial_formats).unwrap().into();
        let replies = deliver_cliprdr(initial, &mut server);
        let trailing = deliver_cliprdr(replies, &mut client);
        assert!(trailing.is_empty());
        while server_rx.try_recv().is_ok() {
            // The simulated server backend observes the client's initial text
            // offer; it is unrelated to the file transfer under test.
        }

        // The simulated Windows side advertises a copied file. RDP123 requests
        // its file list using IronRDP's normal delayed-rendering exchange.
        let remote_offer: Vec<_> = server
            .initiate_file_copy(to_file_descriptors(&[source_file]))
            .unwrap()
            .into();
        let acknowledgements = deliver_cliprdr(remote_offer, &mut client);
        let trailing = deliver_cliprdr(acknowledgements, &mut server);
        assert!(trailing.is_empty());
        let format = match client_rx.recv().await {
            Some(ClipSignal::InitiatePaste(format)) => format,
            _ => panic!("expected remote file-list request"),
        };
        let list_request: Vec<_> = client.initiate_paste(format).unwrap().into();
        let list_response = deliver_cliprdr(list_request, &mut server);
        let trailing = deliver_cliprdr(list_response, &mut client);
        assert!(trailing.is_empty());
        let (files, data_id) = match client_rx.recv().await {
            Some(ClipSignal::RemoteFileList { files, data_id }) => (files, data_id),
            _ => panic!("expected remote file list"),
        };

        // Exercise the exact RDP123 download state machine until it publishes
        // a fully materialized top-level file.
        let names = vec!["report.txt".to_string()];
        let mut remote = RemoteClipboard {
            files,
            data_id,
            ..RemoteClipboard::default()
        };
        let cache_dir = create_remote_clipboard_cache_dir().unwrap();
        remote
            .jobs
            .push_back(plan_remote_clipboard_cache(&remote, &names, cache_dir.clone()).unwrap());

        let ready_paths = loop {
            match next_remote_fetch_action(&mut remote).unwrap() {
                RemoteFetchAction::Idle => panic!("download stalled before completion"),
                RemoteFetchAction::Ready(paths) => break paths,
                RemoteFetchAction::Request(request) => {
                    assert_ne!(request.stream_id, 0);
                    let request_messages: Vec<_> =
                        client.request_file_contents(request).unwrap().into();
                    let immediate = deliver_cliprdr(request_messages, &mut server);
                    assert!(immediate.is_empty());
                    let response = match server_rx.recv().await {
                        Some(ClipSignal::SubmitFileContents(response)) => response,
                        _ => panic!("expected Windows file response"),
                    };
                    let response_messages: Vec<_> =
                        server.submit_file_contents(response).unwrap().into();
                    let trailing = deliver_cliprdr(response_messages, &mut client);
                    assert!(trailing.is_empty());
                    let (stream_id, data) = match client_rx.recv().await {
                        Some(ClipSignal::RemoteFileContents { stream_id, data }) => {
                            (stream_id, data)
                        }
                        _ => panic!("expected downloaded file data"),
                    };
                    apply_remote_file_contents(&mut remote, stream_id, data).unwrap();
                }
            }
        };

        assert_eq!(ready_paths, [cache_dir.join("report.txt")]);
        assert_eq!(std::fs::read(&ready_paths[0]).unwrap(), b"from windows");
        let _ = std::fs::remove_dir_all(cache_dir);
        let _ = std::fs::remove_file(source);
    }

    #[test]
    fn stale_clipboard_rejection_does_not_retry_a_newer_copy() {
        let mut state = LocalClipboardState::default();
        let old_generation = state.replace(LocalClip::Text("old".into()));
        assert!(state.begin_offer(old_generation).is_some());
        let new_generation = state.replace(LocalClip::Text("new".into()));

        assert_eq!(
            state.complete_offer(false),
            LocalClipboardOfferResult::AdvertiseCurrent {
                generation: new_generation
            }
        );
        assert!(state.begin_offer(new_generation).is_some());
    }

    #[test]
    fn stt_runtime_regression_tracks_the_latest_format_list_offer() {
        let mut state = LocalClipboardState::default();
        let ordinary_generation = state.replace(LocalClip::Text("ordinary clipboard".into()));
        assert!(state.begin_offer(ordinary_generation).is_some());

        let paste_generation = state.replace_for_paste("dictated text".into());

        assert!(
            state.begin_offer(paste_generation).is_some(),
            "a stale initialization offer must not block the explicit STT paste"
        );
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::PasteReady {
                generation: paste_generation
            }
        );
    }

    #[test]
    fn clipboard_retries_are_bounded() {
        let mut state = LocalClipboardState::default();
        let generation = state.replace(LocalClip::Text("bounded".into()));

        for _ in CLIPBOARD_RETRY_DELAYS {
            assert!(state.begin_offer(generation).is_some());
            assert!(matches!(
                state.complete_offer(false),
                LocalClipboardOfferResult::Retry { .. }
            ));
        }
        assert!(state.begin_offer(generation).is_some());
        assert_eq!(state.complete_offer(false), LocalClipboardOfferResult::None);
    }

    #[test]
    fn rejected_external_stt_paste_restores_the_deferred_clipboard() {
        let mut state = LocalClipboardState::default();
        let generation = state.replace_for_paste("dictated text".into());
        assert!(state.begin_offer(generation).is_some());
        state.replace(LocalClip::Text("restored original".into()));

        for _ in CLIPBOARD_RETRY_DELAYS {
            assert!(matches!(
                state.complete_offer(false),
                LocalClipboardOfferResult::Retry { .. }
            ));
            assert!(state.begin_offer(generation).is_some());
        }

        let result = state.complete_offer(false);
        let LocalClipboardOfferResult::AdvertiseCurrent {
            generation: restored_generation,
        } = result
        else {
            panic!("expected the restored clipboard to be advertised");
        };
        assert_ne!(restored_generation, generation);
        assert!(matches!(
            &state.clip,
            LocalClip::Text(text) if text == "restored original"
        ));
    }

    #[test]
    fn external_stt_paste_waits_for_clipboard_acceptance() {
        let mut state = LocalClipboardState::default();
        let generation = state.replace_for_paste("dictated text".into());

        assert!(state.begin_offer(generation).is_some());
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::PasteReady { generation }
        );
        assert!(state.consume_confirmed_paste(generation));
    }

    #[test]
    fn newer_clipboard_text_waits_until_external_stt_data_is_sent() {
        let mut state = LocalClipboardState::default();
        let generation = state.replace_for_paste("dictated text".into());
        assert!(state.begin_offer(generation).is_some());
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::PasteReady { generation }
        );

        state.replace(LocalClip::Text("newer clipboard text".into()));

        assert!(state.consume_confirmed_paste(generation));
        let deferred_generation = state
            .finish_serving_paste(generation)
            .expect("newer clipboard text must remain queued");
        assert_ne!(deferred_generation, generation);
        assert!(matches!(
            &state.clip,
            LocalClip::Text(text) if text == "newer clipboard text"
        ));
    }

    #[test]
    fn repeated_external_stt_paste_of_same_text_starts_a_fresh_offer() {
        let mut state = LocalClipboardState::default();
        let first = state.replace_for_paste("same text".into());
        assert!(state.begin_offer(first).is_some());

        let second = state.replace_for_paste("same text".into());

        assert_ne!(second, first);
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::AdvertiseCurrent { generation: second }
        );
        assert!(state.begin_offer(second).is_some());
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::PasteReady { generation: second }
        );
    }

    #[test]
    fn normal_clipboard_acceptance_does_not_trigger_remote_paste() {
        let mut state = LocalClipboardState::default();
        let generation = state.replace(LocalClip::Text("ordinary copy".into()));

        assert!(state.begin_offer(generation).is_some());
        assert_eq!(state.complete_offer(true), LocalClipboardOfferResult::None);
    }

    #[test]
    fn clipboard_poller_cannot_cancel_matching_external_stt_paste() {
        let mut state = LocalClipboardState::default();
        let generation = state.replace_for_paste("dictated text".into());
        assert!(state.begin_offer(generation).is_some());

        let polled_generation = state.replace(LocalClip::Text("dictated text".into()));

        assert_eq!(polled_generation, generation);
        assert!(state.begin_offer(polled_generation).is_none());
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::PasteReady { generation }
        );
    }

    #[test]
    fn stt_runtime_regression_clipboard_restore_cannot_cancel_pending_paste() {
        let mut state = LocalClipboardState::default();
        let generation = state.replace_for_paste("dictated text".into());
        assert!(state.begin_offer(generation).is_some());

        let polled_generation = state.replace(LocalClip::Text("restored original".into()));

        assert_eq!(polled_generation, generation);
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::PasteReady { generation }
        );
        assert!(state.consume_confirmed_paste(generation));
        state
            .finish_serving_paste(generation)
            .expect("the original clipboard must be restored after the paste");
        assert!(matches!(
            &state.clip,
            LocalClip::Text(text) if text == "restored original"
        ));
    }

    #[test]
    fn stt_runtime_regression_dictated_text_survives_until_remote_data_request() {
        let mut state = LocalClipboardState::default();
        let generation = state.replace_for_paste("dictated text".into());
        assert!(state.begin_offer(generation).is_some());
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::PasteReady { generation }
        );
        assert!(state.consume_confirmed_paste(generation));

        let polled_generation = state.replace(LocalClip::Text("restored original".into()));

        assert_eq!(polled_generation, generation);
        assert!(matches!(
            &state.clip,
            LocalClip::Text(text) if text == "dictated text"
        ));
        let restored_generation = state
            .finish_serving_paste(generation)
            .expect("the original clipboard must be restored after serving the dictated text");
        assert_ne!(restored_generation, generation);
        assert!(matches!(
            &state.clip,
            LocalClip::Text(text) if text == "restored original"
        ));
    }

    #[test]
    fn external_stt_timeout_restores_clipboard_only_without_a_data_request() {
        let mut requested = LocalClipboardState::default();
        let requested_generation = requested.replace_for_paste("requested text".into());
        assert!(requested.begin_offer(requested_generation).is_some());
        assert_eq!(
            requested.complete_offer(true),
            LocalClipboardOfferResult::PasteReady {
                generation: requested_generation
            }
        );
        assert!(requested.consume_confirmed_paste(requested_generation));
        requested.replace(LocalClip::Text("requested original".into()));
        assert_eq!(
            requested.begin_paste_data_response(),
            Some(requested_generation)
        );
        assert_eq!(
            requested.expire_unrequested_paste(requested_generation),
            None
        );
        assert!(matches!(
            &requested.clip,
            LocalClip::Text(text) if text == "requested text"
        ));

        let mut unrequested = LocalClipboardState::default();
        let unrequested_generation = unrequested.replace_for_paste("unrequested text".into());
        assert!(unrequested.begin_offer(unrequested_generation).is_some());
        assert_eq!(
            unrequested.complete_offer(true),
            LocalClipboardOfferResult::PasteReady {
                generation: unrequested_generation
            }
        );
        assert!(unrequested.consume_confirmed_paste(unrequested_generation));
        unrequested.replace(LocalClip::Text("unrequested original".into()));
        assert!(unrequested
            .expire_unrequested_paste(unrequested_generation)
            .is_some());
        assert!(matches!(
            &unrequested.clip,
            LocalClip::Text(text) if text == "unrequested original"
        ));
    }

    #[tokio::test]
    async fn external_stt_paste_is_signalled_only_after_clipboard_acceptance() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut state = LocalClipboardState::default();
        let generation = state.replace_for_paste("dictated text".into());
        assert!(state.begin_offer(generation).is_some());
        let local_clip: LocalClipState = std::sync::Arc::new(std::sync::Mutex::new(state));
        let mut backend = MacClipboardBackend {
            tx,
            local_clip,
            tmp: "/tmp".to_string(),
            mode: ClipboardMode::Bidirectional,
        };

        backend.on_format_list_response(true);

        let signal = rx
            .recv()
            .await
            .expect("accepted STT paste must be signalled");
        assert!(matches!(
            signal,
            ClipSignal::PasteAccepted {
                generation: accepted
            } if accepted == generation
        ));
    }

    #[tokio::test]
    async fn external_stt_data_request_serves_dictated_text_before_restore() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut state = LocalClipboardState::default();
        let generation = state.replace_for_paste("dictated text".into());
        assert!(state.begin_offer(generation).is_some());
        assert_eq!(
            state.complete_offer(true),
            LocalClipboardOfferResult::PasteReady { generation }
        );
        assert!(state.consume_confirmed_paste(generation));
        state.replace(LocalClip::Text("restored original".into()));
        let local_clip: LocalClipState = std::sync::Arc::new(std::sync::Mutex::new(state));
        let mut backend = MacClipboardBackend {
            tx,
            local_clip: local_clip.clone(),
            tmp: "/tmp".to_string(),
            mode: ClipboardMode::Bidirectional,
        };

        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });

        let signal = rx
            .recv()
            .await
            .expect("the dictated clipboard data must be submitted");
        let ClipSignal::SubmitData {
            response,
            paste_generation,
        } = signal
        else {
            panic!("expected a clipboard data response");
        };
        assert_eq!(paste_generation, Some(generation));
        assert_eq!(
            response
                .to_unicode_string()
                .expect("the response must contain Unicode text"),
            "dictated text"
        );

        let restored_generation = local_clip
            .lock()
            .unwrap()
            .finish_serving_paste(generation)
            .expect("the original clipboard must be restored after the response");
        assert_ne!(restored_generation, generation);
        assert!(matches!(
            &local_clip.lock().unwrap().clip,
            LocalClip::Text(text) if text == "restored original"
        ));
    }

    #[test]
    fn full_session_queue_reports_clipboard_delivery_failure() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        command_tx
            .try_send(SessionCommand::Input(Vec::new()))
            .unwrap();
        let handle = SessionHandle {
            command_tx,
            framebuffer: crate::SharedFramebuffer::new(),
            pending: std::sync::Arc::new(PendingCommands::default()),
        };

        assert!(!handle.try_command(SessionCommand::LocalClipboard("must remain pending".into())));
    }

    #[test]
    fn clipboard_change_during_reconnect_is_kept_for_the_next_connection() {
        let mut config = test_session_config(GraphicsMode::Classic, true);
        let local_clip: LocalClipState =
            std::sync::Arc::new(std::sync::Mutex::new(LocalClipboardState::default()));

        absorb_offline_command(
            Some(SessionCommand::LocalClipboard("latest".into())),
            &mut config,
            &local_clip,
        );

        assert!(matches!(
            &local_clip.lock().unwrap().clip,
            LocalClip::Text(text) if text == "latest"
        ));
    }

    #[test]
    fn initial_keyboard_sync_enables_only_num_lock() {
        assert_eq!(
            initial_keyboard_sync_event(),
            FastPathInputEvent::SyncEvent(SynchronizeFlags::NUM_LOCK)
        );
    }

    #[test]
    fn external_stt_paste_releases_held_modifiers_before_ctrl_v() {
        let mut database = Database::new();
        let windows_key = Scancode::from_u8(true, 0x5b);
        let _ = database.apply([Operation::KeyPressed(windows_key)]);

        assert_eq!(
            remote_clipboard_paste_input_events(&mut database),
            vec![
                FastPathInputEvent::KeyboardEvent(
                    KeyboardFlags::RELEASE | KeyboardFlags::EXTENDED,
                    0x5b,
                ),
                FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 0x1d),
                FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 0x2f),
                FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 0x2f),
                FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 0x1d),
            ]
        );
    }

    #[test]
    fn connector_decode_errors_hide_internal_source_locations() {
        let decode: DecodeError = not_enough_bytes_err("test packet", 0, 4);
        let (error, retryable) =
            connector_failure(ConnectorError::decode(decode), false).into_parts();
        let message = error.to_string();

        assert!(retryable);
        assert_eq!(
            message,
            "The remote computer closed the connection or sent an incomplete RDP response. \
             It may be offline, restarting, or not ready to accept RDP connections yet."
        );
        assert!(!message.contains("vendor/"));
        assert!(!message.contains("decode error"));
    }

    #[test]
    fn transient_credssp_failures_retry_without_exposing_source_paths() {
        let transient = ConnectorError::new(
            "CredSSP",
            ironrdp::connector::ConnectorErrorKind::Credssp(SspiError::new(
                SspiErrorKind::InternalError,
                "server is still starting",
            )),
        );
        let (error, retryable) = connector_failure(transient, false).into_parts();
        let message = error.to_string();

        assert!(retryable);
        assert_eq!(
            message,
            "The remote computer is not ready to complete authentication yet."
        );
        assert!(!message.contains("/Users/"));
        assert!(!message.contains(".cargo"));

        let rejected = ConnectorError::new(
            "CredSSP",
            ironrdp::connector::ConnectorErrorKind::Credssp(SspiError::new(
                SspiErrorKind::LogonDenied,
                "bad credentials",
            )),
        );
        let (error, retryable) = connector_failure(rejected, false).into_parts();
        assert!(!retryable);
        assert_eq!(
            error.to_string(),
            "The remote computer rejected the sign-in. Check the username, password, and account permissions."
        );

        let reconnect_rejection = ConnectorError::new(
            "CredSSP",
            ironrdp::connector::ConnectorErrorKind::AccessDenied,
        );
        let (_, retryable) = connector_failure(reconnect_rejection, true).into_parts();
        assert!(retryable);
    }

    #[test]
    fn disconnect_timeouts_hide_operating_system_error_codes() {
        assert_eq!(
            user_facing_disconnect_reason("Operation timed out (os error 60)"),
            "The remote computer did not respond in time."
        );
        assert_eq!(
            user_facing_disconnect_reason("Connection reset by peer (os error 54)"),
            "Connection reset by peer"
        );
        assert_eq!(
            user_facing_disconnect_reason(
                "[connector error @ /Users/patrick/.cargo/registry/src/lib.rs:413] decode error"
            ),
            "The remote computer closed the connection or sent an incomplete RDP response. \
             It may be offline, restarting, or not ready to accept RDP connections yet."
        );
        assert_eq!(
            user_facing_disconnect_reason(
                "[CredSSP @ /Users/patrick/.cargo/registry/src/lib.rs:413] CredSSP, caused by: \
                 InternalError: server is restarting"
            ),
            "The remote computer is not ready to complete authentication yet."
        );
    }

    #[test]
    fn remote_file_range_rejects_more_data_than_requested_or_remaining() {
        assert!(validate_remote_file_range(1_048_576, 2_000_000, 1_048_577).is_err());
        assert!(validate_remote_file_range(1_048_576, 12, 13).is_err());
    }

    #[test]
    fn remote_file_range_accepts_full_and_short_final_chunks() {
        assert!(validate_remote_file_range(1_048_576, 2_000_000, 1_048_576).is_ok());
        assert!(validate_remote_file_range(1_048_576, 12, 12).is_ok());
    }

    #[test]
    fn single_label_hosts_get_an_mdns_fallback() {
        assert_eq!(
            mdns_fallback_hostname("BIHA-5CG6094NC9").as_deref(),
            Some("BIHA-5CG6094NC9.local")
        );
        assert_eq!(
            mdns_fallback_hostname(" workstation ").as_deref(),
            Some("workstation.local")
        );
    }

    #[test]
    fn addresses_and_qualified_hosts_do_not_get_an_mdns_fallback() {
        assert_eq!(mdns_fallback_hostname("server.example.test"), None);
        assert_eq!(mdns_fallback_hostname("192.168.1.124"), None);
        assert_eq!(mdns_fallback_hostname("2001:db8::1"), None);
        assert_eq!(mdns_fallback_hostname(""), None);
    }

    #[test]
    fn control_flags_do_not_consume_coalesced_mouse_markers() {
        let pending = PendingCommands::default();
        {
            let mut mouse = pending.mouse_move.lock().unwrap();
            mouse.value = Some((40, 50));
            mouse.queued = true;
        }
        pending.release_all_keys.store(true, Ordering::Release);

        let command = resolve_pending_command(
            SessionCommand::Input(vec![InputEvent::MouseMove { x: 1, y: 2 }]),
            &pending,
        );
        assert!(matches!(
            command,
            Some(SessionCommand::Input(events))
                if matches!(events.as_slice(), [InputEvent::MouseMove { x: 40, y: 50 }])
        ));
        assert!(pending.release_all_keys.load(Ordering::Acquire));
    }

    #[test]
    fn stale_control_markers_are_ignored() {
        let pending = PendingCommands::default();
        assert!(resolve_pending_command(SessionCommand::ReleaseAllKeys, &pending).is_none());
        assert!(resolve_pending_command(SessionCommand::Shutdown, &pending).is_none());
    }

    #[test]
    fn held_keys_are_counted_and_balanced() {
        // Two presses without releases keep the count up (keep-alive suppressed).
        let cmd = SessionCommand::Input(vec![key(true), key(true)]);
        assert_eq!(update_keys_down(&cmd, 0), 2);
        // The matching releases bring it back to zero.
        let cmd = SessionCommand::Input(vec![key(false), key(false)]);
        assert_eq!(update_keys_down(&cmd, 2), 0);
    }

    #[test]
    fn key_counter_never_underflows_and_full_release_zeroes_it() {
        // A stray release with nothing held saturates at zero, not u32::MAX.
        assert_eq!(
            update_keys_down(&SessionCommand::Input(vec![key(false)]), 0),
            0
        );
        // ReleaseAllKeys clears any stuck held-key state.
        assert_eq!(update_keys_down(&SessionCommand::ReleaseAllKeys, 3), 0);
    }

    #[test]
    fn non_key_input_leaves_the_held_counter_untouched() {
        let mouse = SessionCommand::Input(vec![InputEvent::MouseMove { x: 5, y: 5 }]);
        assert_eq!(update_keys_down(&mouse, 1), 1);
        assert_eq!(update_keys_down(&SessionCommand::Shutdown, 1), 1);
    }

    #[test]
    fn transport_compression_is_independent_of_graphics_mode() {
        for graphics in [GraphicsMode::Classic, GraphicsMode::Egfx] {
            let enabled = build_config(&test_session_config(graphics, true), false);
            assert_eq!(enabled.compression_type, Some(CompressionType::Rdp61));

            let disabled = build_config(&test_session_config(graphics, false), false);
            assert_eq!(disabled.compression_type, None);
        }
    }

    #[test]
    fn authentication_mode_selects_exactly_one_pre_session_protocol() {
        let password = build_config(&test_session_config(GraphicsMode::Classic, true), false);
        assert!(password.enable_credssp);
        assert!(!password.enable_rdsaad);

        let mut entra = test_session_config(GraphicsMode::Classic, true);
        entra.authentication = AuthenticationMode::EntraWeb;
        let entra = build_config(&entra, false);
        assert!(!entra.enable_credssp);
        assert!(entra.enable_rdsaad);
    }

    #[test]
    fn file_descriptors_split_wire_names_and_mark_directories() {
        use super::{to_file_descriptors, LocalClipFile};
        use ironrdp::cliprdr::pdu::ClipboardFileAttributes;

        let files = vec![
            LocalClipFile {
                path: "/tmp/report.pdf".into(),
                wire_name: "report.pdf".to_string(),
                size: 1234,
                is_dir: false,
            },
            LocalClipFile {
                path: "/tmp/project".into(),
                wire_name: "project".to_string(),
                size: 0,
                is_dir: true,
            },
            LocalClipFile {
                path: "/tmp/project/src/main.rs".into(),
                wire_name: "project\\src\\main.rs".to_string(),
                size: 7,
                is_dir: false,
            },
        ];
        let descriptors = to_file_descriptors(&files);

        assert_eq!(descriptors[0].name, "report.pdf");
        assert_eq!(descriptors[0].relative_path, None);
        assert_eq!(descriptors[0].file_size, Some(1234));

        assert_eq!(descriptors[1].name, "project");
        assert_eq!(
            descriptors[1].attributes,
            Some(ClipboardFileAttributes::DIRECTORY)
        );
        assert_eq!(descriptors[1].file_size, None);

        assert_eq!(descriptors[2].name, "main.rs");
        assert_eq!(
            descriptors[2].relative_path.as_deref(),
            Some("project\\src")
        );
    }

    #[test]
    fn remote_clipboard_cache_preserves_multiple_files_and_folders() {
        let remote = RemoteClipboard {
            files: vec![
                RemoteFileEntry {
                    wire_name: "report.txt".to_string(),
                    size: Some(6),
                    is_dir: false,
                },
                RemoteFileEntry {
                    wire_name: "project".to_string(),
                    size: None,
                    is_dir: true,
                },
                RemoteFileEntry {
                    wire_name: "project\\notes.txt".to_string(),
                    size: Some(5),
                    is_dir: false,
                },
            ],
            ..RemoteClipboard::default()
        };
        let cache_dir =
            std::env::temp_dir().join(format!("rdp123-remote-cache-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cache_dir).unwrap();

        let job = plan_remote_clipboard_cache(
            &remote,
            &["report.txt".to_string(), "project".to_string()],
            cache_dir.clone(),
        )
        .unwrap();

        assert_eq!(
            job.top_level_paths,
            [cache_dir.join("report.txt"), cache_dir.join("project")]
        );
        let destinations: Vec<_> = job
            .queue
            .iter()
            .map(|entry| entry.dest.strip_prefix(&cache_dir).unwrap().to_path_buf())
            .collect();
        assert_eq!(
            destinations,
            [
                std::path::PathBuf::from("report.txt"),
                std::path::PathBuf::from("project"),
                std::path::PathBuf::from("project/notes.txt"),
            ]
        );

        drop(job);
        assert!(!cache_dir.exists(), "unfinished caches must be removed");
    }

    #[test]
    fn invalid_remote_cache_offer_removes_its_empty_cache() {
        let remote = RemoteClipboard::default();
        let cache_dir = std::env::temp_dir().join(format!(
            "rdp123-invalid-cache-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&cache_dir).unwrap();

        let result =
            plan_remote_clipboard_cache(&remote, &["missing.txt".to_string()], cache_dir.clone());

        assert!(result.is_err());
        assert!(!cache_dir.exists(), "rejected cache plans must be removed");
    }

    #[test]
    fn remote_clipboard_top_level_names_cannot_escape_the_cache() {
        let root = std::path::Path::new("/tmp/RDP123/Clipboard/cache-id");

        assert_eq!(
            remote_top_level_destination(root, "report.txt").unwrap(),
            root.join("report.txt")
        );
        for unsafe_name in ["", ".", "..", "../outside", "/tmp/outside", "folder/file"] {
            assert!(
                remote_top_level_destination(root, unsafe_name).is_err(),
                "{unsafe_name:?} must be rejected"
            );
        }
    }

    #[test]
    fn collected_selection_walks_folders_with_relative_wire_names() {
        use super::collect_clipboard_files;

        let root = std::env::temp_dir().join(format!("rdp123-clip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("folder/inner")).unwrap();
        std::fs::write(root.join("folder/a.txt"), b"aaa").unwrap();
        std::fs::write(root.join("folder/inner/b.txt"), b"bb").unwrap();

        let files = collect_clipboard_files(&[root.join("folder")]);
        let mut names: Vec<&str> = files.iter().map(|f| f.wire_name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "folder",
                "folder\\a.txt",
                "folder\\inner",
                "folder\\inner\\b.txt"
            ]
        );
        let a = files
            .iter()
            .find(|f| f.wire_name == "folder\\a.txt")
            .unwrap();
        assert_eq!(a.size, 3);
        assert!(!a.is_dir);

        let _ = std::fs::remove_dir_all(&root);
    }
}
