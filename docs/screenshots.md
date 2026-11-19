# Screenshots and clipboard history

GAVNO captures the screen and remembers recent copies itself. It spawns no helper
programs and runs nothing while neither feature is in use. If the GPU readback ever
fails, Print falls back to `grim`/`slurp`/`wl-copy` when they are installed.

## Screenshots

| Keys | Action |
|---|---|
| Print | Capture the active output immediately. |
| Shift+Print, Mod+Shift+S | Open the selection overlay. |

The overlay captures every output once when it opens and shows that frozen frame, so
no client is asked for frames while it is up. Outside the selection the frame is
dimmed; a crosshair follows the pointer.

- Drag to draw a rectangle; its size in pixels is shown live.
- Hover a window to highlight it and click to capture just that window.
- Space or F toggles the whole output.
- Arrow keys move the selection by 1 px, Shift+arrows by 10 px.
- Enter, or a click inside the selection, captures; Esc (or a right click) cancels.

The capture is full output resolution, regardless of the output scale. The PNG is
encoded and written on a worker thread to `$XDG_PICTURES_DIR/Screenshots` (falling
back to `user-dirs.dirs`, then `~/Pictures/Screenshots`) as
`Screenshot_YYYY-MM-DD_HH-MM-SS.png`, and offered as `image/png` on the clipboard by
the compositor itself (no `wl-copy`). A small toast slides in at the bottom right for
about three seconds, "Copied - click to open folder"; a click opens the folder with
`xdg-open`.

## Clipboard history

Mod+V opens a picker in the launcher's style: a search box, a list with text previews
and image thumbnails, Up/Down (or the wheel) to move, Enter to choose, Delete to forget
the selected entry and Esc to close. Choosing an entry makes the compositor the owner
of the selection with that data and, when a window is focused, sends Ctrl+V to it after
the picker has closed.

What is recorded:

- Text selections (`text/plain;charset=utf-8`, `text/plain`) up to 1 MiB and
  `image/png` selections up to 8 MiB, set by clients through the data device or
  data-control. Each is read once through a pipe on a worker thread, only when the
  selection changes. Text wins when an offer carries both.
- Screenshots taken with the built-in tool.
- The last `history_size` entries (default 50), identical copies deduplicated.

What is not recorded: selections offering `application/x-gavno-secret`, selections whose
`x-kde-passwordManagerHint` value is `secret` (or cannot be read), anything while the
session is locked, and anything larger than the caps. History lives in memory only and
is never written to disk.

```toml
[clipboard]
history = true      # false clears the history and frees Mod+V
history_size = 50   # 1-500
```
