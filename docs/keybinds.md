# Keyboard binds

These are the compositor's key bindings, implemented in
`crates/gavno/src/input.rs`, plus the pointer and touchpad behaviour that goes
with them. "Mod" is the configurable mod key: Super by default on the TTY
backend, Alt when nested (`--winit`, because the host compositor keeps Super
for itself). Set `mod_key` in [the config file](config.md) to change the
default; `GAVNO_MOD=super` or `GAVNO_MOD=alt` overrides both the default and
the file.

Binds match the Latin layout, so they also work while the Russian layout is
active.

## Apps

| Keys | Action |
|---|---|
| Mod+Return, Mod+KP Enter | Start the terminal from `terminal` (default `foot`; `GAVNO_TERMINAL` overrides). |
| Mod+D | Start the launcher from `launcher`. Unset by default, so the key goes to the client; `GAVNO_LAUNCHER` overrides. |

## Windows

| Keys | Action |
|---|---|
| Mod+Q | Close the focused window. |
| Mod+M | Minimize the focused window. |
| Mod+Ctrl+F | Toggle maximize. |
| Mod+Shift+F | Toggle fullscreen. |

## Snap

| Keys | Action |
|---|---|
| Mod+Ctrl+Left | Snap to the left half. If already snapped left, move to the previous output. |
| Mod+Ctrl+Right | Snap to the right half. If already snapped right, move to the next output. |
| Mod+Ctrl+Up | From a side half, snap to the top quarter on that side; otherwise maximize. |
| Mod+Ctrl+Down | Step down: maximized goes to floating, a top quarter to the side half, a side half to the bottom quarter. |

Left and Right also move between quarters of a row (for example TopRight +
Ctrl+Left gives TopLeft).

