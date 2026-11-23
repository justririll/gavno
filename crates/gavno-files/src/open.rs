//! Native application launching (including Terminal=true desktop apps)
//! and an in-process GTK4 "Open With..." chooser with no portal dependency.
use crate::{
    config::Config,
    model::{self, FileEntry},
    views::Report,
};
use adw::prelude::*;
use std::{
    cell::RefCell,
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExecKind {
    None,
    Elf,
    Script,
}

pub fn open_entry(
    window: &adw::ApplicationWindow,
    entry: FileEntry,
    config: Config,
    report: Report,
) {
    if entry.is_broken_symlink {
        report(format!("Broken symbolic link: {}", entry.display_name));
        return;
    }
    let path = entry.path.clone();
    let mode = entry.mode;
    let initial_ct = entry.content_type.clone();
    let window = window.clone();
    glib::spawn_future_local(async move {
        let sniff_path = path.clone();
        let (exec_kind, mime) = gio::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            let is_reg = std::fs::metadata(&sniff_path).is_ok_and(|m| m.is_file());
            let n = if is_reg {
                std::fs::File::open(&sniff_path)
                    .and_then(|mut f| f.read(&mut buf))
                    .unwrap_or(0)
            } else {
                0
            };
            let sample = &buf[..n];
            let exec_kind = if is_reg && (mode & 0o111) != 0 {
                if sample.starts_with(b"\x7fELF") {
                    ExecKind::Elf
                } else if sample.starts_with(b"#!") {
                    ExecKind::Script
                } else {
                    ExecKind::None
                }
            } else {
                ExecKind::None
            };
            let mime = if initial_ct.is_empty() || initial_ct == "application/octet-stream" {
                let (ct, _) = gio::content_type_guess(
                    sniff_path.file_name().and_then(|s| s.to_str()),
                    Some(sample),
                );
                ct.to_string()
            } else {
                initial_ct
            };
            (exec_kind, mime)
        })
        .await
        .unwrap_or((ExecKind::None, entry.content_type.clone()));

        if exec_kind != ExecKind::None {
            prompt_executable(&window, entry, exec_kind, mime, config, report).await;
            return;
        }
        launch_with_default(&window, entry, mime, config, report).await;
    });
}

async fn prompt_executable(
    window: &adw::ApplicationWindow,
    entry: FileEntry,
    kind: ExecKind,
    mime: String,
    config: Config,
    report: Report,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Run “{}”?", entry.display_name))
        .body("This file has executable permissions.")
        .build();
    dialog.add_response("cancel", "Cancel");
    if kind == ExecKind::Script {
        dialog.add_response("edit", "Open as Text");
    }
    dialog.add_response("terminal", "Run in Terminal");
    dialog.add_response("run", "Run");
    dialog.set_close_response("cancel");
    dialog.set_default_response(Some(if kind == ExecKind::Script {
        "edit"
    } else {
        "terminal"
    }));
    dialog.set_response_appearance("run", adw::ResponseAppearance::Suggested);

    match dialog.choose_future(Some(window)).await.as_str() {
        "run" => {
            if let Err(e) = spawn_executable(&entry.path) {
                report(e);
            }
        }
        "terminal" => {
            let cmd = vec![entry.path.to_string_lossy().into_owned()];
            let cwd = entry.path.parent().unwrap_or_else(|| Path::new("/"));
            if let Err(e) = spawn_in_terminal(&config.terminal_command(), cwd, &cmd) {
                report(e);
            }
        }
        "edit" => {
            let text_mime = if model::is_textual_mime(&mime) {
                mime
            } else {
                "text/plain".to_owned()
            };
            launch_with_default(window, entry, text_mime, config, report).await;
        }
        _ => {}
    }
}

async fn launch_with_default(
    window: &adw::ApplicationWindow,
    entry: FileEntry,
    mime: String,
    config: Config,
    report: Report,
) {
    if let Some(app) = gio::AppInfo::default_for_type(&mime, false) {
        if let Err(e) = launch_app(&app, &entry, &config).await {
            report(e);
        }
    } else {
        open_with_dialog(window, entry, Some(mime), config, report);
    }
}

