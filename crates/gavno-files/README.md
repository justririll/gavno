# gavno-files v1

An English-language GTK4/libadwaita file manager for GAVNO. The interface follows
`design/files-mockup.html`: slate surfaces, apricot selections, indigo folders,
a compact navigation header, a places sidebar, and a status footer. It starts
with the dark appearance.

Run `gavno-files [PATH|URI]`. A file argument selects the file in its containing
folder. Local file URIs and `trash:///` are supported.

## Build

System dependencies: GTK >= 4.18, libadwaita >= 1.6, GIO/GLib, GdkPixbuf, their
headers/pkg-config files, and the desired image loaders (including SVG/WebP).
The target systems' newer GTK/adwaita versions are supported without enabling
APIs beyond the requested feature levels. Trash browsing/restoring requires
the GIO GVfs trash backend; removable-drive handling requires the corresponding
GIO volume monitor/backend. Missing backends produce an error toast.

Always put Cargo downloads and outputs on the HDD:

```sh
export CARGO_HOME=/home/kirill/HDD/ds-build/cargo-home
export CARGO_TARGET_DIR=/home/kirill/HDD/ds-build/target-files
cargo build -j 6 -p gavno-files
cargo test -j 6 -p gavno-files
cargo build -j 6 -p gavno
```

Direct dependencies resolved in `Cargo.lock`:

| Dependency | Version | Features / purpose |
| --- | --- | --- |
| gtk4 (`gtk`) | 0.10.3 | `v4_18`, grid/list widgets and clipboard/DnD |
| libadwaita (`adw`) | 0.8.1 | `v1_6`, application, dialogs and toasts |
| gio | 0.21.5 | Files, asynchronous I/O, volumes and D-Bus |
| glib | 0.21.5 | Main contexts, futures and GObjects |
| gdk-pixbuf | 0.21.5 | Worker-thread image decoding |
| serde | 1.0.229 | `derive`, configuration |
| toml | 0.9.12+spec-1.1.0 | Configuration parsing/writing |
| md5 | 0.8.1 | Freedesktop thumbnail filenames |
| lru | 0.16.4 | In-memory texture eviction |
| futures-channel | 0.3.34 | Async callback/worker result delivery |

## Interaction

- Back/forward/up buttons; Alt+Left/Right/Up; Backspace goes back.
- Breadcrumb buttons; Ctrl+L edits the location. Tab completes folder names and
  presents up to 40 candidates. Relative paths, `~`, and file URIs work.
- Ctrl+1/Ctrl+2 select grid/list. List column headers sort Name, Size, Modified,
  and Type. Natural sorting puts `file2` before `file10`; folders remain first
  in both sort directions.
- Ctrl+H toggles hidden files. Grid/list and hidden preferences persist.
- Double-click/Enter opens items. Open With uses GTK's application chooser via
  `FileLauncher`, including desktop portals when available.
- Ctrl+C/X/V exchange both `x-special/gnome-copied-files` and `text/uri-list`.
  Conflicts offer Replace, Skip, and Keep Both (`name (copy).ext`, then
  `name (copy 2).ext`). A successful cut paste clears the clipboard only if it
  still contains that same cut operation.
- URI-list drag-and-drop works on folders and empty space; Shift moves and Ctrl
  copies. Drop completion is reported after the operation finishes.
- Delete moves items to Trash and offers a 12-second Undo toast. Shift+Delete
  confirms permanent recursive deletion. Trash offers Restore, Delete
  Permanently, and confirmed Empty Trash.
- F2 starts inline editing. Ctrl+Shift+N opens New Folder.
- Space previews images, the first 64 KiB of text, or metadata. Space/Esc closes
  quick look. Properties includes asynchronous recursive folder sizing.
- Typing filters the current directory. Ctrl+F searches subfolders, with
  cancellation when the query/location changes and a 10,000-result limit.
  Symlink directories are not traversed.
- Right-click opens a context menu. Shift+F4 opens an external terminal in the
  selected folder, or the current folder. The header operations popover shows
  per-file progress and cancellation for transfers/deletion/restoring.
- Sidebar entries use XDG user directories, GTK3 bookmarks, and GIO mounted
  volumes/unmounted mountable volumes. Drives can be mounted and ejected.

Directory enumeration is asynchronous in 128-entry batches. Filtering and
sorting are incremental. GIO synchronous operations such as `move_`,
`make_directory`, and `set_display_name` run on workers. Each directory monitor
owns a separate GLib context/thread; change notifications are forwarded to GTK.
Copies traverse directories without following symlinks. Cross-filesystem moves
fall back to traversal if GIO cannot move a whole directory directly; original
folders are removed only when empty. Cancelling a transfer leaves completed
items and any partial destination file in place.