In a tiling workspace these keys move the focused tile instead of snapping.
See [Tiling](#tiling).

## Tiling

Tiling is per workspace. Mod+T or Mod+Alt+Space toggles the active workspace
between floating and tiling; with Alt as Mod in nested mode, the second chord
is Alt+Super+Space. The mode chip next to the workspace dots in the top bar
does the same.

| Keys | Action |
|---|---|
| Mod+T, Mod+Alt+Space | Toggle floating/tiling for the active workspace. |
| Mod+Space | Toggle the focused window's floating override in a tiling workspace. |
| Mod+Shift+I | Rotate the focused tile's split; a lone tile flips the axis for its next insertion. |
| Mod+arrows | Focus the neighbouring window. |
| Mod+Ctrl+arrows | Move the focused tile toward that side, swapping with the neighbour. |
| Mod+Shift+arrows | Move the splitter on that side by 40 logical pixels. |

| Pointer | Action |
|---|---|
| Drag a gap between tiles | Resize only that divider. The hit target is at least 12 px wide. |
| Mod+RMB on a tile | Move the nearest dividers under the pointer. |
| Drag a tile onto another tile | Drop on an outer quarter to insert on that side; drop in the middle to swap. |
| Drag a tile by its title bar | Same drop targets; the preview shows the resulting slot. |
| 4-finger throw toward a neighbour | Swap with the neighbour in that direction. |

New tiles split the focused tile along its longer side (or the flipped axis
after Mod+Shift+I), with 50/50 ratios. Outer and inner gaps both use `gap`.
Ratios stop before any adjacent tile falls below 120 px. Maximized, fullscreen,
and explicitly floating windows stay above tiles. Minimized windows leave the
tree and rejoin when restored. New workspaces float unless
`[tiling] default = true`.

Toggling into tiling turns snapped windows into floating windows. Toggling
back restores the floating geometry. Snapping is disabled in a tiling
workspace: Mod+Ctrl+arrows move the tile instead. IPC `togglefloating` accepts
a window `id`.

## Focus and swap

| Keys | Action |
|---|---|
| Mod+Left, Mod+Right, Mod+Up, Mod+Down | Focus the nearest window in that direction. |
| Mod+Alt+Left/Right/Up/Down | Swap with the nearest window in that direction, keeping both geometry slots. |
| Mod+Super+Left/Right/Up/Down | Same swap when Mod is Alt (nested default), because Alt is then taken by Mod. |

## Resize

| Keys | Action |
|---|---|
| Mod+Shift+Left, Mod+Shift+Up | Shrink the focused floating window by 40 px per press. |
| Mod+Shift+Right, Mod+Shift+Down | Grow the focused floating window by 40 px per press. |

Resize only applies to floating windows; snapped, maximized and fullscreen
windows ignore it. In a tiling workspace the same keys move the splitter on
that side by 40 px instead.

## Window gestures and precision resize

Window gestures need a touchpad. The 4-finger gestures act on the window
under the pointer, or on the focused window when the pointer is over empty
desktop.

| Gesture | Action |
|---|---|
| 4-finger swipe | Drag the window. Release to place it, snap it, minimize it or throw it. |
| 4-finger pinch | Resize both dimensions, anchored at the side nearest the pointer; in a tiling workspace, move the adjacent dividers. |
| Super + 2-finger pinch | Resize proportionally around the window centre; on a tile, move the adjacent dividers. Without Super, 2-finger gestures go to the client. |
| Super + 2-finger scroll | Resize from the edges nearest the pointer: horizontal changes width, vertical changes height. On a tile it moves the adjacent dividers; over empty desktop it steps workspaces. |

For a non-tiled window, a 4-finger swipe release uses these targets:

| Release at | Result |
|---|---|
| Top edge | Maximize. |
| Top or bottom corner, within 110 px of a side | Snap to that quarter. |
| Left or right edge | Snap to that half. |
| Bottom edge, away from corners | Minimize. |
| Anywhere else | Drop at the window's current position plus velocity × 0.18 s, keeping 48 px on screen. |

A fast throw is projected before clamping, so a quick throw past the panel
still reaches its edge. A cancelled gesture restores the original geometry and
mode.

Without a gesture, resize a non-tiled window by dragging its border. Bands
extend 12 px outside and 4 px into the window; corners are 28 px squares.
Fullscreen windows and tiles have no bands, and topmost content blocks the
bands of windows underneath. Client minimum and maximum sizes are respected,
and the opposite edge stays fixed. Mod+RMB on a floating window resizes from
the edges nearest the pointer; on a tile it moves the nearest dividers.

Push the pointer into the top-left corner of an output; after 100 logical px
of resisted motion the overview opens. Push the bottom edge to reveal an
autohide dock.

## Resize mode

Mod+R enters resize mode for the focused window.

| Keys | Action |
|---|---|
| Pointer motion | Move the active edge or edges. |
| Tab, Shift+Tab | Cycle through the eight edges. |
| Left/Right/Up/Down | Move the active edge by 10 px. |
| Shift+arrows | Move it by 50 px. |
| Escape, Return, KP Enter | Leave resize mode. |
| Any other key | Ignored. |
| Any pointer button | Leave resize mode; the click does not reach the window. |

A small HUD shows the current size and the active edge. Resize mode ends if
the window closes, loses focus, or the overview or lock screen opens. It works
on floating windows and on tiles, where it moves the adjacent dividers.

## Alt+Tab switcher

| Keys | Action |
|---|---|
| Alt+Tab | Open the switcher and cycle forward. |
| Alt+Shift+Tab | Open the switcher and cycle backward. |
| Tab, Shift+Tab | Cycle forward or backward while the switcher is open. |
| Escape | Close the switcher without switching. |
| Release Alt | Focus the selected window. |

The switcher is a text list of windows in most-recently-used order.

## Workspaces

Each output has its own workspace list. Shortcuts count workspaces from 1, IPC
counts them from 0.

| Keys | Action |
|---|---|
| Mod+1 ... Mod+9 | Select workspace 1-9 on the active output. |
| Mod+Shift+1 ... Mod+Shift+9 | Move the focused window to workspace 1-9. |
| Mod+Tab, Mod+Ctrl+Page Down | Select the next workspace. |
| Mod+Shift+Tab, Mod+Ctrl+Page Up | Select the previous workspace. |

The number keys select an existing workspace, including the trailing empty one.
Selecting or moving to an index that does not exist does nothing.

The list is dynamic: using the trailing empty workspace appends another empty
one, and empty inactive workspaces are removed once the switch settles. Windows
on inactive workspaces keep their geometry, mode and place in their workspace's
focus order; they receive no input and are only drawn as read-only content in
the overview and in the switching strip.

With `mod_key = "alt"`, Mod+Tab is the workspace bind, so Alt+Tab does not open
the switcher.

## Overview

The overview zooms out the active output: all workspaces are shown side by side,
windows keep live content and their titles, and a strip of workspace thumbnails
sits at the top.

| Keys | Action |
|---|---|
| Mod+A | Toggle the overview. |
| Super tap | Toggle the overview: press and release Super with no other key held and no click in between. |
| Super+Shift+F23 | Toggle the overview. This is what the Copilot key sends on Snapdragon X Elite laptops. |
| Escape | Close the overview. |

| Pointer | Action |
|---|---|
| Left click a window | Focus it, switching to its workspace if needed, and close the overview. |
| Drag a window onto a workspace thumbnail | Move the window to that workspace. Moving less than 6 px counts as a click. |
| Left click a workspace thumbnail | Select that workspace. |
| Middle click a window | Ask the window to close. |
| Scroll over a window | Flick it with a spring. A fast upward flick closes the window. |

Keys that are not compositor binds are swallowed while the overview is open.

## Dock and top bar

The dock and top bar hide while a fullscreen window is on the output. When the
dock is set to autohide, push the pointer against the bottom edge to reveal
it.

| Pointer | Action |
|---|---|
| Left click an icon | Focus the most recently used window of that app; if it is already focused, minimize it. Launch the app when it has no windows. |
| Middle click an icon | Launch a new window. |
| Right click an icon | Open the app menu: New window, Quit (closes all its windows), Pin or Unpin. Pin changes last until GAVNO exits. |
| Scroll over an icon | Cycle through that app's windows, 30 logical px per step. |
| Scroll over the top bar | Switch to the previous or next workspace, 30 logical px per step. |

The top bar shows the tiling/floating chip, workspace dots, the keyboard
layout, the clock and battery, and a power menu. Clicking the tiling chip
toggles the workspace mode; clicking the layout chip cycles the keyboard
layout. The power menu has Lock, Log out, Suspend, Restart and Shut down.

## Touchpad gestures

| Gesture | Action |
|---|---|
| 3-finger swipe left/right | Drive the workspace strip. |
| 3-finger swipe up/down | Open or close the overview. |
| 3-finger hold | Catch a running workspace switch or overview animation; releasing settles to the nearest state. |
| 4-finger swipe | Drag and throw a window; see [Window gestures and precision resize](#window-gestures-and-precision-resize). |
| 4-finger pinch | Resize a window. |
| 2-finger swipe, pinch or hold | Forwarded to the client under the pointer through pointer-gestures. |
| 2-finger swipe with Super held at the start | Behaves like a 3-finger swipe: horizontal drives the workspace strip, vertical the overview. |
| 2-finger pinch or scroll with Super held | Resize a window; see [Window gestures and precision resize](#window-gestures-and-precision-resize). |

A compositor-handled swipe locks to its dominant axis after 12 logical input
units. Horizontal movement rubber-bands past the first and last workspace. On
release the finger velocity is projected 0.18 seconds ahead and the nearest
workspace or overview state is selected; a cancelled swipe returns to where it
started. A 3-finger hold only catches an animation that is already running.

Three-finger pinches and rotation gestures are not handled. Four-finger swipes
and pinches go to the client while a client holds an active pointer lock.
Gestures are off in `--greeter` mode and while the session is locked.

`GAVNO_GESTURE_DISTANCE` sets the logical input units per workspace (default
400; values below 24 are ignored). The overview uses 340 units for its full
travel. `GAVNO_NATURAL_SCROLL=0` reverses the compositor-handled gesture
direction; it does not change scrolling delivered to clients.

## Lock

| Keys | Action |
|---|---|
| Mod+L, Ctrl+Alt+L | Lock the session. |
| Ctrl+Alt+F1 ... F12 | Switch to virtual terminal 1-12 while locked (TTY backend only). |

The top bar power menu also has Lock. GAVNO locks after
`[lock] after_idle_secs` seconds of inactivity (default 300; `0` disables
automatic locking) and before suspend through logind. Idle inhibitors postpone
the idle lock.

While the session is locked, GAVNO ignores other binds and window gestures and
routes input to the lock screen. Desktop clients receive no keyboard or
pointer input.
The IPC socket only answers `screen`, with `{"locked": true}`; every other
request fails with `session locked`. Failed password attempts block input for
1, 2, 4, 8 and 16 seconds, then 30 seconds between tries.

See [the lock screen](lock.md).

## Session / VT

| Keys | Action |
|---|---|
| Ctrl+Alt+Delete | Quit GAVNO. Works while a client inhibits keyboard shortcuts, but not while the session is locked. |
| Ctrl+Alt+F1 ... F12 | Switch to virtual terminal 1-12 (TTY backend only). Works while locked. |

In `--greeter` mode every key except VT switching goes to the login screen.

## HDR output (DRM backend)

- Mod+Shift+H: toggle HDR on the active output, when EDID and DRM support it.
- Mod+Shift+F2 / Mod+Shift+F3: decrease / increase SDR white by 10 nits while
  HDR is enabled (80–400 nits, default 203). They do nothing while HDR is off.

See [configuration](config.md) for mode selection, HDR requirements, and IPC.

## Screenshots

Screenshots use `grim`, `slurp` and `wl-copy` (wl-clipboard) through the
wlr-screencopy protocol. Files go to `$XDG_PICTURES_DIR/Screenshots` (default
`~/Pictures/Screenshots`) and are copied to the clipboard.

| Keys | Action |
|---|---|
| Print | Capture the active output. |
| Shift+Print, Mod+Shift+S | Select an area with slurp and capture it. |