pub fn open_with_dialog(
    parent: &adw::ApplicationWindow,
    entry: FileEntry,
    resolved_mime: Option<String>,
    config: Config,
    report: Report,
) {
    let dialog = adw::Window::builder()
        .title(format!("Open “{}” With", entry.display_name))
        .transient_for(parent)
        .modal(true)
        .default_width(460)
        .default_height(540)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    let cancel_btn = gtk::Button::with_label("Cancel");
    let open_btn = gtk::Button::with_label("Open");
    open_btn.add_css_class("suggested-action");
    open_btn.set_sensitive(false);
    header.pack_start(&cancel_btn);
    header.pack_end(&open_btn);
    root.append(&header);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search applications…"));
    search.set_margin_start(12);
    search.set_margin_end(12);
    search.set_margin_top(10);
    search.set_margin_bottom(8);
    root.append(&search);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("boxed-list");
    list.set_margin_start(12);
    list.set_margin_end(12);
    list.set_margin_bottom(10);
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .build();
    root.append(&scroll);

    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    bottom.set_margin_start(14);
    bottom.set_margin_end(14);
    bottom.set_margin_top(8);
    bottom.set_margin_bottom(12);
    let show_all = gtk::CheckButton::with_label("Show all applications");
    let set_default = gtk::CheckButton::with_label("Always use for this type");
    set_default.set_hexpand(true);
    set_default.set_halign(gtk::Align::End);
    bottom.append(&show_all);
    bottom.append(&set_default);
    root.append(&bottom);
    dialog.set_content(Some(&root));

    let mime = resolved_mime.unwrap_or_else(|| {
        if entry.content_type.is_empty() {
            "application/octet-stream".to_owned()
        } else {
            entry.content_type.clone()
        }
    });

    let mut recommended = Vec::new();
    let mut seen = HashSet::new();
    for app in gio::AppInfo::recommended_for_type(&mime)
        .into_iter()
        .chain(gio::AppInfo::all_for_type(&mime))
    {
        if app
            .downcast_ref::<gio::DesktopAppInfo>()
            .is_some_and(|d| d.is_hidden())
        {
            continue;
        }
        let key = app_key(&app);
        if seen.insert(key) {
            recommended.push(app);
        }
    }
    let mut all_apps = recommended.clone();
    for app in gio::AppInfo::all() {
        let should_show = app
            .downcast_ref::<gio::DesktopAppInfo>()
            .map(|d| d.should_show())
            .unwrap_or(true);
        if !should_show {
            continue;
        }
        let key = app_key(&app);
        if seen.insert(key) {
            all_apps.push(app);
        }
    }

    let displayed: Rc<RefCell<Vec<gio::AppInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let populate = Rc::new(glib::clone!(
        #[weak]
        list,
        #[weak]
        search,
        #[weak]
        show_all,
        #[weak]
        open_btn,
        #[strong]
        displayed,
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let q = search.text().trim().to_lowercase();
            let include_all = show_all.is_active() || !q.is_empty() || recommended.is_empty();
            let source = if include_all { &all_apps } else { &recommended };
            let mut shown = Vec::new();
            for app in source {
                let name = app.display_name().to_string();
                let desc = app
                    .description()
                    .map(|d| d.to_string())
                    .unwrap_or_default();
                if !q.is_empty()
                    && !name.to_lowercase().contains(&q)
                    && !desc.to_lowercase().contains(&q)
                {
                    continue;
                }
                let row = gtk::ListBoxRow::new();
                let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                hbox.set_margin_top(6);
                hbox.set_margin_bottom(6);
                hbox.set_margin_start(8);
                hbox.set_margin_end(8);
                let img = match app.icon() {
                    Some(gicon) => gtk::Image::from_gicon(&gicon),
                    None => gtk::Image::from_icon_name("application-x-executable-symbolic"),
                };
                img.set_pixel_size(32);
                hbox.append(&img);
                let vbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
                let title = gtk::Label::new(Some(&name));
                title.set_xalign(0.);
                title.set_ellipsize(gtk::pango::EllipsizeMode::End);
                vbox.append(&title);
                if !desc.is_empty() {
                    let sub = gtk::Label::new(Some(&desc));
                    sub.set_xalign(0.);
                    sub.set_ellipsize(gtk::pango::EllipsizeMode::End);
                    sub.add_css_class("dim-label");
                    sub.add_css_class("caption");
                    vbox.append(&sub);
                }
                hbox.append(&vbox);
                row.set_child(Some(&hbox));
                list.append(&row);
                shown.push(app.clone());
            }
            displayed.replace(shown);
            if let Some(first) = list.row_at_index(0) {
                list.select_row(Some(&first));
                open_btn.set_sensitive(true);
            } else {
                open_btn.set_sensitive(false);
            }
        }
    ));
    populate();

    search.connect_search_changed(glib::clone!(
        #[strong]
        populate,
        move |_| populate()
    ));
    show_all.connect_toggled(glib::clone!(
        #[strong]
        populate,
        move |_| populate()
    ));
    list.connect_row_selected(glib::clone!(
        #[weak]
        open_btn,
        move |_, row| open_btn.set_sensitive(row.is_some())
    ));

    let launch_selected = Rc::new(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        list,
        #[weak]
        set_default,
        #[strong]
        displayed,
        #[strong]
        entry,
        #[strong]
        mime,
        #[strong]
        config,
        #[strong]
        report,
        move || {
            let Some(row) = list.selected_row() else {
                return;
            };
            let idx = row.index() as usize;
            let Some(app) = displayed.borrow().get(idx).cloned() else {
                return;
            };
            let make_default = set_default.is_active();
            dialog.close();
            if make_default {
                if let Err(e) = app.set_as_default_for_type(&mime) {
                    report(format!("Cannot set default app: {e}"));
                }
            }
            let entry = entry.clone();
            let config = config.clone();
            let report = report.clone();
            glib::spawn_future_local(async move {
                if let Err(e) = launch_app(&app, &entry, &config).await {
                    report(e);
                }
            });
        }
    ));

    list.connect_row_activated(glib::clone!(
        #[strong]
        launch_selected,
        move |_, _| launch_selected()
    ));
    open_btn.connect_clicked(glib::clone!(
        #[strong]
        launch_selected,
        move |_| launch_selected()
    ));
    cancel_btn.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| dialog.close()
    ));
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        dialog,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                dialog.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    ));
    dialog.add_controller(keys);
    dialog.present();
}

