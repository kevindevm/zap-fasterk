//! User-initiated native capture and local H.264 in an isolated, private camera helper.
//! The helper is this executable, with no archive access. Its process can be killed even when
//! a native camera driver blocks, so locking or ending a call releases capture immediately.

use anyhow::{Result, anyhow};
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, CameraInfo, FrameFormat, RequestedFormat,
    RequestedFormatType, Resolution,
};
use nokhwa::{Camera, pixel_format::RgbFormat};
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, Profile};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use whatsapp_rust::voip::{CallHandle, KeyframeUrgency, TimedVideoFrame, VideoFrame, VideoSource};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const FPS: u32 = 15;
const MAX_AU: usize = 2 * 1024 * 1024;
const MAX_DEVICE_LIST: usize = 64 * 1024;
const MAX_SELECTION: usize = 4096;

/// The platform identity stays local to the account's settings and the helper pipe.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CameraDevice {
    pub id: String,
    pub label: String,
}

fn camera_id(info: &CameraInfo) -> String {
    // Windows symbolic links and macOS uniqueIDs survive enumeration order changes.
    let native_id = info.misc();
    if !native_id.is_empty() {
        format!("native:{native_id}")
    } else {
        // V4L exposes a node index rather than a stable hardware ID. Include its name so a
        // differently named replacement at the same node is never selected silently.
        serde_json::to_string(&(
            info.index().as_string(),
            info.human_name(),
            info.description(),
        ))
        .expect("camera identity consists only of strings")
    }
}

fn camera_devices(infos: &[CameraInfo]) -> Vec<CameraDevice> {
    infos
        .iter()
        .map(|info| CameraDevice {
            id: camera_id(info),
            label: info.human_name(),
        })
        .collect()
}

fn selected_index(infos: &[CameraInfo], selected: Option<&str>) -> Result<CameraIndex> {
    match selected {
        None => Ok(CameraIndex::Index(0)),
        Some(id) => infos
            .iter()
            .find(|info| camera_id(info) == id)
            .map(|info| {
                let native_id = info.misc();
                if native_id.is_empty() {
                    info.index().clone()
                } else {
                    CameraIndex::String(native_id)
                }
            })
            .ok_or_else(|| anyhow!("The selected camera is unavailable. Choose another camera.")),
    }
}

fn helper_command(argument: &str) -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg(argument)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    Ok(command)
}

struct HelperChild(Option<Child>);
impl HelperChild {
    fn process(&mut self) -> &mut Child {
        self.0
            .as_mut()
            .expect("helper child exists until handed to session")
    }
    fn into_process(mut self) -> Child {
        self.0
            .take()
            .expect("helper child exists until handed to session")
    }
}
impl Drop for HelperChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn write_bounded(output: &mut impl Write, bytes: &[u8], limit: usize) -> std::io::Result<()> {
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "camera metadata too large",
        ));
    }
    output.write_all(&(bytes.len() as u32).to_le_bytes())?;
    output.write_all(bytes)?;
    output.flush()
}

fn read_bounded(input: &mut impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut size = [0; 4];
    input.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    if size > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "camera metadata too large",
        ));
    }
    let mut bytes = vec![0; size];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Enumeration never opens a camera stream. A stuck native query is killed after ten seconds;
/// cancellation also kills the child instead of leaving a blocking driver on the worker.
pub async fn devices() -> Result<Vec<CameraDevice>> {
    let mut child = HelperChild(Some(helper_command("--camera-list-helper")?.spawn()?));
    let mut output = child
        .process()
        .stdout
        .take()
        .ok_or_else(|| anyhow!("camera list pipe unavailable"))?;
    let (sender, result) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("camera-list".into())
        .spawn(move || {
            let listed = read_bounded(&mut output, MAX_DEVICE_LIST)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(serde_json::from_slice::<Vec<CameraDevice>>(&bytes)?));
            let _ = sender.send_blocking(listed);
        })?;
    tokio::time::timeout(Duration::from_secs(10), result.recv())
        .await
        .map_err(|_| anyhow!("Camera discovery timed out. Try refreshing devices."))?
        .map_err(|_| anyhow!("Camera discovery failed. Try refreshing devices."))?
}

/// Intercept before regular application startup. No settings or account storage is opened.
pub fn run_list_helper() {
    if let Ok(infos) = nokhwa::query(ApiBackend::Auto)
        && let Ok(bytes) = serde_json::to_vec(&camera_devices(&infos))
    {
        let _ = write_bounded(&mut std::io::stdout().lock(), &bytes, MAX_DEVICE_LIST);
    }
}

