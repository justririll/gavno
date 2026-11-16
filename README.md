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
  an app menu and scroll-to-cycle windows. The 32 px glass bar has workspace
  pills, an icon tiling toggle, the focused app icon/title and a centered clock.
  Click the clock for a Monday-first calendar; click the status capsule for
  quick settings with volume/brightness sliders, NetworkManager status,
  battery and session power controls. Scroll workspace pills to switch
  workspaces, scroll the volume icon to adjust audio, and click the layout
  code to cycle keyboard layouts. Popovers close on an outside click, Esc or
  Super. Hardware media keys work while locked and show a spring-animated OSD.
  Status I/O runs off the main loop; periodic refreshes stop while the bar is
  hidden (`crates/gavno/src/topbar.rs`, `crates/gavno/src/bar/`).
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
- Built-in session D-Bus notifications, popup actions, calendar history and Do not disturb.
- Low battery notifications at 15% and 5% while discharging.
- Screen sharing (browsers, OBS) through `xdg-desktop-portal-wlr` on top of
  wlr-screencopy. GAVNO exports its display variables to D-Bus activation; install
  the portal preference and the xdpw config (output chosen with `slurp`):

  ```sh
  install -Dm644 dist/xdg-desktop-portal/gavno-portals.conf /usr/share/xdg-desktop-portal/gavno-portals.conf
  install -Dm644 dist/xdg-desktop-portal-wlr/config ~/.config/xdg-desktop-portal-wlr/config
  ```
- Idle means no redraw.

Keyboard binds: [docs/keybinds.md](docs/keybinds.md)

## Self-test

Inside a running GAVNO session, `scripts/selftest.sh` checks the compositor,
IPC, X11, notifications, the polkit agent, screen capture and the ScreenCast
portal, the foot server and resource use, then asks a few yes/no questions
about what is on screen (`--auto` skips them). The report is written to
`~/.local/state/gavno/selftest-*.txt`.

## Configuration

Configuration: [docs/config.md](docs/config.md)

The startup config supports `[modules]` boolean flags `dock`, `bar`, `overview`,
`tiling`, `snap`, `switcher`, `gestures`, `window_gestures`, `resize_mode`,
`animations`, `decorations`, `ipc`, `screencopy`, `launcher`, and `notifications`,
all `true` by default.
For example, `[modules]` followed by `dock = false` disables the dock. The legacy
`dock.enabled` and `bar.enabled` switches also apply; either flag can disable the
feature. Feature panics disable and reset only that feature until restart.
Lock and Greeter stay core and fail closed. See [docs/modules.md](docs/modules.md).


Lock screen: [docs/lock.md](docs/lock.md)

## Not yet

- Three-finger pinch and rotation gestures.

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

### Classic desktop shell

Swipe down with three fingers, or press `Mod+Shift+W`, to enter Classic: a
Windows-style taskbar and Start menu around the same windows and workspaces.
Swipe up to return. Classic includes Windows shortcuts, bottom-anchored quick
settings and calendar, and a show-desktop toggle. Configure startup mode, pins,
alignment and helper commands in [`[classic]`](docs/config.md#classic-shell);
see the [gesture and shortcut tables](docs/keybinds.md#classic-mode). Desktop
icons, selection, safe trash/rename actions and draggable clock/system widgets
use the primary output. File updates use inotify, and widget refreshes stop
when the desktop is covered or locked. Taskbar apps default to left alignment.

## License

Copyright (C) 2026 kirill and the GAVNO contributors.

GAVNO is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version. It is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See [COPYING](COPYING) for the full text.

The top bar's media title opens a built-in Spotify panel when `[media] music_panel = true`
(the default). It needs the separately installed, D-Bus activated `org.gavno.Music` daemon;
a music icon appears when that service is available and no MPRIS player is active.
The panel offers Home, Library and Playlists (`Ctrl+1`–`Ctrl+3`), type-to-search,
track controls, seek and volume. Use Up/Down and Enter to select a row, Alt+Left or
Backspace with an empty search to go back, and Escape or an outside click to close.
Login opens the daemon's browser OAuth flow. Errors stay inside the panel.
Closing releases the panel worker, timers, pages and cover images; the existing
MPRIS bar and lock-screen controls continue to work independently.