fn app_key(app: &gio::AppInfo) -> String {
    app.id()
        .map(|s| s.to_string())
        .unwrap_or_else(|| app.name().to_string())
}

fn desktop_cwd(app: &gio::AppInfo, entry: &FileEntry) -> PathBuf {
    if let Some(dir) = app
        .downcast_ref::<gio::DesktopAppInfo>()
        .and_then(|d| d.string("Path"))
        .map(|s| PathBuf::from(s.as_str()))
        .filter(|p| !p.as_os_str().is_empty() && p.is_dir())
    {
        return dir;
    }
    entry
        .path
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .to_path_buf()
}

async fn launch_app(
    app: &gio::AppInfo,
    entry: &FileEntry,
    config: &Config,
) -> Result<(), String> {
    let requires_term = app
        .downcast_ref::<gio::DesktopAppInfo>()
        .is_some_and(|d| d.boolean("Terminal"));
    if requires_term {
        let cmdline = app
            .downcast_ref::<gio::DesktopAppInfo>()
            .and_then(|d| d.string("Exec"))
            .map(|s| s.to_string())
            .or_else(|| app.commandline().map(|p| p.to_string_lossy().into_owned()))
            .ok_or_else(|| format!("No command line for {}", app.display_name()))?;
        let argv = expand_desktop_exec(&cmdline, &[&entry.path], &[&entry.uri])?;
        let cwd = desktop_cwd(app, entry);
        return spawn_in_terminal(&config.terminal_command(), &cwd, &argv);
    }

    let ctx = gtk::gdk::Display::default().map(|d| d.app_launch_context());
    match app
        .launch_uris_future(&[entry.uri.as_str()], ctx.as_ref())
        .await
    {
        Ok(()) => Ok(()),
        Err(gio_err) => {
            if let Some(cmdline) = app
                .downcast_ref::<gio::DesktopAppInfo>()
                .and_then(|d| d.string("Exec"))
            {
                let argv = expand_desktop_exec(cmdline.as_str(), &[&entry.path], &[&entry.uri])?;
                let cwd = desktop_cwd(app, entry);
                spawn_detached(&argv, Some(&cwd)).map_err(|e| format!("{gio_err}; {e}"))
            } else {
                Err(gio_err.to_string())
            }
        }
    }
}

