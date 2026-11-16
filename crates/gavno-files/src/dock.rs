//! The docked terminal (F4): a VTE shell that slides up under the file view
//! and shares its folder with the window. `cd` in the shell moves the window
//! (through the OSC 7 working-directory report) and navigating the window
//! runs `cd` in the shell while it sits idle at its prompt.
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

pub type PathHandler = Rc<dyn Fn(PathBuf)>;

pub struct Dock {
    pub revealer: gtk::Revealer,
    pub terminal: vte4::Terminal,
    title: gtk::Label,
    shell: Cell<Option<glib::Pid>>,
    cwd: RefCell<Option<PathBuf>>,
    height: Cell<i32>,
    report: Rc<dyn Fn(String)>,
}

impl Dock {
    /// `on_cwd` gets the shell's new folder; `on_popout` the folder a
    /// separate terminal window should open in; `report` user-facing errors.
    pub fn new(on_cwd: PathHandler, on_popout: PathHandler, report: Rc<dyn Fn(String)>) -> Rc<Self> {
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
        // VTE asks for its current row count as natural height, so a dock
        // could only ever grow. The scrolled window hides that request and
        // the dock's height comes from the drag alone.
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .vexpand(true)
            .child(&terminal)
            .build();
        body.append(&scroller);
        body.set_height_request(DEFAULT_HEIGHT);
        // The terminal expands inside the dock only; without this the
        // expand flag propagates up and the dock takes the window's spare
        // height from the file view, so its height ignores the drag.
        body.set_vexpand(false);
        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .transition_duration(260)
            .reveal_child(false)
            .child(&body)
            .vexpand(false)
            .build();

        let this = Rc::new(Self {
            revealer,
            terminal,
            title,
            shell: Cell::new(None),
            cwd: RefCell::new(None),
            height: Cell::new(DEFAULT_HEIGHT),
            report,
        });

        // Resize by dragging the header; dragging it nearly flat hides it.
        // The header moves with the drag, so offsets are measured in window
        // coordinates: header-relative offsets would cancel themselves out.
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
                let Some(widget) = gesture.widget() else { return };
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
                let (Some(widget), Some((x, y))) = (gesture.widget(), gesture.start_point()) else { return };
                let Some(now) = window_y(&widget, x + dx, y + dy) else { return };
                let (height, from) = start.get();
                // Never taller than the window allows: a larger request would
                // grow the window instead of moving only the dragged edge.
                let max = widget.root().map_or(900, |r| r.height() - 160).max(MIN_HEIGHT);
                let height = (height - (now - from).round() as i32).clamp(HIDE_HEIGHT - 20, max);
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

        // Clipboard keys; plain Ctrl+C/V belong to the shell.
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak]
            this,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, state| {
                let wanted = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
                if !state.contains(wanted) {
                    return glib::Propagation::Proceed;
                }
                match key.to_lower() {
                    gdk::Key::c => this.terminal.copy_clipboard_format(vte4::Format::Text),
                    gdk::Key::v => this.terminal.paste_clipboard(),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        ));
        this.terminal.add_controller(keys);

        #[allow(deprecated)]
        this.terminal.connect_current_directory_uri_changed(glib::clone!(
            #[weak]
            this,
            move |terminal| {
                let Some(path) = terminal
                    .current_directory_uri()
                    // fish reports file://<hostname>/path; the host is ours.
                    .and_then(|uri| glib::filename_from_uri(&uri).ok())
                    .map(|(path, _host)| path)
                else {
                    return;
                };
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
                this.cwd.replace(None);
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
        if self.shell.get().is_none() {
            self.spawn(dir);
        } else {
            self.follow(dir);
        }
        self.revealer.set_reveal_child(true);
        self.terminal.grab_focus();
    }

    pub fn hide(&self) {
        let had_focus = self.terminal.has_focus();
        self.revealer.set_reveal_child(false);
        if had_focus {
            self.revealer.parent().map(|p| p.child_focus(gtk::DirectionType::TabForward));
        }
    }

    /// The window moved to `dir`: `cd` there if the shell is idle at its prompt.
    pub fn follow(&self, dir: &Path) {
        let Some(pid) = self.shell.get() else { return };
        if self.cwd.borrow().as_deref() == Some(dir) || !self.shell_idle(pid) {
            return;
        }
        let Some(text) = dir.to_str() else { return };
        // Backslashes mean different things in sh and fish single quotes.
        if text.contains('\\') || text.contains('\n') {
            return;
        }
        let quoted = glib::shell_quote(text);
        // Ctrl+E Ctrl+U clears a half-typed line; the leading space keeps
        // the command out of bash/fish history.
        let mut command = b"\x05\x15 cd -- ".to_vec();
        command.extend_from_slice(quoted.to_string_lossy().as_bytes());
        command.push(b'\r');
        self.terminal.feed_child(&command);
        self.cwd.replace(Some(dir.to_path_buf()));
        self.update_title();
    }

    fn pop_out(&self, on_popout: &PathHandler) {
        if let Some(dir) = self.cwd.borrow().clone() {
            on_popout(dir);
        }
        self.hide();
    }

    /// The shell's folder as last reported or set.
    pub fn cwd(&self) -> Option<PathBuf> {
        self.cwd.borrow().clone()
    }

    /// True when the shell itself owns the terminal, i.e. no program runs.
    fn shell_idle(&self, pid: glib::Pid) -> bool {
        let Some(pty) = self.terminal.pty() else { return false };
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
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        self.cwd.replace(Some(dir.to_path_buf()));
        self.update_title();
        let weak = Rc::downgrade(self);
        self.terminal.spawn_async(
            vte4::PtyFlags::DEFAULT,
            dir.to_str(),
            &[shell.as_str()],
            &[],
            glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            gio::Cancellable::NONE,
            move |result| {
                let Some(this) = weak.upgrade() else { return };
                match result {
                    Ok(pid) => this.shell.set(Some(pid)),
                    Err(e) => {
                        this.hide();
                        (this.report)(format!("Terminal: {e}"));
                    }
                }
            },
        );
    }
}

/// `~/Projects/ds` style folder names for titles.
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
