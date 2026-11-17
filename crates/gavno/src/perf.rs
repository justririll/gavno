//! Startup wall time and bounded warnings for work on the compositor thread.
use std::{
    cell::{Cell, RefCell},
    convert::Infallible,
    rc::Rc,
    sync::{atomic::{AtomicUsize, Ordering}, Mutex, OnceLock},
    time::{Duration, Instant},
};

use smithay::{
    output::Output,
    reexports::calloop::{
        EventIterator, EventSource, LoopHandle, Poll, PostAction, Readiness, Token, TokenFactory,
    },
};

static PROCESS_START: OnceLock<Instant> = OnceLock::new();
static STALL_THRESHOLD: OnceLock<Duration> = OnceLock::new();
const DEFAULT_STALL: Duration = Duration::from_millis(8);
const WINDOW: Duration = Duration::from_secs(60);
const LIMIT: usize = 20;

/// Called as the first operation in main, before logging or allocator setup.
pub fn start_process() {
    let _ = PROCESS_START.set(Instant::now());
}

pub fn configure_stalls() {
    let threshold = std::env::var("GAVNO_STALL_MS")
        .ok()
        .as_deref()
        .and_then(parse_threshold)
        .unwrap_or(DEFAULT_STALL);
    let _ = STALL_THRESHOLD.set(threshold);
}

fn parse_threshold(value: &str) -> Option<Duration> {
    let ms: f64 = value.parse().ok()?;
    if !ms.is_finite() || ms < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(ms / 1000.0).ok()
}

pub fn elapsed_ms() -> f64 {
    PROCESS_START
        .get()
        .map_or(0.0, |start| start.elapsed().as_secs_f64() * 1000.0)
}

/// Asset loads (icons, fonts) reuse these checkpoints all session long; only
/// the startup window logs at info, later loads drop to debug.
const STARTUP_WINDOW_MS: f64 = 10_000.0;

pub fn in_startup() -> bool {
    elapsed_ms() < STARTUP_WINDOW_MS
}

pub fn startup(stage: &'static str) {
    let ms = elapsed_ms();
    if ms < STARTUP_WINDOW_MS {
        tracing::info!(target: "gavno::startup", stage, ms);
    } else {
        tracing::debug!(target: "gavno::startup", stage, ms);
    }
}

pub fn startup_output(stage: &'static str, output: &str) {
    let ms = elapsed_ms();
    if ms < STARTUP_WINDOW_MS {
        tracing::info!(target: "gavno::startup", stage, ms, output);
    } else {
        tracing::debug!(target: "gavno::startup", stage, ms, output);
    }
}

static DRM_TRACE_DEPTH: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    static DRM_CHECKPOINTS: RefCell<Vec<(&'static str, f64)>> = const { RefCell::new(Vec::new()) };
}

pub fn drm_startup_trace() -> bool {
    DRM_TRACE_DEPTH.load(Ordering::Relaxed) != 0 && in_startup()
}

/// Restrict verbose Smithay observation to device/output initialization.
/// Flush outside the subscriber callback, where tracing permits new events.
pub struct DrmStartupGuard(Option<String>);
impl DrmStartupGuard {
    pub fn new(output: Option<&str>) -> Self {
        DRM_TRACE_DEPTH.fetch_add(1, Ordering::Relaxed);
        Self(output.map(str::to_owned))
    }
}
impl Drop for DrmStartupGuard {
    fn drop(&mut self) {
        if DRM_TRACE_DEPTH.fetch_sub(1, Ordering::Relaxed) != 1 { return; }
        let checkpoints = DRM_CHECKPOINTS.with(|queue| std::mem::take(&mut *queue.borrow_mut()));
        for (stage, ms) in checkpoints {
            if let Some(output) = self.0.as_deref() {
                tracing::info!(target: "gavno::startup", stage, ms, output);
            } else {
                tracing::info!(target: "gavno::startup", stage, ms);
            }
        }
    }
}

/// Observe Smithay's internal DRM boundaries without a dependency fork. The
/// separately filtered layer is active only during startup and emits no timers.
/// Formatting/logging remains governed by the user's normal RUST_LOG filter.
pub struct DrmStartup;
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DrmStartup {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(Option<&'static str>);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() != "message" { return; }
                let message = format!("{value:?}");
                self.0 = drm_stage(&message);
            }
        }
        let mut message = Message(None);
        event.record(&mut message);
        if let Some(stage) = message.0 {
            DRM_CHECKPOINTS.with(|queue| queue.borrow_mut().push((stage, elapsed_ms())));
        }
    }
}

fn drm_stage(message: &str) -> Option<&'static str> {
    if message.starts_with("DrmDevice initializing") { Some("udev.drm.capabilities.begin") }
    else if message.starts_with("Mapping:") { Some("udev.drm.properties.ready") }
    else if message.starts_with("Testing color format:") { Some("udev.output.format_test.begin") }
    else if message.starts_with("Preparing Commit") { Some("udev.output.commit.prepare") }
    else if message.starts_with("Testing screen config") { Some("udev.output.atomic_test.begin") }
    else if message.starts_with("Setting screen:") { Some("udev.output.atomic_commit.begin") }
    else { None }
}