pub fn expand_desktop_exec(
    cmdline: &str,
    paths: &[&Path],
    uris: &[&str],
) -> Result<Vec<String>, String> {
    let parsed = glib::shell_parse_argv(cmdline).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut inserted = false;
    for raw in parsed {
        let token = raw.to_string_lossy().into_owned();
        if matches!(
            token.as_str(),
            "%i" | "%c" | "%k" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m"
        ) {
            continue;
        }
        if token == "%f" {
            if let Some(p) = paths.first() {
                out.push(p.to_string_lossy().into_owned());
            }
            inserted = true;
            continue;
        }
        if token == "%F" {
            for p in paths {
                out.push(p.to_string_lossy().into_owned());
            }
            inserted = true;
            continue;
        }
        if token == "%u" {
            if let Some(u) = uris.first() {
                out.push((*u).to_owned());
            }
            inserted = true;
            continue;
        }
        if token == "%U" {
            for u in uris {
                out.push((*u).to_owned());
            }
            inserted = true;
            continue;
        }
        let mut expanded = String::with_capacity(token.len());
        let mut chars = token.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                expanded.push(c);
                continue;
            }
            match chars.peek().copied() {
                Some('%') => {
                    chars.next();
                    expanded.push('%');
                }
                Some('f' | 'F') => {
                    chars.next();
                    if let Some(p) = paths.first() {
                        expanded.push_str(&p.to_string_lossy());
                    }
                    inserted = true;
                }
                Some('u' | 'U') => {
                    chars.next();
                    if let Some(u) = uris.first() {
                        expanded.push_str(u);
                    }
                    inserted = true;
                }
                Some('i' | 'c' | 'k' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm') => {
                    chars.next();
                }
                _ => expanded.push('%'),
            }
        }
        out.push(expanded);
    }
    if !inserted {
        for p in paths {
            out.push(p.to_string_lossy().into_owned());
        }
    }
    if out.is_empty() {
        return Err("Empty command line".to_owned());
    }
    Ok(out)
}

