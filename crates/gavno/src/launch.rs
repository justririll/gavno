//! Launch latency, matched by trusted activation token or app identity.
//! Only pending launches/tokens own an expiry timer; the idle desktop does no work here.
use crate::Gavno;
use smithay::{
    backend::renderer::element::RenderElementStates,
    desktop::{utils::with_surfaces_surface_tree, Window},
    output::Output,
    reexports::{
        calloop::{
            channel::{self, Event},
            timer::{TimeoutAction, Timer},
            RegistrationToken,
        },
        wayland_server::{backend::ObjectId, protocol::wl_surface::WlSurface, Resource},
    },
    wayland::{
        compositor::with_states, shell::xdg::XdgToplevelSurfaceData,
        xdg_activation::XdgActivationToken,
    },
};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

const LIFETIME: Duration = Duration::from_secs(30);
const TOKEN_LIFETIME: Duration = Duration::from_secs(60);
type Spawned = Arc<OnceLock<(Instant, Option<u32>)>>;

struct Request {
    command: String,
    token: Option<String>,
    result: Spawned,
    reply: Option<channel::Sender<()>>,
}

static SPAWNER: OnceLock<Option<std::sync::mpsc::Sender<Request>>> = OnceLock::new();

/// Initialize before listening, after setting the session's display environment.
/// No fork hooks: Rust can use glibc's posix_spawn fast path. All environment
/// copying, process creation and shell reaping stay on this sleeping worker.
pub(crate) fn init_spawner() {
    SPAWNER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Request>();
        match std::thread::Builder::new()
            .name("gavno-spawner".into())
            .spawn(move || {
                for request in rx {
                    let mut cmd = std::process::Command::new("sh");
                    cmd.arg("-c")
                        .arg(crate::state::detached_script(&request.command));
                    if let Some(token) = &request.token {
                        cmd.env("XDG_ACTIVATION_TOKEN", token);
                    }
                    match cmd.spawn() {
                        Ok(mut child) => {
                            let _ = request.result.set((Instant::now(), Some(child.id())));
                            // Only the short-lived outer shell is our child. Waiting
                            // never follows the lifetime of the detached application.
                            let _ = child.wait();
                        }
                        Err(error) => {
                            let _ = request.result.set((Instant::now(), None));
                            tracing::warn!(%error, command = %request.command, "spawn failed");
                        }
                    }
                    if let Some(reply) = request.reply {
                        let _ = reply.send(());
                    }
                }
            }) {
            Ok(_) => Some(tx),
            Err(error) => {
                tracing::warn!(%error, "cannot start app spawner");
                None
            }
        }
    });
}

pub(crate) fn spawn(
    command: &str,
    token: Option<&str>,
    reply: Option<channel::Sender<()>>,
) -> Spawned {
    init_spawner();
    let result = Arc::new(OnceLock::new());
    let request = Request {
        command: command.to_owned(),
        token: token.map(str::to_owned),
        result: result.clone(),
        reply,
    };
    if !SPAWNER
        .get()
        .and_then(Option::as_ref)
        .is_some_and(|tx| tx.send(request).is_ok())
    {
        let _ = result.set((Instant::now(), None));
        tracing::warn!(command, "app spawner is unavailable");
    }
    result
}

/// Install a wakeup for the rare case where a client presents before the worker
/// publishes spawn completion. There is no polling of the shared result.
pub(crate) fn init(gv: &mut Gavno) {
    init_spawner();
    crate::icons::app_worker::init(gv);
    crate::classic::logo::spawn(gv);
    let (tx, rx) = channel::channel();
    match gv.loop_handle.insert_source(rx, |event, _, gv| {
        let _stall = crate::perf::Stall::new("launch.spawn_reply");
        if matches!(event, Event::Msg(())) {
            gv.finish_launches();
        }
    }) {
        Ok(_) => gv.launches.reply = Some(tx),
        Err(error) => tracing::warn!(%error, "cannot watch app spawn completion"),
    }
}