/// Also records the end of fallible cold paths that return early.
pub struct Startup(&'static str);
impl Startup {
    pub fn new(begin: &'static str, end: &'static str) -> Self {
        startup(begin);
        Self(end)
    }
}
impl Drop for Startup {
    fn drop(&mut self) {
        startup(self.0);
    }
}

struct RateLimit {
    recent: [Option<Instant>; LIMIT],
    next: usize,
}
impl RateLimit {
    const fn new() -> Self {
        Self {
            recent: [None; LIMIT],
            next: 0,
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        if self.recent[self.next].is_some_and(|last| now.duration_since(last) < WINDOW) {
            return false;
        }
        self.recent[self.next] = Some(now);
        self.next = (self.next + 1) % LIMIT;
        true
    }
}
static WARNINGS: Mutex<RateLimit> = Mutex::new(RateLimit::new());

const STAGES: usize = 6;

/// No allocation or lock below the threshold. Output::name() allocates, so
/// keep a borrowed output and resolve its name only when emitting a warning.
pub struct Stall<'a> {
    start: Instant,
    source: &'static str,
    output: Option<&'a Output>,
    stages: [(&'static str, Duration); STAGES],
    marked: usize,
}
impl Stall<'_> {
    pub fn new(source: &'static str) -> Self {
        Self {
            start: Instant::now(),
            source,
            output: None,
            stages: [("", Duration::ZERO); STAGES],
            marked: 0,
        }
    }

    /// Ends the current stage. A stalled scope logs how long each stage took;
    /// marks beyond the fixed capacity are ignored.
    pub fn mark(&mut self, stage: &'static str) {
        if let Some(slot) = self.stages.get_mut(self.marked) {
            *slot = (stage, self.start.elapsed());
            self.marked += 1;
        }
    }
}
impl<'a> Stall<'a> {
    pub fn output_since(source: &'static str, output: &'a Output, start: Instant) -> Self {
        Self {
            output: Some(output),
            start,
            ..Self::new(source)
        }
    }
}
impl Drop for Stall<'_> {
    fn drop(&mut self) {
        record_stall(self.source, self.output, self.start.elapsed(), &self.stages[..self.marked]);
    }
}

/// "elements=1.2 draw=20.3 rest=0.1": milliseconds spent in each marked stage.
fn stage_breakdown(stages: &[(&'static str, Duration)], total: Duration) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut previous = Duration::ZERO;
    for &(stage, at) in stages {
        let _ = write!(out, "{stage}={:.1} ", at.saturating_sub(previous).as_secs_f64() * 1000.0);
        previous = at;
    }
    let _ = write!(out, "rest={:.1}", total.saturating_sub(previous).as_secs_f64() * 1000.0);
    out
}

fn record_stall(
    source: &'static str,
    output: Option<&Output>,
    elapsed: Duration,
    stages: &[(&'static str, Duration)],
) {
    if elapsed < *STALL_THRESHOLD.get().unwrap_or(&DEFAULT_STALL) {
        return;
    }
    // Shared rolling window across all sources, including nested measurements.
    let allowed = WARNINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow(Instant::now());
    if !allowed {
        return;
    }
    let ms = elapsed.as_secs_f64() * 1000.0;
    // `Option` fields are omitted from the event when `None`.
    let stages = (!stages.is_empty()).then(|| stage_breakdown(stages, elapsed));
    let stages = stages.as_deref();
    if let Some(output) = output {
        tracing::warn!(target: "gavno::stall", source, ms, output = output.name(), stages, "event-loop stall");
    } else {
        tracing::warn!(target: "gavno::stall", source, ms, stages, "event-loop stall");
    }
}

/// An FD-free observer: starts timing after poll returns, without waking the
/// loop or changing its timeout. The run callback finishes after idle work.
struct DispatchWatch(Rc<Cell<Option<Instant>>>);

pub fn watch_dispatch<Data>(
    handle: &LoopHandle<'_, Data>,
) -> smithay::reexports::calloop::Result<Rc<Cell<Option<Instant>>>> {
    let start = Rc::new(Cell::new(None));
    handle.insert_source(DispatchWatch(start.clone()), |(), _, _| {})?;
    Ok(start)
}

pub fn finish_dispatch(start: &Cell<Option<Instant>>) {
    if let Some(start) = start.take() {
        record_stall("calloop.dispatch", None, start.elapsed(), &[]);
    }
}

impl EventSource for DispatchWatch {
    type Event = ();
    type Metadata = ();
    type Ret = ();
    type Error = Infallible;
    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn before_handle_events(&mut self, _: EventIterator<'_>) {
        self.0.set(Some(Instant::now()));
    }
    fn process_events<F>(&mut self, _: Readiness, _: Token, _: F) -> Result<PostAction, Infallible>
    where
        F: FnMut((), &mut ()),
    {
        Ok(PostAction::Continue)
    }
    fn register(
        &mut self,
        _: &mut Poll,
        _: &mut TokenFactory,
    ) -> smithay::reexports::calloop::Result<()> {
        Ok(())
    }
    fn reregister(
        &mut self,
        _: &mut Poll,
        _: &mut TokenFactory,
    ) -> smithay::reexports::calloop::Result<()> {
        Ok(())
    }
    fn unregister(&mut self, _: &mut Poll) -> smithay::reexports::calloop::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smithay_checkpoints_flush_after_the_drm_call_and_preserve_filtering() {
        use std::sync::Arc;
        use tracing_subscriber::{layer::SubscriberExt, Layer};
        struct Capture(Arc<Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> Layer<S> for Capture {
            fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
                struct Stage(Option<String>);
                impl tracing::field::Visit for Stage {
                    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
                    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                        if field.name() == "stage" { self.0 = Some(value.into()); }
                    }
                }
                let mut stage = Stage(None);
                event.record(&mut stage);
                if let Some(stage) = stage.0 { self.0.lock().unwrap().push(stage); }
            }
        }
        let stages = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(Capture(stages.clone()).with_filter(tracing_subscriber::filter::filter_fn(|metadata| metadata.target() == "gavno::startup")))
            .with(DrmStartup.with_filter(tracing_subscriber::filter::dynamic_filter_fn(|metadata, _| {
                drm_startup_trace() && metadata.target().starts_with("smithay::backend::drm")
            })));
        fn property_snapshot() {
            tracing::trace!(target: "smithay::backend::drm::device::atomic", "Mapping: test properties");
        }
        tracing::subscriber::with_default(subscriber, || {
            property_snapshot();
            {
                let _guard = DrmStartupGuard::new(Some("test-output"));
                property_snapshot();
                assert!(stages.lock().unwrap().is_empty());
            }
            property_snapshot();
        });
        assert_eq!(*stages.lock().unwrap(), ["udev.drm.properties.ready"]);
    }

    #[test]
    fn smithay_drm_checkpoints_use_only_known_messages() {
        assert_eq!(drm_stage("DrmDevice initializing"), Some("udev.drm.capabilities.begin"));
        assert_eq!(drm_stage("Mapping: {planes}"), Some("udev.drm.properties.ready"));
        assert_eq!(drm_stage("Testing color format: XR30"), Some("udev.output.format_test.begin"));
        assert_eq!(drm_stage("Setting screen: request"), Some("udev.output.atomic_commit.begin"));
        assert_eq!(drm_stage("unrelated message"), None);
    }

    #[test]
    fn threshold_accepts_fractional_ms_and_rejects_invalid_values() {
        assert_eq!(parse_threshold("2.5"), Some(Duration::from_micros(2500)));
        assert_eq!(parse_threshold("0"), Some(Duration::ZERO));
        for value in ["", "bad", "-1", "NaN", "inf", "1e100"] {
            assert_eq!(parse_threshold(value), None, "{value}");
        }
    }

    #[test]
    fn stage_breakdown_reports_each_stage_and_the_rest() {
        let ms = Duration::from_millis;
        let stages = [("elements", ms(2)), ("draw", ms(22))];
        assert_eq!(stage_breakdown(&stages, ms(23)), "elements=2.0 draw=20.0 rest=1.0");
        assert_eq!(stage_breakdown(&[], ms(9)), "rest=9.0");
    }

    #[test]
    fn stall_marks_beyond_capacity_are_ignored() {
        let mut stall = Stall::new("test");
        for _ in 0..STAGES + 3 {
            stall.mark("stage");
        }
        assert_eq!(stall.marked, STAGES);
        std::mem::forget(stall);
    }

    #[test]
    fn warnings_are_limited_in_a_rolling_minute() {
        let mut limit = RateLimit::new();
        let start = Instant::now();
        for i in 0..LIMIT {
            assert!(limit.allow(start + Duration::from_secs(i as u64)));
        }
        assert!(!limit.allow(start + Duration::from_secs(59)));
        assert!(limit.allow(start + WINDOW));
        assert!(!limit.allow(start + WINDOW));
        assert!(limit.allow(start + WINDOW + Duration::from_secs(1)));
    }

    #[test]
    fn dispatch_watch_starts_after_poll_and_does_not_wake_the_loop() {
        use smithay::reexports::calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop,
        };
        let mut event_loop = EventLoop::<()>::try_new().unwrap();
        let start = watch_dispatch(&event_loop.handle()).unwrap();
        let before_callback = Rc::new(Cell::new(None));
        let callback_start = start.clone();
        let callback_observed = before_callback.clone();
        let deadline = Instant::now() + Duration::from_millis(25);
        event_loop
            .handle()
            .insert_source(Timer::from_deadline(deadline), move |_, _, _| {
                callback_observed.set(callback_start.get());
                TimeoutAction::Drop
            })
            .unwrap();
        event_loop.dispatch(None, &mut ()).unwrap();
        let observed = start
            .get()
            .expect("observer ran even without an event of its own");
        assert!(observed >= deadline);
        drop(event_loop);
        assert_eq!(before_callback.get(), Some(observed));
        finish_dispatch(&start);
        assert!(start.get().is_none());
    }
}
