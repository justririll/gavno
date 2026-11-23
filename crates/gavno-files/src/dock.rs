//! The docked terminal (F4): a VTE shell that slides up under the file view
//! and shares its folder with the window. `cd` in the shell moves the window
//! (through the OSC 7 working-directory report) and navigating the window
//! runs `cd` in the shell while it sits idle at its prompt.
use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

pub type PathHandler = Rc<dyn Fn(PathBuf)>;

#[cfg(feature = "vte")]
mod inner {
    use super::{pretty, PathHandler};
    use gtk::{gdk, prelude::*};
    use std::{
        cell::{Cell, RefCell},
        os::fd::AsRawFd,
        path::{Path, PathBuf},
        rc::Rc,
    };
    use vte4::prelude::*;

    const DEFAULT_HEIGHT: i32 = 220;
    const MIN_HEIGHT: i32 = 90;
    /// Dragging the handle below this height hides the terminal.
    const HIDE_HEIGHT: i32 = 60;

    const PALETTE: [&str; 16] = [
        "#1b1f2a", "#f07178", "#a6da95", "#e5c07b", "#7aa2f7", "#c3a6ff", "#7fd3d3", "#c0c6d4",
        "#4a5266", "#ff8b92", "#b8eba5", "#f5d491", "#93b4ff", "#d4bcff", "#96e6e6", "#eef1f7",
    ];

    pub struct Dock {
        pub revealer: gtk::Revealer,
        pub terminal: vte4::Terminal,
        title: gtk::Label,
        shell: Cell<Option<glib::Pid>>,
        spawning: Cell<bool>,
        cwd: RefCell<Option<PathBuf>>,
        pending_cwd: RefCell<Option<PathBuf>>,
        height: Cell<i32>,
        report: Rc<dyn Fn(String)>,
    }

    impl Dock {
        /// `on_cwd` gets the shell's new folder; `on_popout` the folder a
        /// separate terminal window should open in; `report` user-facing errors.
        pub fn new(
            on_cwd: PathHandler,
            on_popout: PathHandler,
            report: Rc<dyn Fn(String)>,
        ) -> Rc<Self> {
            let terminal = vte4::Terminal::new();
            terminal.set_vexpand(true);
            terminal.set_hexpand(true);
            terminal.set_scrollback_lines(10_000);
            terminal.set_mouse_autohide(true);
            terminal.set_font(Some(&gtk::pango::FontDescription::from_string(
                "IBM Plex Mono, Geist Mono, monospace 10",
            )));
            let rgba = |s: &str| gdk::RGBA::parse(s).unwrap_or(gdk::RGBA::BLACK);
            let palette: Vec<gdk::RGBA> = PALETTE.iter().map(|c| rgba(c)).collect();
            terminal.set_colors(
                Some(&rgba("#d9dbe7")),
                Some(&rgba("#13141d")),
                &palette.iter().collect::<Vec<_>>(),
            );
            terminal.set_color_cursor(Some(&rgba("#f2a46a")));
            terminal.set_color_cursor_foreground(Some(&rgba("#13141d")));
            terminal.set_color_highlight(Some(&rgba("#2b3245")));
            terminal.set_color_highlight_foreground(Some(&rgba("#e6eaf2")));
            terminal.add_css_class("dock-terminal");

            let title = gtk::Label::new(Some("Terminal"));
            title.set_xalign(0.);
            title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            title.add_css_class("dock-title");
            let grip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            grip.add_css_class("dock-grip");
            grip.set_valign(gtk::Align::Center);
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            let popout = gtk::Button::from_icon_name("window-new-symbolic");
            popout.set_tooltip_text(Some("Separate window (Shift+F4)"));
            let close = gtk::Button::from_icon_name("window-close-symbolic");
            close.set_tooltip_text(Some("Hide (F4)"));
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            head.add_css_class("dock-head");
            head.append(&title);
            head.append(&grip);
            head.append(&spacer);
            head.append(&popout);
            head.append(&close);
            head.set_cursor_from_name(Some("ns-resize"));

            let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
            body.add_css_class("dock");
            body.append(&head);
            let scroller = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vscrollbar_policy(gtk::PolicyType::External)
                .vexpand(true)
                .child(&terminal)
                .build();
            body.append(&scroller);
            body.set_height_request(DEFAULT_HEIGHT);
            body.set_vexpand(false);
            let revealer = gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::SlideUp)
                .transition_duration(220)
                .reveal_child(false)
                .child(&body)
                .vexpand(false)
                .build();

