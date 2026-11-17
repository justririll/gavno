//! Launch latency, matched by trusted activation token or app identity.
//! Only pending launches/tokens own an expiry timer; the idle desktop does no work here.
use crate::Gavno;
use smithay::{
    backend::renderer::element::RenderElementStates,
    desktop::Window,
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
}
struct Observed {
    surface: ObjectId,
    commit: Instant,
    mapped: Option<Instant>,
    submitted: Option<Output>,
    presented: Option<(Instant, &'static str)>,
}
#[derive(Default)]
pub(crate) struct Launches {
    pending: Vec<Launch>,
    observed: Vec<Observed>,
    expiry: Option<(RegistrationToken, Instant)>,
    tokens: Vec<(Instant, XdgActivationToken)>,
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
            let Some(window) = self.observed.iter().find(|w| Some(&w.surface) == launch.surface.as_ref()) else { return true };
            let (Some((spawned, _)), Some(mapped), Some((presented, presentation))) =
                (launch.spawned.get(), window.mapped, window.presented) else { return true };
            let ms = |time: Instant| time.saturating_duration_since(launch.action).as_secs_f64() * 1000.0;
            tracing::info!(target: "gavno::launch", command = %launch.command,
                app_id = launch.app_id.as_deref().unwrap_or(""), dispatch_ms = ms(launch.dispatched), spawn_ms = ms(*spawned),
                commit_ms = ms(window.commit), mapped_ms = ms(mapped), presented_ms = ms(presented),
                commit_to_present_ms = presented.saturating_duration_since(window.commit).as_secs_f64() * 1000.0,
                presentation, "app launch");
            false
        });
        if self.pending.is_empty() {
            self.observed.clear();
        }
    }
}
impl Gavno {
    pub(crate) fn launch_shell(&mut self, command: &str, app_id: Option<&str>) {
        self.launch_shell_at(Instant::now(), command, app_id);
    }
    pub(crate) fn launch_shell_at(&mut self, action: Instant, command: &str, app_id: Option<&str>) {
        self.expire_launches();
        let token = self
            .protocols
            .activation
            .create_external_token(None)
            .0
            .clone();
        let token_text = token.as_str().to_owned();
        self.launches.tokens.push((Instant::now(), token));
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
        });
        self.arm_launch_expiry();
    }
    fn expire_launches(&mut self) {
        let now = Instant::now();
        self.launches.expire(now);
        self.launches.tokens.retain(|(created, token)| {
            if now.duration_since(*created) < LIFETIME {
                true
            } else {
                self.protocols.activation.remove_token(token);
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
                    .map(|(created, _)| *created + LIFETIME),
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
            .retain(|(_, owned)| owned.as_str() != token);
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
                && states
                    .element_render_state(toplevel.wl_surface())
                    .is_some_and(|s| s.visible_area > 0)
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
        h.gv.launches.tokens = vec![(now - LIFETIME, old.clone()), (now, fresh.clone())];
        h.gv.arm_launch_expiry();
        h.gv.expire_launches();
        h.gv.arm_launch_expiry();
        assert!(h.gv.protocols.activation.data_for_token(&old).is_none());
        assert!(h.gv.protocols.activation.data_for_token(&fresh).is_some());
        assert_eq!(h.gv.launches.tokens.len(), 1);
        assert_eq!(h.gv.launches.expiry.as_ref().unwrap().1, now + LIFETIME);
        h.gv.launches.tokens[0].0 = now - LIFETIME;
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
