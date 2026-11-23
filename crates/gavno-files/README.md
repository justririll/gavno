# gavno-files

A native GTK4/libadwaita file manager for the GAVNO desktop. Slate surfaces,
apricot accents, indigo folders, 2x cached vector icons, pure-Rust directory
scanning, Freedesktop Trash, tabs, docked VTE terminal, and pkexec root mode.

Run `gavno-files [PATH|URI]`. A file argument opens its parent folder with that
file selected. Both local paths/`file://` URIs and `trash:///` are supported.

## Build & Test

On the laptop target (`aarch64`, GTK >= 4.18, libadwaita >= 1.6, vte4):

```sh
cargo test -j 8 -p gavno-files
cargo build --release -j 8 -p gavno-files
```

## Architecture Highlights

- **Pure-Rust directory scanning & sorting (`model.rs`):** Enumerates folders
  via `std::fs::read_dir` + `symlink_metadata` without reading file contents
  during listing. Formats sizes, local timestamps, owners, and MIME labels once
  per scan, and sorts in Rust with folders always first.
- **Cached HiDPI icons & LIFO thumbnails (`icons.rs`, `thumbs.rs`):** Icons
  rasterize once into 2x `gdk::Texture`s cached by `(Icon, size)`. Image
  thumbnails decode on a 2-worker LIFO stack with immediate generation-based
  cancellation on scroll/navigation.
- **Virtualized single-bound views (`views.rs`):** Only the active view (`GridView`
  or `ColumnView`) binds to `MultiSelection` over `BoxedAnyObject` entries, with
  rubberband selection, drag-and-drop, middle-click tabs, and a non-intrusive
  rename popover.
- **Zero GVfs / portal requirements (`trash.rs`, `open.rs`):** Implements the
  Freedesktop Trash specification directly in `$XDG_DATA_HOME/Trash`, launches
  `Terminal=true` desktop apps inside the user's configured terminal, and embeds
  a native GTK4 "Open With…" chooser.
- **Root mode (`root.rs`, `polkit.rs`, `gavno-files-helper`):** Async D-Bus
  in-process polkit agent + `pkexec` line-protocol helper with 5-minute idle
  auto-exit.