            let this = Rc::new(Self {
                revealer,
                terminal,
                title,
                shell: Cell::new(None),
                spawning: Cell::new(false),
                cwd: RefCell::new(None),
                pending_cwd: RefCell::new(None),
                height: Cell::new(DEFAULT_HEIGHT),
                report,
            });

            let drag = gtk::GestureDrag::new();
            let start = Rc::new(Cell::new((DEFAULT_HEIGHT, 0.0f32)));
            let window_y = |widget: &gtk::Widget, x: f64, y: f64| {
                let root = widget.root()?;
                widget
                    .compute_point(&root, &gtk::graphene::Point::new(x as f32, y as f32))
                    .map(|p| p.y())
            };
            drag.connect_drag_begin(glib::clone!(
                #[weak]
                this,
                #[strong]
                start,
                move |gesture, x, y| {
                    let Some(widget) = gesture.widget() else {
                        return;
                    };
                    start.set((this.height.get(), window_y(&widget, x, y).unwrap_or(0.0)));
                }
            ));
            drag.connect_drag_update(glib::clone!(
                #[weak]
                this,
                #[weak]
                body,
                #[strong]
                start,
                move |gesture, dx, dy| {
                    let (Some(widget), Some((x, y))) = (gesture.widget(), gesture.start_point())
                    else {
                        return;
                    };
                    let Some(now) = window_y(&widget, x + dx, y + dy) else {
                        return;
                    };
                    let (height, from) = start.get();
                    let max = widget
                        .root()
                        .map_or(900, |r| r.height() - 160)
                        .max(MIN_HEIGHT);
                    let height =
                        (height - (now - from).round() as i32).clamp(HIDE_HEIGHT - 20, max);
                    body.set_height_request(height.max(MIN_HEIGHT));
                    this.height.set(height);
                }
            ));
            drag.connect_drag_end(glib::clone!(
                #[weak]
                this,
                #[weak]
                body,
                move |_, _, _| {
                    if this.height.get() < HIDE_HEIGHT {
                        this.height.set(DEFAULT_HEIGHT);
                        body.set_height_request(DEFAULT_HEIGHT);
                        this.hide();
                    }
                }
            ));
            head.add_controller(drag);

            close.connect_clicked(glib::clone!(
                #[weak]
                this,
                move |_| this.hide()
            ));
            popout.connect_clicked(glib::clone!(
                #[weak]
                this,
                #[strong]
                on_popout,
                move |_| this.pop_out(&on_popout)
            ));

            // Clipboard and zoom keys; plain Ctrl+C/V belong to the shell.
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(glib::clone!(
                #[weak]
                this,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_, key, _, state| {
                    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
                    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
                    if ctrl && shift {
                        match key.to_lower() {
                            gdk::Key::c => {
                                this.terminal.copy_clipboard_format(vte4::Format::Text);
                                return glib::Propagation::Stop;
                            }
                            gdk::Key::v => {
                                this.terminal.paste_clipboard();
                                return glib::Propagation::Stop;
                            }
                            _ => {}
                        }
                    }
                    if ctrl {
                        let scale = this.terminal.font_scale();
                        match key {
                            gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => {
                                this.terminal.set_font_scale((scale + 0.1).min(2.5));
                                return glib::Propagation::Stop;
                            }
                            gdk::Key::minus | gdk::Key::KP_Subtract => {
                                this.terminal.set_font_scale((scale - 0.1).max(0.5));
                                return glib::Propagation::Stop;
                            }
                            gdk::Key::_0 | gdk::Key::KP_0 => {
                                this.terminal.set_font_scale(1.0);
                                return glib::Propagation::Stop;
                            }
                            _ => {}
                        }
                    }
                    glib::Propagation::Proceed
                }
            ));
            this.terminal.add_controller(keys);