#[derive(Default)]
struct Frames {
    captured: AtomicU64,
    queue_dropped: AtomicU64,
    encoded_bytes: AtomicU64,
    encode_micros: AtomicU64,
    capture_clock: AtomicU64,
    peer_received: AtomicU64,
    peer_decoded: AtomicU64,
    peer_decode_failed: AtomicU64,
    peer_waiting: AtomicU64,
    peer_decode_micros: AtomicU64,
    peer_images_taken: AtomicU64,
    peer_keyframe_needed: AtomicBool,
    local: Mutex<Option<egui::ColorImage>>,
    peer: Mutex<Option<egui::ColorImage>>,
    error: Mutex<Option<&'static str>>,
    context: Mutex<Option<egui::Context>>,
}

/// Shared latest pictures only. A view never contains a protocol identity or camera handle.
#[derive(Clone, Default)]
pub struct VideoView(Arc<Frames>);
impl std::fmt::Debug for VideoView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VideoView")
    }
}
impl PartialEq for VideoView {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl VideoView {
    #[cfg(any(test, feature = "demo"))]
    pub fn synthetic(local: egui::ColorImage, peer: egui::ColorImage) -> Self {
        let view = Self::default();
        view.publish(true, local);
        view.publish(false, peer);
        view
    }
    pub fn attach(&self, ctx: &egui::Context) {
        *self.0.context.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx.clone());
    }
    pub fn take_local(&self) -> Option<egui::ColorImage> {
        self.0
            .local
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
    pub fn take_peer(&self) -> Option<egui::ColorImage> {
        let image = self.0.peer.lock().unwrap_or_else(|e| e.into_inner()).take();
        if image.is_some() {
            self.0.peer_images_taken.fetch_add(1, Ordering::Relaxed);
        }
        image
    }
    pub fn log_stats(&self, generation: u64) {
        let frames = &self.0;
        log::info!(
            "[CALL] video generation={} captured={} queue_dropped={} encoded_bytes={} encode_us={} capture_clock={} peer_received={} peer_decoded={} peer_decode_failed={} peer_waiting={} peer_decode_us={} peer_images_taken={}",
            generation,
            frames.captured.load(Ordering::Relaxed),
            frames.queue_dropped.load(Ordering::Relaxed),
            frames.encoded_bytes.load(Ordering::Relaxed),
            frames.encode_micros.load(Ordering::Relaxed),
            frames.capture_clock.load(Ordering::Relaxed),
            frames.peer_received.load(Ordering::Relaxed),
            frames.peer_decoded.load(Ordering::Relaxed),
            frames.peer_decode_failed.load(Ordering::Relaxed),
            frames.peer_waiting.load(Ordering::Relaxed),
            frames.peer_decode_micros.load(Ordering::Relaxed),
            frames.peer_images_taken.load(Ordering::Relaxed)
        );
    }
    pub fn error(&self) -> Option<&'static str> {
        *self.0.error.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn publish(&self, local: bool, image: egui::ColorImage) {
        let slot = if local { &self.0.local } else { &self.0.peer };
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(image);
        self.wake();
    }
    fn fail(&self, message: &'static str) {
        *self.0.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
        self.wake();
    }
    fn wake(&self) {
        if let Some(ctx) = self
            .0
            .context
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            ctx.request_repaint();
        }
    }
}

/// Timestamped source. The library consumes capture time rather than inventing a
/// frame for each dequeued item, so loss does not compress the peer's media clock.
#[derive(Clone)]
pub struct CameraSource {
    timed: async_channel::Receiver<TimedVideoFrame>,
    unused: async_channel::Receiver<Vec<u8>>,
}
impl VideoSource for CameraSource {
    fn frames(&self) -> async_channel::Receiver<Vec<u8>> {
        self.unused.clone()
    }
    fn timed_frames(&self) -> Option<async_channel::Receiver<TimedVideoFrame>> {
        Some(self.timed.clone())
    }
}