struct Launch {
    action: Instant,
    dispatched: Instant,
    command: String,
    app_id: Option<String>,
    executable: Option<String>,
    token: String,
    spawned: Spawned,
    surface: Option<ObjectId>,
    /// Where the launching dock icon or launcher row was, global logical px.
    source: Option<crate::anim::Rect>,
}
struct LaunchToken {
    created: Instant,
    token: XdgActivationToken,
    focus_unchanged: bool,
}
struct Observed {
    logged: bool,
    surface: ObjectId,
    commit: Instant,
    mapped: Option<Instant>,
    submitted: Option<Output>,
    presented: Option<(Instant, &'static str)>,
}
#[derive(Default)]
pub(crate) struct Launches {
    // Focus authorization survives expiry of measurement observations.
    focus_epoch: u64,
    pending: Vec<Launch>,
    observed: Vec<Observed>,
    expiry: Option<(RegistrationToken, Instant)>,
    tokens: Vec<LaunchToken>,
    reply: Option<channel::Sender<()>>,
}

fn identity(value: &str) -> &str {
    value.trim_end_matches(".desktop")
}
fn same_app(a: &str, b: &str) -> bool {
    !a.is_empty() && identity(a).eq_ignore_ascii_case(identity(b))
}
fn executable(command: &str) -> Option<String> {
    // Exec arguments have already been expanded and shell-quoted by DesktopEntry.
    // Arbitrary shell scripts simply rely on activation tokens if this is ambiguous.
    let first = if let Some(quoted) = command.strip_prefix('\'') {
        quoted.split_once('\'')?.0
    } else {
        command.split_whitespace().next()?
    };
    Some(first.rsplit('/').next()?.to_owned())
}
impl Launch {
    fn matches(&self, app_id: &str) -> bool {
        self.app_id
            .as_deref()
            .is_some_and(|id| same_app(id, app_id))
            || self.executable.as_deref().is_some_and(|exe| {
                same_app(exe, app_id)
                    || app_id
                        .rsplit('.')
                        .next()
                        .is_some_and(|id| same_app(exe, id))
            })
    }
}
impl Launches {
    /// Launches started and not yet matched to a window.
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }
    fn expire(&mut self, now: Instant) {
        self.pending
            .retain(|launch| now.duration_since(launch.action) < LIFETIME);
        self.observed
            .retain(|window| now.duration_since(window.commit) < LIFETIME);
        if self.pending.is_empty() {
            self.observed.clear();
        }
    }
    fn finish(&mut self) {
        self.expire(Instant::now());
        self.pending.retain(|launch| {
            let Some(window) = self.observed.iter_mut().find(|w| !w.logged && Some(&w.surface) == launch.surface.as_ref()) else { return true };
            let (Some((spawned, _)), Some(mapped), Some((presented, presentation))) =
                (launch.spawned.get(), window.mapped, window.presented) else { return true };
            let ms = |time: Instant| time.saturating_duration_since(launch.action).as_secs_f64() * 1000.0;
            tracing::info!(target: "gavno::launch", command = %launch.command,
                app_id = launch.app_id.as_deref().unwrap_or(""), dispatch_ms = ms(launch.dispatched), spawn_ms = ms(*spawned),
                commit_ms = ms(window.commit), mapped_ms = ms(mapped), presented_ms = ms(presented),
                commit_to_present_ms = presented.saturating_duration_since(window.commit).as_secs_f64() * 1000.0,
                presentation, "app launch");
            window.logged = true;
            false
        });
        if self.pending.is_empty() {
            self.observed.clear();
        }
    }
}
fn surface_tree_visible(surface: &WlSurface, states: &RenderElementStates) -> bool {
    let mut visible = false;
    with_surfaces_surface_tree(surface, |surface, _| {
        visible |= states
            .element_render_state(surface)
            .is_some_and(|state| state.visible_area > 0);
    });
    visible
}
impl Gavno {
    /// Only compositor-owned tokens receive the longer lifetime. Consumption is
    /// separate from measurement matching, which may already have finished.
    pub(crate) fn consume_launch_token(&mut self, token: &XdgActivationToken) -> Option<bool> {
        let index = self
            .launches
            .tokens
            .iter()
            .position(|owned| &owned.token == token)?;
        let owned = self.launches.tokens.remove(index);
        Some(owned.focus_unchanged && owned.created.elapsed() <= TOKEN_LIFETIME)
    }
    pub(crate) fn launch_focus_epoch(&self) -> u64 {
        self.launches.focus_epoch
    }
    /// Focus changes and client-directed key presses revoke earlier launches,
    /// including consumed tokens whose surface is still waiting to map.
    pub(crate) fn launch_user_activity(&mut self, input_started: Instant) {
        self.launches.focus_epoch = self.launches.focus_epoch.wrapping_add(1);
        for owned in &mut self.launches.tokens {
            // A launch initiated by this very input retains its authorization.
            if owned.created < input_started {
                owned.focus_unchanged = false;
            }
        }
    }
    pub(crate) fn launch_shell(&mut self, command: &str, app_id: Option<&str>) {
        self.launch_shell_at(Instant::now(), command, app_id);
    }
    pub(crate) fn launch_shell_at(&mut self, action: Instant, command: &str, app_id: Option<&str>) {
        self.launch_shell_from(action, command, app_id, None);
    }
    /// `source` is the rectangle the new window grows from.
    pub(crate) fn launch_shell_from(&mut self, action: Instant, command: &str, app_id: Option<&str>, source: Option<crate::anim::Rect>) {
        self.expire_launches();
        let token = self
            .protocols
            .activation
            .create_external_token(None)
            .0
            .clone();
        let token_text = token.as_str().to_owned();
        self.launches.tokens.push(LaunchToken {
            created: Instant::now(),
            token,
            focus_unchanged: true,
        });
        let spawned = spawn(command, Some(&token_text), self.launches.reply.clone());
        let dispatched = Instant::now();
        self.launches.pending.push(Launch {
            action,
            dispatched,
            command: command.to_owned(),
            app_id: app_id.map(str::to_owned),
            executable: executable(command),
            token: token_text,
            spawned,
            surface: None,
            source,
        });
        self.arm_launch_expiry();
    }
    fn expire_launches(&mut self) {
        let now = Instant::now();
        self.launches.expire(now);
        self.launches.tokens.retain(|owned| {
            if now.duration_since(owned.created) < TOKEN_LIFETIME {
                true
            } else {
                self.protocols.activation.remove_token(&owned.token);
                false
            }
        });
    }
    fn arm_launch_expiry(&mut self) {
        let deadline = self
            .launches
            .pending
            .iter()
            .map(|l| l.action + LIFETIME)
            .chain(
                self.launches
                    .tokens
                    .iter()
                    .map(|owned| owned.created + TOKEN_LIFETIME),
            )
            .min();
        if self.launches.expiry.as_ref().map(|(_, at)| *at) == deadline {
            return;
        }
        if let Some((timer, _)) = self.launches.expiry.take() {
            self.loop_handle.remove(timer);
        }
        if let Some(deadline) = deadline {
            match self
                .loop_handle
                .insert_source(Timer::from_deadline(deadline), |_, _, gv| {
                    let _stall = crate::perf::Stall::new("timer.launch_expiry");
                    gv.launches.expiry = None;
                    gv.expire_launches();
                    gv.arm_launch_expiry();
                    TimeoutAction::Drop
                }) {
                Ok(timer) => self.launches.expiry = Some((timer, deadline)),
                Err(error) => tracing::warn!(%error, "cannot schedule launch expiry"),
            }
        }
    }
    fn finish_launches(&mut self) {
        self.launches.finish();
        self.arm_launch_expiry();
    }
    /// The launch source of a window, once launch tracking matched it by
    /// activation token or app id.
    pub(crate) fn launch_source(&self, window: &Window) -> Option<crate::anim::Rect> {
        let id = window.toplevel()?.wl_surface().id();
        self.launches.pending.iter().find(|l| l.surface.as_ref() == Some(&id)).and_then(|l| l.source)
    }
    pub(crate) fn launch_commit(&mut self, window: &Window) {
        if self.launches.pending.is_empty() {
            return;
        }
        self.expire_launches();
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let surface = toplevel.wl_surface();
        if self
            .launches
            .observed
            .iter()
            .any(|w| w.surface == surface.id())
        {
            return;
        }
        let app_id = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .app_id
                .clone()
                .unwrap_or_default()
        });
        // FIFO for repeated launches of an app. Tokens can identify reused/server processes later.
        if !self
            .launches
            .pending
            .iter()
            .any(|l| l.surface.as_ref() == Some(&surface.id()))
        {
            if let Some(launch) = self
                .launches
                .pending
                .iter_mut()
                .find(|l| l.surface.is_none() && l.matches(&app_id))
            {
                launch.surface = Some(surface.id());
            }
        }
        self.launches.observed.push(Observed {
            logged: false,
            surface: surface.id(),
            commit: Instant::now(),
            mapped: None,
            submitted: None,
            presented: None,
        });
    }
    pub(crate) fn launch_mapped(&mut self, window: &Window) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        if let Some(w) = self
            .launches
            .observed
            .iter_mut()
            .find(|w| w.surface == toplevel.wl_surface().id())
        {
            w.mapped = Some(Instant::now());
        }
    }
    pub(crate) fn launch_activation(&mut self, token: &str, surface: &WlSurface) {
        self.expire_launches();
        self.launches
            .tokens
            .retain(|owned| owned.token.as_str() != token);
        // A completed FIFO observation cannot be attributed again by a late token.
        if self
            .launches
            .observed
            .iter()
            .any(|w| w.surface == surface.id() && w.logged)
        {
            return;
        }
        if let Some(index) = self.launches.pending.iter().position(|l| l.token == token) {
            // An exact token wins over a tentative app-id match, including
            // concurrent launches with the same app id.
            for launch in &mut self.launches.pending {
                if launch.surface.as_ref() == Some(&surface.id()) {
                    launch.surface = None;
                }
            }
            self.launches.pending[index].surface = Some(surface.id());
        }
        self.finish_launches();
    }
    pub(crate) fn launch_submitted(&mut self, output: &Output, states: &RenderElementStates) {
        if self.launches.pending.is_empty() {
            return;
        }
        for window in self.space.elements() {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            if self.space.outputs_for_element(window).contains(output)
                && surface_tree_visible(toplevel.wl_surface(), states)
                && (!self.features.on(crate::module::Feature::Animations)
                    || self
                        .window_animations
                        .motion(window)
                        .is_none_or(|m| m.opacity() > 0.0))
            {
                if let Some(w) = self
                    .launches
                    .observed
                    .iter_mut()
                    .find(|w| w.surface == toplevel.wl_surface().id())
                {
                    if w.submitted.is_none() {
                        w.submitted = Some(output.clone());
                    }
                }
            }
        }
    }
    pub(crate) fn launch_presented(&mut self, output: &Output, presentation: &'static str) {
        if self.launches.pending.is_empty() {
            return;
        }
        for w in &mut self.launches.observed {
            if w.presented.is_none() && w.submitted.as_ref() == Some(output) {
                w.presented = Some((Instant::now(), presentation));
            }
        }
        self.finish_launches();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(action: Instant, app_id: Option<&str>, command: &str, token: &str) -> Launch {
        let spawned = Arc::new(OnceLock::new());
        spawned
            .set((action + Duration::from_millis(1), Some(123)))
            .unwrap();
        Launch {
            action,
            dispatched: action,
            app_id: app_id.map(str::to_owned),
            command: command.into(),
            executable: executable(command),
            token: token.into(),
            spawned,
            surface: None,
            source: None,
        }
    }
    fn set_app_id(window: &Window, app_id: &str) {
        with_states(window.toplevel().unwrap().wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .app_id = Some(app_id.into());
        });
    }

    fn old_token(
        h: &mut crate::state::tests::Harness,
        owned: bool,
        age: Duration,
    ) -> (
        XdgActivationToken,
        smithay::wayland::xdg_activation::XdgActivationTokenData,
    ) {
        let data = smithay::wayland::xdg_activation::XdgActivationTokenData {
            timestamp: Instant::now() - age,
            ..Default::default()
        };
        let token =
            h.gv.protocols
                .activation
                .create_external_token(data.clone())
                .0
                .clone();
        if owned {
            h.gv.launches.tokens.push(LaunchToken {
                created: data.timestamp,
                token: token.clone(),
                focus_unchanged: true,
            });
        }
        (token, data)
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MeasurementExpiry {
        Live,
        BeforeActivation,
        AfterActivation,
    }

    fn deferred_first_map(expiry: MeasurementExpiry, revoke: bool) {
        let expire_measurement = expiry == MeasurementExpiry::BeforeActivation;
        use smithay::wayland::xdg_activation::XdgActivationHandler;
        let mut h = crate::state::tests::Harness::new();
        let previous = h.window();
        h.gv.map_new_window(previous.clone());
        let alternative = h.window();
        h.gv.map_new_window(alternative.clone());
        h.gv.focus_window(Some(&previous));
        let target = h.window();
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &target);
        target.toplevel().unwrap().send_configure();
        h.gv.unmapped.push(target.clone());
        let (token, data) = old_token(
            &mut h,
            true,
            Duration::from_secs(if expire_measurement { 35 } else { 6 }),
        );
        let action = if expire_measurement {
            Instant::now() - Duration::from_secs(35)
        } else {
            Instant::now()
        };
        h.gv.launches
            .pending
            .push(launch(action, Some("fixture"), "fixture", token.as_str()));
        h.gv.request_activation(
            token.clone(),
            data,
            target.toplevel().unwrap().wl_surface().clone(),
        );
        assert!(h.gv.protocols.activation.data_for_token(&token).is_none());
        assert!(h.gv.launches.tokens.is_empty());
        assert_eq!(h.gv.launches.pending.is_empty(), expire_measurement);
        assert!(!crate::handlers::xdg_activation::is_urgent(&target));
        if expiry == MeasurementExpiry::AfterActivation {
            for launch in &mut h.gv.launches.pending {
                launch.action = Instant::now() - LIFETIME;
            }
            h.gv.expire_launches();
            assert!(h.gv.launches.pending.is_empty());
        }
        if revoke {
            let input_started = Instant::now();
            h.gv.focus_window(Some(&alternative));
            h.gv.launch_user_activity(input_started);
        }
        let focus_before = h.gv.focused_window();
        h.commit_size(&target, 640, 400);
        assert!(h.gv.space.element_location(&target).is_some());
        assert_eq!(crate::handlers::xdg_activation::is_urgent(&target), revoke);
        assert_eq!(
            h.gv.focused_window(),
            if revoke { focus_before } else { Some(target) }
        );
    }
    #[test]
    fn consumed_activation_remains_revocable_until_first_buffer() {
        for revoke in [false, true] {
            deferred_first_map(MeasurementExpiry::Live, revoke);
        }
    }
    #[test]
    fn deferred_first_map_authorization_survives_measurement_expiry() {
        for revoke in [false, true] {
            deferred_first_map(MeasurementExpiry::BeforeActivation, revoke);
        }
    }
    #[test]
    fn consumed_authorization_survives_later_measurement_expiry() {
        for revoke in [false, true] {
            deferred_first_map(MeasurementExpiry::AfterActivation, revoke);
        }
    }
    #[test]
    fn reversed_fifo_arrival_cannot_deny_fresh_launch_without_activation() {
        let mut h = crate::state::tests::Harness::new();
        let previous = h.window();
        h.gv.map_new_window(previous);
        let (a, _) = old_token(&mut h, true, Duration::from_secs(6));
        h.gv.focus_window(None);
        h.gv.launch_user_activity(Instant::now());
        let (b, _) = old_token(&mut h, true, Duration::ZERO);
        let now = Instant::now();
        h.gv.launches.pending = vec![
            launch(now, Some("fixture"), "fixture", a.as_str()),
            launch(now, Some("fixture"), "fixture", b.as_str()),
        ];
        let b_window = h.window();
        set_app_id(&b_window, "fixture");
        h.gv.launch_commit(&b_window);
        assert_eq!(
            h.gv.launches.pending[0].surface,
            Some(b_window.toplevel().unwrap().wl_surface().id()),
            "FIFO remains a telemetry hint"
        );
        assert!(!h.gv.launches.tokens[0].focus_unchanged);
        assert!(h.gv.launches.tokens[1].focus_unchanged);
        assert!(!crate::handlers::xdg_activation::is_urgent(&b_window));
        h.gv.map_new_window(b_window.clone());
        assert_eq!(h.gv.focused_window(), Some(b_window));
    }
    #[test]
    fn polkit_blocks_activation_and_first_map_autofocus() {
        use smithay::wayland::xdg_activation::XdgActivationHandler;
        for mapped in [false, true] {
            let mut h = crate::state::tests::Harness::new();
            let target = h.window();
            if mapped {
                h.gv.map_new_window(target.clone());
            } else {
                h.gv.unmapped.push(target.clone());
            }
            let previous = h.window();
            h.gv.map_new_window(previous.clone());
            let (token, data) = old_token(&mut h, true, Duration::from_secs(6));
            let _outcome = crate::polkit::tests::push_dialog(&mut h.gv, "launch-focus", &["admin"]);
            assert!(h.gv.polkit_visible());
            h.gv.request_activation(token, data, target.toplevel().unwrap().wl_surface().clone());
            assert!(crate::handlers::xdg_activation::is_urgent(&target));
            assert_eq!(h.gv.focused_window(), Some(previous.clone()));
            assert_eq!(h.gv.space.elements().last(), Some(&previous));
            if !mapped {
                h.gv.map_new_window(target.clone());
            }
            assert_eq!(h.gv.focused_window(), Some(previous.clone()));
            let no_token = h.window();
            h.gv.map_new_window(no_token.clone());
            assert!(crate::handlers::xdg_activation::is_urgent(&no_token));
            assert_eq!(h.gv.focused_window(), Some(previous));
        }
    }

    #[test]
    fn revoked_activation_does_not_mark_the_focused_window_urgent() {
        use smithay::wayland::xdg_activation::XdgActivationHandler;
        let mut h = crate::state::tests::Harness::new();
        let target = h.window();
        h.gv.map_new_window(target.clone());
        let previous = h.window();
        h.gv.map_new_window(previous);
        let (token, data) = old_token(&mut h, true, Duration::from_secs(6));
        let input_started = Instant::now();
        h.gv.focus_window(Some(&target));
        h.gv.launch_user_activity(input_started);
        // More typing while this window stays focused also revokes the token.
        h.gv.launch_user_activity(Instant::now());
        h.gv.request_activation(
            token.clone(),
            data,
            target.toplevel().unwrap().wl_surface().clone(),
        );
        assert_eq!(h.gv.focused_window(), Some(target.clone()));
        assert!(!crate::handlers::xdg_activation::is_urgent(&target));
        assert!(h.gv.protocols.activation.data_for_token(&token).is_none());
        assert_eq!(h.gv.consume_launch_token(&token), None);
    }
    #[test]
    fn modal_states_still_deny_attention_for_the_focused_window() {
        use smithay::wayland::xdg_activation::XdgActivationHandler;
        for polkit in [false, true] {
            let mut h = crate::state::tests::Harness::new();
            let target = h.window();
            h.gv.map_new_window(target.clone());
            let (token, data) = old_token(&mut h, true, Duration::from_secs(6));
            h.gv.launch_user_activity(Instant::now());
            let _outcome = if polkit {
                Some(crate::polkit::tests::push_dialog(
                    &mut h.gv,
                    "focused-activation",
                    &["admin"],
                ))
            } else {
                h.gv.protocols.locks.locked = true;
                None
            };
            h.gv.request_activation(token, data, target.toplevel().unwrap().wl_surface().clone());
            assert!(crate::handlers::xdg_activation::is_urgent(&target));
        }
    }
    #[test]
    fn direct_keyboard_focus_gain_clears_urgency_for_all_window_surfaces() {
        use smithay::wayland::xdg_activation::XdgActivationHandler;
        for kind in ["root", "popup", "subsurface"] {
            let mut h = crate::state::tests::Harness::new();
            let target = h.window();
            h.gv.map_new_window(target.clone());
            h.commit_size(&target, 400, 300);
            let focus = match kind {
                "root" => target.toplevel().unwrap().wl_surface().clone(),
                "popup" => h.popup(&target),
                "subsurface" => h.subsurface(&target),
                _ => unreachable!(),
            };
            let previous = h.window();
            h.gv.map_new_window(previous);
            let (token, data) = old_token(&mut h, true, Duration::from_secs(6));
            crate::handlers::xdg_activation::mark_urgent(&target);
            assert!(crate::handlers::xdg_activation::is_urgent(&target));
            let keyboard = h.gv.seat.get_keyboard().unwrap();
            keyboard.set_focus(
                &mut h.gv,
                Some(focus.clone()),
                smithay::utils::SERIAL_COUNTER.next_serial(),
            );
            assert!(
                !crate::handlers::xdg_activation::is_urgent(&target),
                "{kind}"
            );
            h.gv.launch_user_activity(Instant::now());
            h.gv.request_activation(token, data, target.toplevel().unwrap().wl_surface().clone());
            assert_eq!(
                keyboard.current_focus(),
                Some(focus),
                "{kind}: activation must not replace focus"
            );
            assert!(
                !crate::handlers::xdg_activation::is_urgent(&target),
                "{kind}"
            );
        }
    }

    #[test]
    fn compositor_launch_tokens_outlive_client_tokens_and_are_single_use() {
        use smithay::wayland::xdg_activation::XdgActivationHandler;
        for owned in [false, true] {
            let mut h = crate::state::tests::Harness::new();
            let w = h.window();
            h.gv.map_new_window(w.clone());
            let other = h.window();
            h.gv.map_new_window(other.clone());
            let (token, data) = old_token(&mut h, owned, Duration::from_secs(6));
            h.gv.request_activation(
                token.clone(),
                data,
                w.toplevel().unwrap().wl_surface().clone(),
            );
            assert_eq!(crate::handlers::xdg_activation::is_urgent(&w), !owned);
            assert_eq!(h.gv.focused_window(), Some(if owned { w } else { other }));
            assert!(h.gv.protocols.activation.data_for_token(&token).is_none());
            assert_eq!(h.gv.consume_launch_token(&token), None);
        }
    }
    #[test]
    fn user_focus_change_invalidates_late_launch_activation_and_expired_launches_fail() {
        use smithay::wayland::xdg_activation::XdgActivationHandler;
        for age in [Duration::from_secs(6), Duration::from_secs(61)] {
            let mut h = crate::state::tests::Harness::new();
            let w = h.window();
            h.gv.map_new_window(w.clone());
            let (token, data) = old_token(&mut h, true, age);
            let input_started = Instant::now();
            let other = h.window();
            h.gv.map_new_window(other.clone());
            h.gv.launch_user_activity(input_started);
            h.gv.request_activation(token, data, w.toplevel().unwrap().wl_surface().clone());
            assert!(crate::handlers::xdg_activation::is_urgent(&w));
            assert_eq!(h.gv.focused_window(), Some(other));
        }
    }
    #[test]
    fn covered_root_with_visible_subsurface_is_presented() {
        use smithay::backend::renderer::element::{
            Id, RenderElementPresentationState, RenderElementState,
        };
        let mut h = crate::state::tests::Harness::new();
        let w = h.window();
        h.gv.config.modules.animations = false;
        h.gv.features = crate::module::Features::new(&h.gv.config);
        let child = h.subsurface(&w);
        let root = w.toplevel().unwrap().wl_surface();
        let output = Output::new(
            "subsurface-launch".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: (1000, 800).into(),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );
        h.gv.space.map_output(&output, (0, 0));
        h.gv.map_new_window(w.clone());
        h.commit_size(&w, 100, 100);
        h.gv.space.refresh();
        set_app_id(&w, "fixture");
        h.gv.launches
            .pending
            .push(launch(Instant::now(), Some("fixture"), "fixture", "a"));
        h.gv.launch_commit(&w);
        h.gv.launch_mapped(&w);
        let mut states = RenderElementStates::default();
        states.states.insert(
            Id::from(root),
            RenderElementState {
                visible_area: 0,
                presentation_state: RenderElementPresentationState::Skipped,
            },
        );
        assert!(!surface_tree_visible(root, &states));
        states.states.insert(
            Id::from(&child),
            RenderElementState {
                visible_area: 10000,
                presentation_state: RenderElementPresentationState::Rendering { reason: None },
            },
        );
        assert!(surface_tree_visible(root, &states));
        h.gv.launch_submitted(&output, &states);
        assert_eq!(h.gv.launches.observed[0].submitted, Some(output.clone()));
        h.gv.launch_presented(&output, "test");
        assert!(h.gv.launches.pending.is_empty());
    }

    fn source_rect(x: f64) -> crate::anim::Rect {
        use smithay::utils::Rectangle;
        Rectangle::new((x, 700.0).into(), (48.0, 48.0).into())
    }
    #[test]
    fn launch_source_matches_by_app_id_and_by_token() {
        let mut h = crate::state::tests::Harness::new();
        let by_app = h.window();
        let by_token = h.window();
        let other = h.window();
        set_app_id(&by_app, "files");
        set_app_id(&by_token, "mystery");
        set_app_id(&other, "unrelated");
        let now = Instant::now();
        let mut dock = launch(now, Some("files"), "files", "t1");
        dock.source = Some(source_rect(10.0));
        let mut launcher = launch(now, None, "wrapper --run", "t2");
        launcher.source = Some(source_rect(20.0));
        let plain = launch(now, Some("plain"), "plain", "t3");
        h.gv.launches.pending = vec![dock, launcher, plain];
        h.gv.launch_commit(&by_app);
        h.gv.launch_commit(&by_token);
        h.gv.launch_commit(&other);
        assert_eq!(h.gv.launch_source(&by_app), Some(source_rect(10.0)));
        assert_eq!(h.gv.launch_source(&by_token), None, "unknown app id and no token yet");
        h.gv.launch_activation("t2", by_token.toplevel().unwrap().wl_surface());
        assert_eq!(h.gv.launch_source(&by_token), Some(source_rect(20.0)));
        assert_eq!(h.gv.launch_source(&other), None);
        let stranger = h.window();
        assert_eq!(h.gv.launch_source(&stranger), None, "never launched from the shell");
    }
    #[test]
    fn launched_window_grows_from_its_source_and_others_from_the_centre() {
        let mut h = crate::state::tests::Harness::new();
        let launched = h.window();
        set_app_id(&launched, "files");
        let mut dock = launch(Instant::now(), Some("files"), "files", "t1");
        dock.source = Some(source_rect(10.0));
        h.gv.launches.pending = vec![dock];
        h.gv.launch_commit(&launched);
        h.gv.map_new_window(launched.clone());
        let motion = h.gv.window_animations.motion(&launched).expect("animates");
        assert!(motion.active());
        assert_eq!(motion.rect(source_rect(0.0)), source_rect(10.0), "first frame is the dock icon");
        let plain = h.window();
        h.gv.map_new_window(plain.clone());
        let motion = h.gv.window_animations.motion(&plain).expect("animates");
        assert!(motion.active());
        assert_eq!(motion.scale(), 0.94, "the old scale-in from the centre");
        let fallback = source_rect(99.0);
        assert_eq!(motion.rect(fallback), fallback, "no geometry override");
    }
    #[test]
    fn launch_shell_records_the_source() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.launch_shell_from(Instant::now(), "true", Some("x"), Some(source_rect(5.0)));
        h.gv.launch_shell_at(Instant::now(), "true", Some("y"));
        assert_eq!(h.gv.launches.pending[0].source, Some(source_rect(5.0)));
        assert_eq!(h.gv.launches.pending[1].source, None);
    }

    #[test]
    fn late_token_never_reuses_a_logged_fifo_observation() {
        let mut h = crate::state::tests::Harness::new();
        let a = h.window();
        let b = h.window();
        set_app_id(&a, "zenity");
        set_app_id(&b, "zenity");
        let now = Instant::now();
        h.gv.launches.pending = vec![
            launch(now, Some("zenity"), "zenity", "a"),
            launch(now, Some("zenity"), "zenity", "b"),
        ];
        h.gv.launch_commit(&b);
        h.gv.launch_mapped(&b);
        h.gv.launches.observed[0].presented = Some((Instant::now(), "test"));
        h.gv.finish_launches();
        assert!(h.gv.launches.observed[0].logged);
        assert_eq!(h.gv.launches.pending.len(), 1);
        h.gv.launch_activation("b", b.toplevel().unwrap().wl_surface());
        assert!(h.gv.launches.pending[0].surface.is_none());
        h.gv.launch_commit(&a);
        h.gv.launch_mapped(&a);
        assert_eq!(
            h.gv.launches.pending[0].surface,
            Some(a.toplevel().unwrap().wl_surface().id())
        );
        h.gv.launches.observed[1].presented = Some((Instant::now(), "test"));
        h.gv.finish_launches();
        assert!(h.gv.launches.pending.is_empty());
        assert!(h.gv.launches.observed.is_empty());
    }

    #[test]
    fn identities_match_desktop_ids_and_quoted_executables() {
        let l = launch(
            Instant::now(),
            Some("org.example.Editor.desktop"),
            "'/opt/My Apps/editor' '--new'",
            "token",
        );
        assert!(l.matches("ORG.EXAMPLE.EDITOR"));
        assert!(l.matches("editor"));
        assert!(l.matches("org.other.editor"));
        assert!(!l.matches(""));
        assert!(!l.matches("org.example.editor-helper"));
        assert_eq!(executable(""), None);
        assert_eq!(executable("'unterminated"), None);
        assert_eq!(executable("/usr/bin/zenity --info"), Some("zenity".into()));
    }

    #[test]
    fn first_buffer_is_recorded_once_and_repeated_launches_match_fifo() {
        let mut h = crate::state::tests::Harness::new();
        let a = h.window();
        let b = h.window();
        set_app_id(&a, "zenity");
        set_app_id(&b, "zenity");
        let now = Instant::now();
        h.gv.launches.pending = vec![
            launch(now, Some("zenity"), "zenity", "a"),
            launch(now, Some("zenity"), "zenity", "b"),
        ];
        h.gv.launch_commit(&a);
        let commit = h.gv.launches.observed[0].commit;
        h.gv.launch_commit(&a);
        assert_eq!(h.gv.launches.observed.len(), 1);
        assert_eq!(h.gv.launches.observed[0].commit, commit);
        assert!(h.gv.launches.pending[1].surface.is_none());
        h.gv.launch_commit(&b);
        assert_eq!(
            h.gv.launches.pending[0].surface,
            Some(a.toplevel().unwrap().wl_surface().id())
        );
        assert_eq!(
            h.gv.launches.pending[1].surface,
            Some(b.toplevel().unwrap().wl_surface().id())
        );
    }

    #[test]
    fn activation_token_overrides_weak_match_and_can_arrive_before_buffer() {
        let mut h = crate::state::tests::Harness::new();
        let window = h.window();
        set_app_id(&window, "zenity");
        let now = Instant::now();
        h.gv.launches.pending = vec![
            launch(now, Some("zenity"), "zenity", "a"),
            launch(now, Some("zenity"), "zenity", "b"),
        ];
        h.gv.launch_commit(&window);
        let surface = window.toplevel().unwrap().wl_surface();
        h.gv.launch_activation("b", surface);
        assert!(h.gv.launches.pending[0].surface.is_none());
        assert_eq!(h.gv.launches.pending[1].surface, Some(surface.id()));
        h.gv.launch_activation("unknown", surface);
        assert!(h.gv.launches.pending[0].surface.is_none());
        h.gv.launches.observed.clear();
        h.gv.launch_commit(&window);
        assert_eq!(h.gv.launches.observed.len(), 1);
        assert!(h.gv.launches.pending[0].surface.is_none());
    }

    #[test]
    fn log_waits_for_presentation_and_worker_completion_then_retires_once() {
        let mut h = crate::state::tests::Harness::new();
        let window = h.window();
        set_app_id(&window, "zenity");
        h.gv.launches
            .pending
            .push(launch(Instant::now(), Some("zenity"), "zenity", "a"));
        h.gv.launches.pending[0].spawned = Arc::new(OnceLock::new());
        h.gv.launch_commit(&window);
        h.gv.launch_mapped(&window);
        h.gv.finish_launches();
        assert_eq!(h.gv.launches.pending.len(), 1);
        h.gv.launches.observed[0].presented = Some((Instant::now(), "test"));
        h.gv.finish_launches();
        assert_eq!(h.gv.launches.pending.len(), 1);
        h.gv.launches.pending[0]
            .spawned
            .set((Instant::now(), Some(123)))
            .unwrap();
        h.gv.finish_launches();
        assert!(h.gv.launches.pending.is_empty());
        assert!(h.gv.launches.observed.is_empty());
        h.gv.finish_launches();
    }

    #[test]
    fn unmatched_launches_expire_at_thirty_seconds_without_recurring_work() {
        let now = Instant::now();
        let mut tracker = Launches::default();
        tracker.pending.push(launch(now, None, "unknown", "a"));
        tracker.expire(now + LIFETIME - Duration::from_nanos(1));
        assert_eq!(tracker.pending.len(), 1);
        tracker.expire(now + LIFETIME);
        assert!(tracker.pending.is_empty());
        assert!(tracker.observed.is_empty());
        assert!(tracker.expiry.is_none());
    }

    #[test]
    fn overlapping_token_expiry_keeps_new_tokens_and_removes_the_last_timer() {
        let mut h = crate::state::tests::Harness::new();
        let old =
            h.gv.protocols
                .activation
                .create_external_token(None)
                .0
                .clone();
        let fresh =
            h.gv.protocols
                .activation
                .create_external_token(None)
                .0
                .clone();
        let now = Instant::now();
        h.gv.launches.tokens = vec![
            LaunchToken {
                created: now - TOKEN_LIFETIME,
                token: old.clone(),
                focus_unchanged: true,
            },
            LaunchToken {
                created: now,
                token: fresh.clone(),
                focus_unchanged: true,
            },
        ];
        h.gv.arm_launch_expiry();
        h.gv.expire_launches();
        h.gv.arm_launch_expiry();
        assert!(h.gv.protocols.activation.data_for_token(&old).is_none());
        assert!(h.gv.protocols.activation.data_for_token(&fresh).is_some());
        assert_eq!(h.gv.launches.tokens.len(), 1);
        assert_eq!(
            h.gv.launches.expiry.as_ref().unwrap().1,
            now + TOKEN_LIFETIME
        );
        h.gv.launches.tokens[0].created = now - TOKEN_LIFETIME;
        h.gv.expire_launches();
        h.gv.arm_launch_expiry();
        assert!(h.gv.launches.tokens.is_empty());
        assert!(h.gv.launches.expiry.is_none());
        assert!(h.gv.protocols.activation.data_for_token(&fresh).is_none());
    }

    #[test]
    fn worker_preserves_tokens_and_shell_comments_without_waiting_for_apps() {
        let dir = std::env::temp_dir().join(format!(
            "gavno-spawn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let release = dir.join("release");
        let token_file = dir.join("token");
        let complete = dir.join("complete");
        let quote = crate::desktop_entry::shell_quote;
        let first = spawn(
            &format!(
                "while [ ! -f {} ]; do sleep 0.01; done; touch {} # comment",
                quote(&release.to_string_lossy()),
                quote(&complete.to_string_lossy())
            ),
            None,
            None,
        );
        let token = "literal ' $(printf wrong) token";
        let second = spawn(
            &format!(
                "printf '%s' \"$XDG_ACTIVATION_TOKEN\" > {} # comment",
                quote(&token_file.to_string_lossy())
            ),
            Some(token),
            None,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !token_file.exists()
            || std::fs::read_to_string(&token_file).unwrap_or_default() != token
        {
            if Instant::now() > deadline {
                std::fs::write(&release, "").unwrap();
                panic!("the spawner waited for the first app or lost its environment");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(first.get().unwrap().1.is_some());
        assert!(second.get().unwrap().1.is_some());
        std::fs::write(&release, "").unwrap();
        while !complete.exists() {
            assert!(
                Instant::now() < deadline,
                "the detached shell did not finish"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