## Configuration and terminal commands

`$XDG_CONFIG_HOME/gavno-files/config.toml` (normally
`~/.config/gavno-files/config.toml`):

```toml
view = "grid" # or "list"
show_hidden = false
terminal = "foot"
```

The terminal command comes from `$TERMINAL`, then `terminal`, then `foot`.
Commands are shell-word parsed and launched directly, without a shell. The
application appends one `--working-directory=<directory>` argument, as foot
expects. For a different terminal, include a `{dir}` argument placeholder:

```toml
terminal = "kitty --directory {dir}"
# terminal = "alacritty --working-directory {dir}"
```

Each placeholder is replaced inside its existing argument, so paths containing
spaces remain one argument. A wrapper script accepting foot's flag also works.

## Thumbnails

One worker decodes PNG/JPEG/WebP/GIF/SVG at a bounded size. GTK textures use a
256-entry LRU. Disk entries use the [freedesktop thumbnail specification](https://specifications.freedesktop.org/thumbnail/latest/):
`$XDG_CACHE_HOME/thumbnails/{normal,large}/<MD5-of-URI>.png` (128/256 px),
`Thumb::URI` and `Thumb::MTime` PNG metadata, private files/directories, and atomic
replacement. Valid thumbnails made by other applications are reused. The cache
itself is excluded from thumbnail generation.

## Desktop installation

The repository supplies a desktop entry and D-Bus activation files. For a user
installation, after building:

```sh
install -Dm755 "$CARGO_TARGET_DIR/debug/gavno-files" "$HOME/.local/bin/gavno-files"
install -Dm644 dist/applications/org.gavno.Files.desktop "$HOME/.local/share/applications/org.gavno.Files.desktop"
install -Dm644 dist/dbus-1/services/org.gavno.Files.service "$HOME/.local/share/dbus-1/services/org.gavno.Files.service"
install -Dm644 dist/dbus-1/services/org.freedesktop.FileManager1.service "$HOME/.local/share/dbus-1/services/org.freedesktop.FileManager1.service"
update-desktop-database "$HOME/.local/share/applications"
```

Make sure `~/.local/bin` is on the session D-Bus activation environment's PATH,
otherwise use the binary's absolute path in the service files' `Exec` fields.
The desktop entry registers `MimeType=inode/directory;`. To choose it explicitly:
`xdg-mime default org.gavno.Files.desktop inode/directory`.

The application exports `/org/freedesktop/FileManager1` with `ShowFolders`,
`ShowItems` (groups items by parent, selects and reveals them), and
`ShowItemProperties`. It requests `org.freedesktop.FileManager1` without taking
ownership away from another running file manager. A name conflict produces a
toast; stop the other file manager to use this implementation. The standard
interface is documented in the [File Manager D-Bus interface specification](https://www.freedesktop.org/wiki/Specifications/file-manager-interface/).

## Validation and known limits

The default test suite needs no display. It covers natural sort, conflict names,
thumbnail URI hashes, both clipboard formats, breadcrumbs, sizes, and config.
An additional ignored integration test requires a display and a writable
isolated configuration/cache. It exercises a 20,000-file progressive listing,
main-loop heartbeat, both views/sort directions, clipboard, rename, live
monitoring, hidden files, recursive search, recursive copy/move, conflict
resolution, and cancellation. Run it with:

```sh
cargo test -j 6 -p gavno-files gtk_large_folder_and_actions -- --ignored --test-threads=1 --nocapture
```

Use isolated `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, and `TMPDIR` for that test.
`GAVNO_FILES_SCREENSHOT=/path/window.png` optionally saves its rendered window.

- Progress is per file plus an item counter, without an ETA or precomputed
  aggregate byte total. Folder copies use the destination's default directory
  permissions; advanced ACL/xattr editing is not provided.
- Quick look uses the bounded thumbnail for images; GIF previews are static.
- External application chooser/launching, physical removable-drive eject, and
  cross-application DnD require the corresponding desktop services/devices.
- Bookmarks are read at window creation; external bookmark changes appear in a
  new window. Recursive search results refresh when the query is rerun.
- Trash restore/undo depends on the GVfs backend and original parent directories
  still existing. A restored item's name conflict uses the normal conflict dialog.

TODO after v1: docked VTE terminal (F4; GTK4 VTE unavailable), polkit root mode
(`gavno-files-helper`), tabs, and network locations.