/// Owns both camera and decoder lifetimes. Dropping closes every outgoing media queue immediately.
pub struct VideoSession {
    pub handle: Option<Arc<CallHandle>>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    control: async_channel::Sender<()>,
    source: CameraSource,
    sink: async_channel::Sender<VideoFrame>,
    pub view: VideoView,
}
impl VideoSession {
    /// Called only after an explicit video action. Device/permission errors are returned without
    /// upgrading the call. Slow native camera operations run outside the WhatsApp worker.
    pub async fn open(selected: Option<String>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let view = VideoView::default();
        let (encoded, timed) = async_channel::bounded(2);
        let (_, unused) = async_channel::bounded(1);
        let source = CameraSource { timed, unused };
        let (sink, peer) = async_channel::bounded::<VideoFrame>(2);
        let (ready, opened) = async_channel::bounded(1);
        let mut command = helper_command("--camera-helper")?;
        let mut process = HelperChild(Some(command.spawn()?));
        let mut output = process
            .process()
            .stdout
            .take()
            .ok_or_else(|| anyhow!("camera helper has no output pipe"))?;
        let mut input = process
            .process()
            .stdin
            .take()
            .ok_or_else(|| anyhow!("camera helper has no control pipe"))?;
        let selection = serde_json::to_vec(&selected)?;
        if selection.len() > MAX_SELECTION {
            return Err(anyhow!(
                "The saved camera selection is invalid. Choose another camera."
            ));
        }
        let (control, requests) = async_channel::bounded::<()>(1);
        let child = Arc::new(Mutex::new(Some(process.into_process())));
        let session = Self {
            handle: None,
            stop: stop.clone(),
            child: child.clone(),
            control: control.clone(),
            source,
            sink,
            view: view.clone(),
        };
        std::thread::Builder::new()
            .name("camera-keyframe-control".into())
            .spawn(move || {
                // Pipe writes can block before a newly spawned helper starts reading. Keep this
                // off the runtime, with the session already owning the kill-on-cancel guard.
                if write_bounded(&mut input, &selection, MAX_SELECTION).is_err() {
                    return;
                }
                while requests.recv_blocking().is_ok() {
                    if input.write_all(&[1]).is_err() {
                        break;
                    }
                }
            })?;
        std::thread::Builder::new()
            .name("call-camera".into())
            .spawn(move || {
                let Ok(mut decoder) = openh264::decoder::Decoder::new() else {
                    return;
                };
                let mut needs_keyframe = false;
                while !stop.load(Ordering::Acquire) {
                    let Ok((kind, payload)) = read_packet(&mut output) else {
                        break;
                    };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    match kind {
                        0 => {
                            let _ = ready.try_send(Ok(()));
                        }
                        1 => {
                            let Some((timestamp, encode_us, payload)) = timed_payload(&payload) else { break; };
                            view.0.captured.fetch_add(1, Ordering::Relaxed);
                            view.0.encoded_bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
                            view.0.encode_micros.fetch_add(u64::from(encode_us), Ordering::Relaxed);
                            view.0.capture_clock.store(u64::from(timestamp), Ordering::Relaxed);
                            if let Ok(Some(frame)) = decoder.decode(payload)
                                && let Some(image) = crate::video::picture(&frame, 0, 640)
                            {
                                view.publish(true, image);
                            }
                            if needs_keyframe && !is_idr(payload) { continue; }
                            let frame = TimedVideoFrame::builder().data(payload.to_vec()).timestamp(timestamp).build();
                            if encoded.try_send(frame).is_err() {
                                view.0.queue_dropped.fetch_add(1, Ordering::Relaxed);
                                if !needs_keyframe || is_idr(payload) { request_keyframe(&control); }
                                needs_keyframe = true;
                            } else {
                                needs_keyframe = false;
                            }
                        }
                        3..=10 => {
                            log::warn!("camera helper failure: stage={kind}");
                            let message = camera_failure_message(kind);
                            let _ = ready.try_send(Err(
                                message,
                            ));
                            view.fail(message);
                            break;
                        }
                        _ => break,
                    }
                }
                if !stop.load(Ordering::Acquire) {
                    let _ = ready.try_send(Err(
                        "Camera access failed. Check permission and device availability.",
                    ));
                    if view.error().is_none() {
                        view.fail("The camera helper stopped unexpectedly. Turn the camera off and try again.");
                    }
                }
                if let Some(mut process) = child.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = process.kill();
                    let _ = process.wait();
                }
            })?;
        spawn_peer_decoder(peer, session.stop.clone(), session.view.clone())?;
        match tokio::time::timeout(Duration::from_secs(20), opened.recv()).await {
            Ok(Ok(Ok(()))) => Ok(session),
            Ok(Ok(Err(message))) => Err(anyhow!(message)),
            _ => Err(anyhow!(
                "The camera did not open in time. Check its permissions and availability."
            )),
        }
    }
    pub fn source(&self) -> CameraSource {
        self.source.clone()
    }
    pub fn sink(&self) -> async_channel::Sender<VideoFrame> {
        self.sink.clone()
    }
    pub fn request_keyframe(&self) {
        request_keyframe(&self.control);
    }
    pub fn recover_peer(&self, handle: &CallHandle) {
        if self
            .view
            .0
            .peer_keyframe_needed
            .swap(false, Ordering::AcqRel)
        {
            handle.request_peer_keyframe(KeyframeUrgency::Immediate);
        }
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.source.timed.close();
        self.sink.close();
        if let Some(mut process) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = process.kill();
            let _ = process.wait();
        }
        self.control.close();
        self.view
            .0
            .local
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.view
            .0
            .peer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}
