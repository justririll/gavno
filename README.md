# GAVNO

GAVNO stands for "Graphical Application View Navigator and Organizer".

A lightweight hybrid Wayland compositor in Rust on [Smithay](https://github.com/Smithay/smithay):
- Windows-style floating with snap;
- GNOME-style dynamic workspaces and overview;
- optional dwindle tiling;
- touchpad-first input;
- spring animations, with rendering only when something changes.

Primary target: Snapdragon X Elite laptops (aarch64, freedreno).

- `crates/gavno`: the compositor (`gavno --tty` on a VT, `gavno --winit` nested).
- `crates/gavno-agent`: the window management MCP server (`gavno-agent mcp`, stdio).
- `design/`: interactive mockups and specs (input, agent, login).
- `agent/AGENTS.md`: global instructions for AI agents on the laptop.
- `scripts/laptop.sh`: build and run on the laptop over SSH.

The window grabs and the initial structure started from Smithay's `smallvil`
example (MIT, © Smithay contributors).

## Status

What works today:

- Floating windows with rounded corners, shadows, a focus ring and spring
  animations for open, close and geometry changes
  (`crates/gavno/src/decor.rs`, `crates/gavno/src/anim.rs`).
- Windows-style snapping: drag a window to an edge, a corner or the top, or
  snap it from the keyboard. A snap preview shows the target zone
  (`crates/gavno/src/snap.rs`, `crates/gavno/src/overlay.rs`,
  `crates/gavno/src/grabs/move_grab.rs`).
- Dynamic per-output workspaces with a sliding strip; Mod+1..9 selects and
  Mod+Shift+1..9 moves a window (`crates/gavno/src/workspace.rs`).
- Per-workspace dwindle tiling: Mod+T or Mod+Alt+Space toggles a workspace,
  Mod+Space floats one window, splitters resize by keyboard or pointer, and
  dragging a tile onto another reorders the tree
  (`crates/gavno/src/tiling.rs`, `crates/gavno/src/grabs/tiling_grab.rs`).
- Overview with live window content, window titles and workspace thumbnails;
  click to focus, drag a window onto a thumbnail to move it, middle click to
  close (`crates/gavno/src/overview.rs`).
- Built-in dock and top bar: pinned and running apps, icon magnification,
  an app menu, scroll-to-cycle windows, workspace dots, a tiling/floating
  chip, layout switching and a power menu
  (`crates/gavno/src/dock.rs`, `crates/gavno/src/topbar.rs`).
- 3-finger touchpad gestures: horizontal swipes switch workspaces, vertical
  swipes open and close the overview, hold catches an animation
  (`crates/gavno/src/gesture.rs`).
- Window gestures and precision resize: 4-finger drag and throw, 4-finger
  pinch, Super + 2-finger scroll/pinch, resize bands, modal resize mode
  (Mod+R), a top-left hot corner and overview flick
  (`crates/gavno/src/window_gesture.rs`,
  `crates/gavno/src/resize_mode.rs`).
- TOML configuration: mod key, terminal, launcher, gap, output scale and
  mode, HDR, tiling, lock and keyboard layout, with `GAVNO_*` environment
  overrides (`crates/gavno/src/config.rs`, [docs/config.md](docs/config.md)).
- Alt+Tab switcher with a text list in most-recently-used order.
- Protocols: cursor-shape, xdg-activation, xdg-decoration (client-side),
  data-control, primary selection, relative pointer, pointer constraints,
  idle notify with idle inhibit, ext-session-lock, keyboard-shortcuts-inhibit
  and layer-shell (`crates/gavno/src/handlers/`).
- Optional XWayland through `xwayland-satellite` when it is in `PATH`.
- greetd greeter mode: `gavno --greeter`, see `design/login.md`.
- Built-in lock screen with PAM authentication, idle and pre-suspend locking,
  failed-attempt backoff, and gated IPC while locked
  (`crates/gavno/src/lock.rs`, `dist/pam.d/gavno-lock`, [docs/lock.md](docs/lock.md)).
- HDR output on DRM: EDID/BT.2020/PQ setup, SDR-to-HDR composition,
  Mod+Shift+H toggle and runtime SDR white control
  (`crates/gavno/src/hdr/`, `crates/gavno/src/backend/udev.rs`).
- Idle means no redraw.

Keyboard binds: [docs/keybinds.md](docs/keybinds.md)

Configuration: [docs/config.md](docs/config.md)

Lock screen: [docs/lock.md](docs/lock.md)

## Not yet

- Three-finger pinch and rotation gestures.
- Screencopy.
- HDR for clients (`wp-color-management-v1`).

## Window management IPC

GAVNO listens on `$XDG_RUNTIME_DIR/gavno/ipc.sock` and exports `GAVNO_SOCK` to its
children. The socket directory is private (0700). Requests and replies are
one JSON object per line, with a numeric request `id`:

```json
{"id":1,"method":"screen","params":{}}
{"id":2,"method":"focus","params":{"id":3}}
{"id":3,"method":"move","params":{"id":3,"x":100,"y":80,"w":820,"h":560}}
```

Methods are `screen`, `workspace`, `focus`, `close`, `move`, `raise`,
`move_to_workspace`, `togglefloating` and `set_hdr`. While the session is
locked, `screen` returns `{"locked": true}` and every other method fails with
`session locked`. Window ids are stable for each toplevel's lifetime and
never reused during a GAVNO run.
`screen` returns outputs and mapped windows, topmost first; geometry is in
logical pixels. `raise` preserves keyboard focus. Width and height are
optional for `move`; resizing asks the client to configure the new size.
Replies contain either `result` or an `error` string, plus the request id.
Validated action requests, including unknown window ids, are appended to
`~/.local/state/gavno/agent.log`; reads are not logged. Actions fail if the audit
log cannot be written.

Register the MCP server with `claude mcp add gavno-agent -- gavno-agent mcp`.
It connects via `GAVNO_SOCK`, falling back to `$XDG_RUNTIME_DIR/gavno/ipc.sock`,
and exposes the same five tools. Window tool arguments use numeric ids;
compact screen output labels them `w3`, `w4`, etc. If GAVNO is unavailable,
tool calls return an error while the MCP server stays running.