pub fn terminal_exec_argv(
    terminal_cmd: &str,
    cwd: &Path,
    child_argv: &[String],
) -> Result<Vec<String>, String> {
    if terminal_cmd.contains('\0') {
        return Err("The terminal command contains a null character".to_owned());
    }
    let parsed = glib::shell_parse_argv(terminal_cmd).map_err(|e| e.to_string())?;
    if parsed.is_empty() {
        return Err("The terminal command is empty".to_owned());
    }
    let mut args: Vec<String> = parsed
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let mut has_dir = false;
    let cwd_str = cwd.to_string_lossy();
    for arg in &mut args {
        if arg.contains("{dir}") {
            *arg = arg.replace("{dir}", &cwd_str);
            has_dir = true;
        }
    }
    let bin = Path::new(&args[0])
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();
    let sep_pos = args
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(i, a)| (a == "--" || a == "-e" || a == "-x").then_some(i));
    let insert_at = sep_pos.unwrap_or(args.len());
    if !has_dir {
        match bin.as_str() {
            "kitty" => {
                args.insert(insert_at, cwd_str.into_owned());
                args.insert(insert_at, "--directory".into());
            }
            "konsole" => {
                args.insert(insert_at, cwd_str.into_owned());
                args.insert(insert_at, "--workdir".into());
            }
            "wezterm" => {
                if !args.iter().skip(1).any(|a| a == "start") {
                    args.insert(insert_at, cwd_str.into_owned());
                    args.insert(insert_at, "--cwd".into());
                    args.insert(insert_at, "start".into());
                } else {
                    args.insert(insert_at, cwd_str.into_owned());
                    args.insert(insert_at, "--cwd".into());
                }
            }
            "foot" | "footclient" | "alacritty" | "gnome-terminal" | "ghostty"
            | "xfce4-terminal" | "mate-terminal" | "tilix" | "sakura" => {
                args.insert(insert_at, format!("--working-directory={cwd_str}"));
            }
            _ => {}
        }
    }
    if !child_argv.is_empty() {
        if sep_pos.is_none() {
            match bin.as_str() {
                "foot" | "footclient" | "kitty" | "gnome-terminal" | "wezterm" => {
                    args.push("--".into())
                }
                "xfce4-terminal" => args.push("-x".into()),
                _ => args.push("-e".into()),
            }
        }
        args.extend(child_argv.iter().cloned());
    }
    Ok(args)
}

pub fn spawn_in_terminal(
    terminal_cmd: &str,
    cwd: &Path,
    child_argv: &[String],
) -> Result<(), String> {
    let args = terminal_exec_argv(terminal_cmd, cwd, child_argv)?;
    spawn_detached(&args, Some(cwd))
}

fn spawn_executable(path: &Path) -> Result<(), String> {
    let argv = vec![path.to_string_lossy().into_owned()];
    spawn_detached(&argv, path.parent())
}

pub fn spawn_detached(argv: &[String], cwd: Option<&Path>) -> Result<(), String> {
    if argv.is_empty() {
        return Err("Empty command".to_owned());
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", argv[0]))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn spawn_terminal_window(terminal_cmd: &str, cwd: &Path) -> Result<(), String> {
    spawn_in_terminal(terminal_cmd, cwd, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_desktop_exec_placeholders() {
        let p = PathBuf::from("/tmp/my file.txt");
        assert_eq!(
            expand_desktop_exec("micro %F", &[&p], &["file:///tmp/my%20file.txt"]).unwrap(),
            vec!["micro", "/tmp/my file.txt"]
        );
        assert_eq!(
            expand_desktop_exec("nvim -p %i %c", &[&p], &["file:///tmp/my%20file.txt"]).unwrap(),
            vec!["nvim", "-p", "/tmp/my file.txt"]
        );
        assert_eq!(
            expand_desktop_exec("printf %% %%f %f", &[&p], &["file:///tmp/my%20file.txt"]).unwrap(),
            vec!["printf", "%", "%f", "/tmp/my file.txt"]
        );
        assert_eq!(
            expand_desktop_exec("app --file=%f", &[], &[]).unwrap(),
            vec!["app", "--file="]
        );
    }

    #[test]
    fn builds_terminal_command_lines() {
        let cwd = Path::new("/tmp/a b");
        assert_eq!(
            terminal_exec_argv("footclient", cwd, &["micro".into(), "x y.txt".into()]).unwrap(),
            vec![
                "footclient",
                "--working-directory=/tmp/a b",
                "--",
                "micro",
                "x y.txt"
            ]
        );
        assert_eq!(
            terminal_exec_argv("kitty --directory {dir}", cwd, &[]).unwrap(),
            vec!["kitty", "--directory", "/tmp/a b"]
        );
        assert_eq!(
            terminal_exec_argv("alacritty", cwd, &["htop".into()]).unwrap(),
            vec!["alacritty", "--working-directory=/tmp/a b", "-e", "htop"]
        );
        assert_eq!(
            terminal_exec_argv("xterm", cwd, &["htop".into()]).unwrap(),
            vec!["xterm", "-e", "htop"]
        );
    }
}