fn spawn_peer_decoder(
    peer: async_channel::Receiver<VideoFrame>,
    decode_stop: Arc<AtomicBool>,
    decode_view: VideoView,
) -> Result<()> {
    std::thread::Builder::new()
        .name("call-video-decoder".into())
        .spawn(move || {
            // Flushing each access unit discards reference state for reordered
            // frames. Keep it between packets, as the file video player does.
            let Ok(mut decoder) = openh264::decoder::Decoder::with_api_config(
                openh264::OpenH264API::from_source(),
                openh264::decoder::DecoderConfig::new()
                    .flush_after_decode(openh264::decoder::Flush::NoFlush),
            ) else {
                decode_view.fail("Video decoding is unavailable.");
                return;
            };
            while let Ok(frame) = peer.recv_blocking() {
                if decode_stop.load(Ordering::Acquire) {
                    return;
                }
                if frame.data.len() > MAX_AU {
                    decode_view
                        .0
                        .peer_keyframe_needed
                        .store(true, Ordering::Release);
                    continue;
                }
                decode_view.0.peer_received.fetch_add(1, Ordering::Relaxed);
                let start = Instant::now();
                let decoded = decoder.decode(&frame.data);
                decode_view
                    .0
                    .peer_decode_micros
                    .fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
                match decoded {
                    Ok(Some(yuv)) => {
                        decode_view.0.peer_decoded.fetch_add(1, Ordering::Relaxed);
                        if let Some(image) = crate::video::picture(&yuv, frame.orientation, 720)
                            && !decode_stop.load(Ordering::Acquire)
                        {
                            decode_view.publish(false, image);
                        }
                    }
                    Ok(None) => {
                        decode_view.0.peer_waiting.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        decode_view
                            .0
                            .peer_decode_failed
                            .fetch_add(1, Ordering::Relaxed);
                        decode_view
                            .0
                            .peer_keyframe_needed
                            .store(true, Ordering::Release);
                        decode_view.fail("Some incoming video frames could not be decoded.");
                    }
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_peer_sink() -> (async_channel::Sender<VideoFrame>, VideoView) {
    let (sink, peer) = async_channel::bounded(2);
    let view = VideoView::default();
    spawn_peer_decoder(peer, Arc::new(AtomicBool::new(false)), view.clone()).unwrap();
    (sink, view)
}

impl std::fmt::Debug for VideoSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VideoSession")
    }
}
impl Drop for VideoSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn request_keyframe(control: &async_channel::Sender<()>) {
    let _ = control.try_send(());
}

/// Internal entry point, intercepted before settings, logging, sessions, or the archive open.
/// The helper communicates only through inherited private pipes and never logs media.
pub fn run_helper() {
    let mut input = std::io::stdin();
    let selected = read_bounded(&mut input, MAX_SELECTION)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Option<String>>(&bytes).ok());
    let Some(selected) = selected else {
        return;
    };
    let stop = Arc::new(AtomicBool::new(false));
    let keyframe = Arc::new(AtomicBool::new(true));
    let command_stop = stop.clone();
    let command_keyframe = keyframe.clone();
    let _ = std::thread::Builder::new()
        .name("camera-control".into())
        .spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut command = [0_u8; 1];
            while input.read_exact(&mut command).is_ok() {
                if command[0] == 1 {
                    command_keyframe.store(true, Ordering::Release);
                }
            }
            command_stop.store(true, Ordering::Release);
        });
    let mut output = std::io::stdout().lock();
    if let Err(error) = capture(selected.as_deref(), &stop, &keyframe, &mut output) {
        let kind = error
            .downcast_ref::<CaptureFailure>()
            .map_or(2, |error| error.0);
        let _ = write_packet(&mut output, kind, &[]);
    }
}

