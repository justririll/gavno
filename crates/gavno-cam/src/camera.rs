//! Webcam capture through GStreamer. Frames are pulled synchronously and
//! only ever live in memory.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::time::Duration;

pub struct Camera {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    pub description: String,
}

pub struct Frame<'a> {
    pub width: usize,
    pub height: usize,
    pub rgb: &'a [u8],
}

const SINK: &str = "appsink name=sink drop=true max-buffers=1 sync=false";
/// Fixed width keeps the camera's own aspect ratio.
const CAPS: &str = "video/x-raw,format=RGB,width=640";

/// libcamera device names ("/base/soc@0/..."), one per usable camera. The
/// default `libcamerasrc` camera can be an infrared sensor, so every camera is
/// tried by name.
fn libcamera_names() -> Vec<String> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Video/Source"), None);
    if monitor.start().is_err() {
        return Vec::new();
    }
    let names = monitor.devices().iter().map(|d| d.display_name().to_string()).filter(|n| n.starts_with('/')).collect();
    monitor.stop();
    names
}

fn candidates() -> Vec<String> {
    if let Ok(custom) = std::env::var("GAVNO_CAM_PIPELINE") {
        return vec![custom];
    }
    let mut list = Vec::new();
    for name in libcamera_names() {
        // Native size first: libcamera scales in its own software ISP.
        list.push(format!("libcamerasrc camera-name=\"{name}\" ! video/x-raw,width=640,height=360 ! videoconvert ! videorate name=rate drop-only=true max-rate=30 ! video/x-raw,format=RGB ! {SINK}"));
        list.push(format!("libcamerasrc camera-name=\"{name}\" ! videoconvert ! videoscale ! videorate name=rate drop-only=true max-rate=30 ! {CAPS} ! {SINK}"));
    }
    list.push(format!("pipewiresrc ! videoconvert ! videoscale ! videorate name=rate drop-only=true max-rate=30 ! {CAPS} ! {SINK}"));
    list
}

impl Camera {
    /// Tries the pipelines in order and returns the first that delivers a frame.
    pub fn open() -> Result<Self, String> {
        // libcamera is chatty about harmless sensor-database gaps.
        if std::env::var_os("LIBCAMERA_LOG_LEVELS").is_none() {
            std::env::set_var("LIBCAMERA_LOG_LEVELS", "*:FATAL");
        }
        gst::init().map_err(|e| format!("gstreamer: {e}"))?;
        let mut errors = Vec::new();
        for description in candidates() {
            match Self::try_open(&description) {
                Ok(camera) => return Ok(camera),
                Err(error) => errors.push(format!("{}: {error}", description.split(" ! ").next().unwrap_or(""))),
            }
        }
        Err(errors.join("; "))
    }

    fn try_open(description: &str) -> Result<Self, String> {
        let pipeline = gst::parse::launch(description)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_string())?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or("no appsink")?;
        let camera = Self { pipeline, sink, description: description.into() };
        camera.pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
        // The first frame proves the device really streams.
        let bus = camera.pipeline.bus().ok_or("no bus")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if camera.sink.try_pull_sample(gst::ClockTime::from_mseconds(200)).is_some() {
                return Ok(camera);
            }
            if let Some(message) = bus.pop_filtered(&[gst::MessageType::Error]) {
                if let gst::MessageView::Error(error) = message.view() {
                    return Err(error.error().to_string());
                }
            }
        }
        Err("no frame within 5 s".into())
    }

    /// Frames per second the camera branch hands to the sink; lowering it while
    /// no hand is in view saves colour conversion and analysis.
    pub fn set_max_rate(&self, fps: i32) {
        if let Some(rate) = self.pipeline.by_name("rate") {
            rate.set_property("max-rate", fps);
        }
    }

    /// Waits up to `timeout` for the next frame; `copy` receives the RGB bytes
    /// while the buffer is mapped.
    pub fn pull<R>(&self, timeout: Duration, copy: impl FnOnce(Frame<'_>) -> R) -> Option<R> {
        let sample = self.sink.try_pull_sample(gst::ClockTime::from_mseconds(timeout.as_millis() as u64))?;
        let info = gstreamer_video_info(&sample)?;
        let buffer = sample.buffer()?;
        let map = buffer.map_readable().ok()?;
        let (width, height) = info;
        // RGB rows are padded to 4 bytes; repack only when they are.
        if map.len() == width * height * 3 {
            return Some(copy(Frame { width, height, rgb: map.as_slice() }));
        }
        let stride = map.len() / height;
        let mut packed = Vec::with_capacity(width * height * 3);
        for row in 0..height {
            packed.extend_from_slice(&map[row * stride..row * stride + width * 3]);
        }
        Some(copy(Frame { width, height, rgb: &packed }))
    }
}

fn gstreamer_video_info(sample: &gst::Sample) -> Option<(usize, usize)> {
    let structure = sample.caps()?.structure(0)?;
    Some((structure.get::<i32>("width").ok()? as usize, structure.get::<i32>("height").ok()? as usize))
}

impl Drop for Camera {
    fn drop(&mut self) {
        // Releases the device so the camera LED goes out.
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