            #[allow(deprecated)]
            this.terminal.connect_current_directory_uri_changed(glib::clone!(
                #[weak]
                this,
                move |terminal| {
                    let local_host = glib::host_name();
                    let Some((path, host)) = terminal
                        .current_directory_uri()
                        .and_then(|uri| glib::filename_from_uri(&uri).ok())
                    else {
                        return;
                    };
                    if let Some(h) = host {
                        if !h.is_empty() && h != "localhost" && h != local_host {
                            return;
                        }
                    }
                    if this.cwd.borrow().as_deref() == Some(path.as_path()) {
                        return;
                    }
                    this.cwd.replace(Some(path.clone()));
                    this.update_title();
                    on_cwd(path);
                }
            ));
            this.terminal.connect_child_exited(glib::clone!(
                #[weak]
                this,
                move |terminal, _| {
                    this.shell.set(None);
                    this.spawning.set(false);
                    this.cwd.replace(None);
                    this.pending_cwd.replace(None);
                    this.update_title();
                    terminal.reset(true, true);
                    this.hide();
                }
            ));
            this
        }

        pub fn is_open(&self) -> bool {
            self.revealer.reveals_child()
        }

        pub fn has_focus(&self) -> bool {
            self.terminal.has_focus()
        }

        /// Slides the terminal up in `dir`, starting the shell on first use.
        pub fn open(self: &Rc<Self>, dir: &Path) {
            self.revealer.set_reveal_child(true);
            if self.shell.get().is_none() {
                if self.spawning.get() {
                    self.pending_cwd.replace(Some(dir.to_path_buf()));
                } else {
                    self.pending_cwd.replace(None);
                    self.spawn(dir);
                }
            } else {
                self.pending_cwd.replace(None);
                self.send_cd(dir);
            }
            self.terminal.grab_focus();
        }

        pub fn hide(&self) {
            let had_focus = self.terminal.has_focus();
            self.revealer.set_reveal_child(false);
            if had_focus {
                if let Some(p) = self.revealer.parent() {
                    p.child_focus(gtk::DirectionType::TabForward);
                }
            }
        }

        /// The window moved to `dir`: sync if the dock is open, or defer until opened.
        pub fn follow(&self, dir: &Path) {
            if self.spawning.get() {
                self.pending_cwd.replace(Some(dir.to_path_buf()));
                return;
            }
            if self.shell.get().is_none() {
                return;
            }
            if !self.is_open() {
                self.pending_cwd.replace(Some(dir.to_path_buf()));
                return;
            }
            if self.terminal.has_focus() {
                return;
            }
            self.send_cd(dir);
        }

        fn send_cd(&self, dir: &Path) {
            let Some(pid) = self.shell.get() else {
                return;
            };
            if self.cwd.borrow().as_deref() == Some(dir) {
                return;
            }
            if !self.shell_idle(pid) {
                self.pending_cwd.replace(Some(dir.to_path_buf()));
                return;
            }
            use std::os::unix::ffi::OsStrExt;
            let bytes = dir.as_os_str().as_bytes();
            if bytes.contains(&b'\\') || bytes.iter().any(|b| b.is_ascii_control()) {
                return;
            }
            let quoted = glib::shell_quote(dir);
            let mut command = b"\x05\x15 cd -- ".to_vec();
            command.extend_from_slice(quoted.as_bytes());
            command.push(b'\r');
            self.terminal.feed_child(&command);
        }