#[derive(Debug)]
struct CaptureFailure(u8);
impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(camera_failure_message(self.0))
    }
}
impl std::error::Error for CaptureFailure {}

fn camera_failure_message(kind: u8) -> &'static str {
    match kind {
        3 => "The selected camera is unavailable. Choose another camera.",
        4 => "The camera could not be opened. Check its availability and access settings.",
        5 => "The camera stopped supplying frames. Turn it off and try another camera.",
        6 => "The camera frame could not be decoded. Try another camera.",
        7 => "Camera video encoding failed. Turn the camera off and try again.",
        8 => "The camera supplied unsupported dimensions. Try another camera.",
        9 => "Camera permission was not granted. Allow camera access and try again.",
        10 => {
            "Windows could not start the camera stream (0xC00D3704). Close apps using the camera and try again."
        }
        _ => "The camera helper stopped unexpectedly. Turn the camera off and try again.",
    }
}

fn capture_read_failure(error: impl std::fmt::Display) -> CaptureFailure {
    if error
        .to_string()
        .to_ascii_lowercase()
        .contains("0xc00d3704")
    {
        CaptureFailure(10)
    } else {
        CaptureFailure(5)
    }
}

fn write_packet(output: &mut impl Write, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_AU {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "camera frame too large",
        ));
    }
    output.write_all(&[kind])?;
    output.write_all(&(payload.len() as u32).to_le_bytes())?;
    output.write_all(payload)?;
    output.flush()
}

fn read_packet(input: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0_u8; 5];
    input.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header[1..].try_into().expect("fixed header")) as usize;
    if size > MAX_AU || header[0] > 10 || (header[0] != 1 && size != 0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid camera helper frame",
        ));
    }
    let mut payload = vec![0; size];
    input.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

fn capture(
    selected: Option<&str>,
    stop: &AtomicBool,
    keyframe: &AtomicBool,
    output: &mut impl Write,
) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
        CameraFormat::new(Resolution::new(WIDTH, HEIGHT), FrameFormat::MJPEG, FPS),
    ));
    let index = if selected.is_some() {
        match selected_index(&nokhwa::query(ApiBackend::Auto)?, selected) {
            Ok(index) => index,
            Err(error) => {
                write_packet(output, 3, &[])?;
                return Err(error);
            }
        }
    } else {
        CameraIndex::Index(0)
    };
    let mut camera = Camera::new(index, requested).map_err(|_| CaptureFailure(4))?;
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    camera.open_stream().map_err(|_| CaptureFailure(4))?;
    if stop.load(Ordering::Acquire) {
        let _ = camera.stop_stream();
        return Ok(());
    }
    let mut encoder = encoder().map_err(|_| CaptureFailure(7))?;
    let origin = Instant::now();
    let mut next = origin;
    let mut number = 0_u64;
    while !stop.load(Ordering::Acquire) {
        let frame = camera.frame().map_err(capture_read_failure)?;
        if stop.load(Ordering::Acquire) {
            break;
        }
        let captured = Instant::now();
        // Read every native sample, even if the closest camera mode is faster
        // than 15 fps. Sleeping here lets native driver queues accumulate lag.
        if !capture_due(captured, &mut next) {
            continue;
        }
        let timestamp = capture_timestamp(captured.duration_since(origin));
        let resolution = frame.resolution();
        if resolution.width() == 0
            || resolution.height() == 0
            || resolution.width() > 4096
            || resolution.height() > 4096
        {
            return Err(CaptureFailure(8).into());
        }
        let encode_started = Instant::now();
        let rgb = decode_camera_frame(&frame).map_err(|_| CaptureFailure(6))?;
        if rgb.width() == 0 || rgb.height() == 0 || rgb.width() > 4096 || rgb.height() > 4096 {
            return Err(CaptureFailure(8).into());
        }
        let (width, height) = encoding_dimensions(rgb.width(), rgb.height())?;
        let rgb =
            image::imageops::resize(&rgb, width, height, image::imageops::FilterType::Triangle);
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(
            rgb.as_raw(),
            (width as usize, height as usize),
        ));
        if keyframe.swap(false, Ordering::AcqRel) || number.is_multiple_of(u64::from(FPS) * 2) {
            encoder.force_intra_frame();
        }
        let access = encoder
            .encode(&yuv)
            .map_err(|_| CaptureFailure(7))?
            .to_vec();
        // Queues are bounded. A dropped delta frame means the next frame must be an IDR.
        if !stop.load(Ordering::Acquire) {
            if number == 0 {
                // Readiness means an actual frame can be encoded, not just an open stream.
                write_packet(output, 0, &[])?;
            }
            let mut payload = Vec::with_capacity(access.len() + 8);
            payload.extend_from_slice(&timestamp.to_le_bytes());
            payload.extend_from_slice(
                &(encode_started
                    .elapsed()
                    .as_micros()
                    .min(u128::from(u32::MAX)) as u32)
                    .to_le_bytes(),
            );
            payload.extend_from_slice(&access);
            write_packet(output, 1, &payload)?;
        }
        number += 1;
    }
    let _ = camera.stop_stream();
    Ok(())
}

// Nokhwa's optional MJPEG decoder is disabled. Use the app's existing JPEG decoder
// instead, while keeping Nokhwa's native conversions for uncompressed camera formats.
fn decode_camera_frame(frame: &nokhwa::Buffer) -> Result<image::RgbImage> {
    if frame.source_frame_format() == FrameFormat::MJPEG {
        let mut reader = image::ImageReader::with_format(
            std::io::Cursor::new(frame.buffer()),
            image::ImageFormat::Jpeg,
        );
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        reader.limits(limits);
        Ok(reader.decode()?.to_rgb8())
    } else {
        Ok(frame.decode_image::<RgbFormat>()?)
    }
}

// H.264 needs even dimensions. Bound the longest side and retain portrait/landscape shape,
// allowing only the subpixel rounding needed by the chroma plane, never a fixed 4:3 stretch.
fn encoding_dimensions(width: u32, height: u32) -> Result<(u32, u32)> {
    if width < 2 || height < 2 || width > 4096 || height > 4096 {
        return Err(anyhow!("unsupported camera dimensions"));
    }
    let scale = (640.0 / f64::from(width.max(height))).min(1.0);
    let even = |value: u32| ((f64::from(value) * scale).round() as u32 / 2 * 2).max(2);
    Ok((even(width), even(height)))
}