        fn pop_out(&self, on_popout: &PathHandler) {
            if let Some(dir) = self.cwd.borrow().clone() {
                on_popout(dir);
            }
            self.hide();
        }

        pub fn cwd(&self) -> Option<PathBuf> {
            self.cwd.borrow().clone()
        }

        fn shell_idle(&self, pid: glib::Pid) -> bool {
            let Some(pty) = self.terminal.pty() else {
                return false;
            };
            let group = unsafe { libc::tcgetpgrp(pty.fd().as_raw_fd()) };
            group == pid.0
        }

        fn update_title(&self) {
            let text = match self.cwd.borrow().as_deref() {
                Some(dir) => format!("Terminal · {}", pretty(dir)),
                None => "Terminal".into(),
            };
            self.title.set_text(&text);
        }

        fn spawn(self: &Rc<Self>, dir: &Path) {
            let Some(dir_str) = dir.to_str() else {
                self.hide();
                (self.report)("Terminal: folder path is not valid UTF-8".into());
                return;
            };
            let shell = std::env::var("SHELL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/bin/sh".into());
            self.spawning.set(true);
            self.cwd.replace(Some(dir.to_path_buf()));
            self.update_title();
            let weak = Rc::downgrade(self);
            self.terminal.spawn_async(
                vte4::PtyFlags::DEFAULT,
                Some(dir_str),
                &[shell.as_str()],
                &[],
                glib::SpawnFlags::DEFAULT,
                || {},
                -1,
                gio::Cancellable::NONE,
                move |result| {
                    let Some(this) = weak.upgrade() else {
                        return;
                    };
                    this.spawning.set(false);
                    match result {
                        Ok(pid) => {
                            this.shell.set(Some(pid));
                            if let Some(pending) = this.pending_cwd.borrow_mut().take() {
                                if this.is_open()
                                    && this.cwd.borrow().as_deref() != Some(pending.as_path())
                                {
                                    this.send_cd(&pending);
                                }
                            }
                        }
                        Err(e) => {
                            this.cwd.replace(None);
                            this.pending_cwd.replace(None);
                            this.update_title();
                            this.hide();
                            (this.report)(format!("Terminal: {e}"));
                        }
                    }
                },
            );
        }
    }
}

#[cfg(not(feature = "vte"))]
mod inner {
    use super::PathHandler;
    use std::{
        path::{Path, PathBuf},
        rc::Rc,
    };

    pub struct Dock {
        pub revealer: gtk::Revealer,
        on_popout: PathHandler,
    }

    impl Dock {
        pub fn new(
            _on_cwd: PathHandler,
            on_popout: PathHandler,
            _report: Rc<dyn Fn(String)>,
        ) -> Rc<Self> {
            let revealer = gtk::Revealer::builder()
                .reveal_child(false)
                .vexpand(false)
                .build();
            Rc::new(Self {
                revealer,
                on_popout,
            })
        }

        pub fn is_open(&self) -> bool {
            false
        }

        pub fn has_focus(&self) -> bool {
            false
        }

        pub fn open(self: &Rc<Self>, dir: &Path) {
            (self.on_popout)(dir.to_path_buf());
        }

        pub fn hide(&self) {}

        pub fn follow(&self, _dir: &Path) {}

        pub fn cwd(&self) -> Option<PathBuf> {
            None
        }
    }
}

pub use inner::Dock;

#[cfg_attr(not(feature = "vte"), allow(dead_code))]
pub fn pretty(dir: &Path) -> String {
    let home = glib::home_dir();
    match dir.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_shorten_home() {
        let home = glib::home_dir();
        assert_eq!(pretty(&home), "~");
        assert_eq!(pretty(&home.join("Projects/ds")), "~/Projects/ds");
        assert_eq!(pretty(Path::new("/etc")), "/etc");
    }
}