fn encoder() -> Result<Encoder> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(600_000))
        .max_frame_rate(FrameRate::from_hz(FPS as f32))
        .profile(Profile::Baseline);
    Ok(Encoder::with_api_config(
        openh264::OpenH264API::from_source(),
        config,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mjpeg_camera_frames_decode_with_the_existing_jpeg_decoder() {
        let picture = image::RgbImage::from_pixel(16, 12, image::Rgb([120, 80, 40]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&picture)
            .unwrap();
        let frame = nokhwa::Buffer::new(Resolution::new(16, 12), &jpeg, FrameFormat::MJPEG);
        let decoded = decode_camera_frame(&frame).unwrap();
        assert_eq!(decoded.dimensions(), (16, 12));
        assert!(decoded.pixels().all(|pixel| pixel.0[0].abs_diff(120) < 10));
        assert!(
            decode_camera_frame(&nokhwa::Buffer::new(
                Resolution::new(16, 12),
                b"invalid",
                FrameFormat::MJPEG
            ))
            .is_err()
        );
    }

    #[test]
    fn camera_failure_pipe_retains_the_specific_safe_stage() {
        for kind in 3..=10 {
            let mut packet = Vec::new();
            write_packet(&mut packet, kind, &[]).unwrap();
            assert_eq!(
                read_packet(&mut packet.as_slice()).unwrap(),
                (kind, Vec::new())
            );
            assert!(!camera_failure_message(kind).contains("helper stopped"));
        }
    }

    #[test]
    fn windows_camera_resource_failure_is_not_reported_as_permission_denial() {
        let failure = capture_read_failure("ReadFrameError (0xC00D3704)");
        assert_eq!(failure.0, 10);
        assert!(camera_failure_message(failure.0).contains("0xC00D3704"));
        assert_eq!(capture_read_failure("Other read failure").0, 5);
    }
    #[test]
    fn camera_selection_survives_reordering_and_refuses_a_missing_device() {
        let a = CameraInfo::new("Front", "", "device-a", CameraIndex::Index(0));
        let b = CameraInfo::new("Rear", "", "device-b", CameraIndex::Index(1));
        let chosen = camera_id(&b);
        let reordered = CameraInfo::new("Rear", "", "device-b", CameraIndex::Index(0));
        assert_eq!(
            selected_index(&[reordered, a.clone()], Some(&chosen)).unwrap(),
            CameraIndex::String("device-b".into())
        );
        assert!(selected_index(&[a], Some(&chosen)).is_err());
        assert_eq!(selected_index(&[], None).unwrap(), CameraIndex::Index(0));
    }

    #[test]
    fn replacement_at_the_same_v4l_node_does_not_match_the_saved_camera() {
        let previous = CameraInfo::new("Front", "/dev/video0", "", CameraIndex::Index(0));
        let replacement = CameraInfo::new("Other", "/dev/video0", "", CameraIndex::Index(0));
        assert!(selected_index(&[replacement], Some(&camera_id(&previous))).is_err());
    }

    #[test]
    fn metadata_pipe_round_trips_and_rejects_oversized_or_truncated_input() {
        let device = CameraDevice {
            id: "invented-device".into(),
            label: "Invented camera".into(),
        };
        let mut bytes = Vec::new();
        write_bounded(
            &mut bytes,
            &serde_json::to_vec(&vec![device.clone()]).unwrap(),
            MAX_DEVICE_LIST,
        )
        .unwrap();
        let payload = read_bounded(&mut bytes.as_slice(), MAX_DEVICE_LIST).unwrap();
        assert_eq!(
            serde_json::from_slice::<Vec<CameraDevice>>(&payload).unwrap(),
            vec![device]
        );
        bytes.pop();
        assert!(read_bounded(&mut bytes.as_slice(), MAX_DEVICE_LIST).is_err());
        assert!(
            read_bounded(
                &mut ((MAX_DEVICE_LIST + 1) as u32).to_le_bytes().as_slice(),
                MAX_DEVICE_LIST
            )
            .is_err()
        );
        assert!(write_bounded(&mut Vec::new(), &[0; 5], 4).is_err());
    }

    #[test]
    fn camera_encoding_keeps_portrait_and_landscape_shape() {
        assert_eq!(encoding_dimensions(1920, 1080).unwrap(), (640, 360));
        assert_eq!(encoding_dimensions(1080, 1920).unwrap(), (360, 640));
        assert_eq!(encoding_dimensions(640, 480).unwrap(), (640, 480));
        assert_eq!(encoding_dimensions(320, 240).unwrap(), (320, 240));
        assert!(encoding_dimensions(0, 480).is_err());
        assert!(encoding_dimensions(5000, 480).is_err());
    }

    #[test]
    fn synthetic_camera_picture_round_trips_through_h264_without_a_device() {
        use openh264::formats::YUVSource;
        for (width, height) in [(WIDTH as usize, HEIGHT as usize), (360, 640)] {
            let rgb = vec![100_u8; width * height * 3];
            let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (width, height)));
            let access = encoder().unwrap().encode(&yuv).unwrap().to_vec();
            assert!(access.len() < MAX_AU);
            let mut decoder = openh264::decoder::Decoder::new().unwrap();
            let decoded = decoder.decode(&access).unwrap().unwrap();
            assert_eq!(decoded.dimensions(), (width, height));
        }
    }

    #[test]
    fn call_quality_peer_decoder_accepts_a_stream_with_reordered_frames() {
        let file = std::fs::File::open(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/video/sample.mp4"
        ))
        .unwrap();
        let size = file.metadata().unwrap().len();
        let mut video = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).unwrap();
        let track = video
            .tracks()
            .values()
            .find(|track| track.track_type().ok() == Some(mp4::TrackType::Video))
            .unwrap();
        let id = track.track_id();
        let count = track.sample_count();
        let mut parameters = Vec::new();
        crate::animation::push_annex_b(&mut parameters, track.sequence_parameter_set().unwrap());
        crate::animation::push_annex_b(&mut parameters, track.picture_parameter_set().unwrap());
        let (sink, view) = test_peer_sink();
        sink.send_blocking(VideoFrame::new(parameters)).unwrap();
        for index in 1..=count {
            let sample = video.read_sample(id, index).unwrap().unwrap();
            let mut access = Vec::new();
            crate::animation::avcc_to_annex_b(&mut access, &sample.bytes);
            sink.send_blocking(VideoFrame::new(access)).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while view.0.peer_received.load(Ordering::Relaxed) < u64::from(count) + 1
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Wait for the last decode, not just receipt of the last access unit.
        drop(sink);
        while view.0.peer_decoded.load(Ordering::Relaxed)
            + view.0.peer_waiting.load(Ordering::Relaxed)
            + view.0.peer_decode_failed.load(Ordering::Relaxed)
            < u64::from(count) + 1
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(view.0.peer_decode_failed.load(Ordering::Relaxed), 0);
        // The live stream retains its small reorder tail, rather than flushing on each packet.
        assert!(view.0.peer_decoded.load(Ordering::Relaxed) >= u64::from(count) - 3);
        assert_eq!(view.take_peer().unwrap().size, [320, 180]);
    }
    #[test]
    fn preview_retains_only_the_latest_picture() {
        let view = VideoView::default();
        for value in [10, 20, 30] {
            view.publish(true, egui::ColorImage::from_rgb([2, 2], &[value; 12]));
        }
        assert_eq!(
            view.take_local().unwrap().pixels[0],
            egui::Color32::from_rgb(30, 30, 30)
        );
        assert!(view.take_local().is_none());
        assert!(view.take_peer().is_none());
    }

    #[test]
    fn helper_pipe_is_framed_and_rejects_oversized_or_truncated_data() {
        let mut bytes = Vec::new();
        write_packet(&mut bytes, 1, &[1, 2, 3]).unwrap();
        assert_eq!(
            read_packet(&mut bytes.as_slice()).unwrap(),
            (1, vec![1, 2, 3])
        );
        bytes.pop();
        assert!(read_packet(&mut bytes.as_slice()).is_err());
        let mut oversized = vec![1];
        oversized.extend_from_slice(&((MAX_AU + 1) as u32).to_le_bytes());
        assert!(read_packet(&mut oversized.as_slice()).is_err());
    }
}

fn capture_due(captured: Instant, next: &mut Instant) -> bool {
    // Keep the cadence anchored. Tiny native scheduling jitter must not turn
    // a nominal 15 fps source into 7.5 fps by skipping every second sample.
    let tolerance = Duration::from_millis(2);
    if captured + tolerance < *next {
        return false;
    }
    let interval = Duration::from_secs_f64(1.0 / f64::from(FPS));
    *next += interval;
    while *next <= captured + tolerance {
        *next += interval;
    }
    true
}

fn capture_timestamp(elapsed: Duration) -> u32 {
    // 90 kHz RTP clock, wrapping at the wire's u32 boundary.
    (elapsed.as_micros() * 90 / 1000) as u32
}

fn timed_payload(payload: &[u8]) -> Option<(u32, u32, &[u8])> {
    let header: [u8; 4] = payload.get(..4)?.try_into().ok()?;
    let encode_us: [u8; 4] = payload.get(4..8)?.try_into().ok()?;
    let data = payload.get(8..).filter(|data| !data.is_empty())?;
    Some((
        u32::from_le_bytes(header),
        u32::from_le_bytes(encode_us),
        data,
    ))
}

fn is_idr(data: &[u8]) -> bool {
    data.windows(4)
        .any(|bytes| bytes[..3] == [0, 0, 1] && bytes[3] & 0x1f == 5)
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    #[test]
    fn native_cadence_jitter_does_not_skip_every_second_frame() {
        let origin = Instant::now();
        let mut next = origin;
        for millis in [0, 65, 134, 199, 267, 333] {
            assert!(capture_due(
                origin + Duration::from_millis(millis),
                &mut next
            ));
        }
        let mut next = origin;
        assert!(capture_due(origin, &mut next));
        assert!(!capture_due(origin + Duration::from_millis(33), &mut next));
        assert!(capture_due(origin + Duration::from_millis(67), &mut next));
    }

    #[test]
    fn capture_clock_preserves_elapsed_time_and_wraps() {
        assert_eq!(capture_timestamp(Duration::from_millis(200)), 18_000);
        assert_eq!(
            capture_timestamp(Duration::from_secs(48_000)),
            4_320_000_000u64 as u32
        );
        assert_eq!(timed_payload(&[1, 2, 3]), None);
        assert_eq!(timed_payload(&[1, 2, 3, 4]), None);
        assert_eq!(
            timed_payload(&[0x50, 0x46, 0, 0, 100, 0, 0, 0, 0x65]),
            Some((18_000, 100, &[0x65][..]))
        );
    }
    #[test]
    fn recovery_requires_an_actual_idr_not_only_parameter_sets() {
        assert!(is_idr(&[0, 0, 0, 1, 0x65, 1]));
        assert!(is_idr(&[0, 0, 1, 0x65, 1]));
        assert!(!is_idr(&[0, 0, 1, 0x67, 0, 0, 1, 0x61]));
    }
}
