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
| Mod+D | Open the built-in application launcher: type to filter installed apps, Up/Down or Tab to choose, Enter or a click to start, Esc to close. A tap of the configured Mod key alone opens it too in GAVNO mode. Setting `launcher` (or `GAVNO_LAUNCHER`) runs that command instead. |
| Mod+E | Open the file manager from `file_manager` (default `gavno-files`). |

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
| Mod+H/J/K/L | In code mode, focus left/down/up/right. The same aliases work in every Mod+arrow chord below. |

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
| Left/Right/Up/Down, H/J/K/L in code mode | Move the active edge by 10 px. |
| Shift+arrows, Shift+H/J/K/L in code mode | Move it by 50 px. |
| Escape, Return, KP Enter | Leave resize mode. |
| Any other key | Ignored. |
| Any pointer button | Leave resize mode; the click does not reach the window. |

A small HUD shows the current size and the active edge. Resize mode ends if
the window closes, loses focus, or the overview or lock screen opens. It works
on floating windows and on tiles, where it moves the adjacent dividers.

In resize mode, Ctrl+arrows move a floating window by 10 logical pixels;
Ctrl+Shift+arrows move it by 50. Code mode also accepts Ctrl+H/J/K/L and
Ctrl+Shift+H/J/K/L. Windows stay within the output work area; tiles ignore
these movement keys.

## Code mode

Code mode is for keyboard-only window control on the GAVNO shell. Classic never
enters it. The top bar shows a **CODE** chip while it is on; click the chip or
press Mod+Shift+C to toggle it. IPC `code_mode` with `{"enable":true|false}`
sets it and no parameters toggle it. `[keyboard] code_mode = true` starts in
code mode.

While it is on:

- Every workspace on every output tiles. Each workspace's own floating/tiling
  mode is saved when code mode turns on and restored when it turns off.
  Workspaces created during code mode return to the normal default. Explicit
  per-window floating windows, dialogs, transients and fixed-size windows stay
  floating, as usual.
- The pointer hides on the first key press that reaches a client or a bind. It
  returns on the next motion, button, scroll or gesture. Hiding does not change
  pointer focus.
- H, J, K and L act like Left, Down, Up and Right in every Mod+arrow chord
  (Mod, Mod+Ctrl, Mod+Shift, Mod+Alt and the nested Mod+Super swap) and in
  resize mode.
- The dock is hidden, as if autohide were on with no pointer push.
- The top bar shows a compact strip of the active key hints next to the **CODE**
  chip, following code mode, resize mode, window hints or overview. The
  lowest-priority hints disappear first when the bar is narrow. `[keyboard] bar_hints = false` hides
  the strip and keeps the chip.

Turning code mode off restores every previous behaviour, including the cursor
and the dock.

### Alias collisions

The aliases win only while code mode is on:

| Chord | Outside code mode | In code mode |
|---|---|---|
| Mod+L | Lock. | Focus right. |
| Mod+Shift+H | Toggle HDR on the active output. | Move the left splitter/resize left (Shift+Left alias). |
| Ctrl+Alt+L | Lock. | Lock (alias never overrides it). |
| Mod+Alt+L (Super as Mod), Mod+Super+L (Alt as Mod) | Lock, because the old Mod+L bind also accepts the extra modifier. | Swap with the right neighbour. |

With `mod_key = "alt"` (the nested default), Mod+Ctrl+L is Ctrl+Alt+L and
therefore still locks instead of moving right; Mod+Super+L performs the
Mod+Alt+L swap. Ctrl+Alt+L keeps its emergency-lock precedence, including
when Super is also held. The Shift+H collision also applies with the extra
swap modifier held. The other aliases do not collide with existing binds.

## Window hints

Mod+G shows accent-coloured letter badges at the centre of each non-minimized
window on the active output's current workspace, including floating and
maximized windows. Type its letter to focus and raise it. Letters use the
Latin layout even when Russian is active, in this order:
`a s d f g h j k l`, `q w e r t y u i o p`, `z x c v b n m`.
With more than 26 windows, type the displayed two-letter label (or longer for
very large window counts). Escape, another key or a pointer button cancels;
the dismissal key/button does not reach a client.

## Cheat sheet

Mod+Slash or Mod+F1 toggles a centred shortcut overlay in both shells. It groups
the current GAVNO or Classic binds, code-mode aliases and active modal keys.
Escape or any other key closes it and is swallowed. The sheet and bar strip
use the same bind inventory next to the dispatcher in `hints.rs`; tests check
that its chords are handled. The sheet is available from resize mode and overview.

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
| Mod tap | Open the launcher in GAVNO mode, or Start in Classic. Overview is the fallback when the built-in launcher is disabled. |
| Super+Shift+F23 | Toggle the overview. This is what the Copilot key sends on Snapdragon X Elite laptops. |
| Arrows or H/J/K/L | Select the nearest window in that direction, across workspaces. |
| Tab / Shift+Tab | Cycle the selection forward / backward. |
| Enter | Focus the selected window, switching workspace, and close. |
| Delete | Ask the selected window to close. |
| 1-9 | Select that workspace. |
| Escape | Close the overview. |

| Pointer | Action |
|---|---|
| Left click a window | Focus it, switching to its workspace if needed, and close the overview. |
| Drag a window onto a workspace thumbnail | Move the window to that workspace. Moving less than 6 px counts as a click. |
| Left click a workspace thumbnail | Select that workspace. |
| Middle click a window | Ask the window to close. |
| Scroll over a window | Flick it with a spring. A fast upward flick closes the window. |

The accent-coloured selection ring starts on the focused window. Arrows and
Tab preview a selection without changing client focus; Enter commits it and
Escape restores the active workspace view. Navigation works in both shells and uses the Latin layout. Other keys are swallowed while
the overview is open; Mod+Slash and Mod+F1 can show its cheat sheet.

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
| Mod+L, Ctrl+Alt+L | Lock the session. In code mode Mod+L focuses right; Ctrl+Alt+L still locks. |
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

In `--greeter` mode every key except VT switching and hardware media keys goes to the login screen.

## Finger pointer (webcam)

| Chord | Action |
| --- | --- |
| Mod+Shift+M | Turn the webcam finger pointer on or off. |

Works in both shells and in code mode; Mod+M (minimize) is unchanged. The
chord does nothing while the session is locked or when `[pointer] camera` is
`false`. See [finger-pointer.md](finger-pointer.md) for the gestures and the
privacy model.

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

## Hardware media keys

These compositor-owned keys require no modifier and work while locked or when
an application inhibits shortcuts. Held keys use `[keyboard] repeat_delay`
(default 300 ms) and `repeat_rate` (default 30 Hz; 0 disables repeat).

| Keys | Action |
|---|---|
| XF86AudioRaiseVolume / XF86AudioLowerVolume | Increase / decrease the default PipeWire sink by 5%, capped at 100%. |
| XF86AudioMute | Toggle default sink mute. |
| XF86AudioMicMute | Toggle default source mute. |
| XF86AudioPlay / XF86AudioPause | Toggle the active MPRIS player's playback, including while locked. |
| XF86AudioNext / XF86AudioPrev | Next / previous track on the active MPRIS player, including while locked. |
| XF86MonBrightnessUp / XF86MonBrightnessDown | Adjust backlight by 5% through logind, never below 1. Unavailable in the nested backend. |

Audio control uses `wpctl`. A glass OSD shows the level/mute state for about
1.5 seconds after the last change. Quick settings also offers audio and
brightness sliders; Esc or Super closes either quick settings or the calendar.

Built-in notifications are controlled with the pointer: click a card to invoke
its default action and dismiss it, click × to dismiss, or choose an action
button. Hover pauses its timeout. Click the clock for notification history and
**Clear**; quick settings includes **Do not disturb** (critical alerts bypass
it). The clock dot marks unread history. These controls belong to the optional
`Notifications` feature (`[modules] notifications = true` by default).

## Classic mode

`Mod+Shift+W` toggles the shell in either mode. It has no other default binding.
The configured Mod key acts as Win. Its unaccompanied tap opens Start in
Classic. Unlisted chords retain their normal GAVNO action.

Classic leaves code mode and makes every workspace float. Former tiles,
including those forced by code mode, keep their tiling arrangement as floating
windows: the same tree, order and ratios, laid out over the work area above the
taskbar with the configured gap. With server-side decorations the title bar is
part of each tile's rect. Three tiled terminals stay three terminals side by
side. Windows snapped to halves or quarters stay snapped, refitted to the
Classic work area. Focus and stacking order stay intact. Returning restores
each workspace's mode and tiling tree, including split axes and ratios, and puts
snapped windows back into their exact snaps. Tiles that were moved or resized in
Classic rejoin their tiles. Code mode comes back on if it was on before Classic
and stays off otherwise. Closed windows drop out; moved windows and windows
opened in Classic join their current workspace as new tiles. Fullscreen windows
stay fullscreen. Existing floating and minimized windows retain their placement
and state across the switch; maximized windows are refitted to the work area of
the shell they enter.

Normal new windows open maximized by default. Parented dialogs and fixed-size
windows keep client sizing. Classic remembers the last maximized state or
floating position, size and output for up to 128 applications when they close.
The next window of that application uses that state, clamped to an available
output. `[classic] open_maximized = false` lets other clients choose their own
size; remembered states still apply. Mod+T cannot enable tiling in Classic.

| Gesture | GAVNO | Classic |
|---|---|---|
| Super + three-finger swipe up | Enter Classic | Toggle show desktop |
| Three-finger swipe up | Nothing | Toggle show desktop |
| Super + three-finger swipe down | Nothing | Return to GAVNO |
| Super + two-finger swipe | Same routing as three fingers | Same routing as three fingers |

Switching the shell mode needs Super held when the swipe picks its
direction, so a plain swipe never flips the whole desktop by accident.

With the overview enabled, swipe up in GAVNO opens the overview instead, so
the Classic directions flip: swipe down enters Classic and toggles show
desktop, swipe up returns to GAVNO.

An open overview retains its existing vertical gestures. Horizontal gestures
continue switching workspaces. Mode transitions follow the fingers; release
uses the existing distance and velocity projection, and cancellation returns
to the originating mode.

| Keys in Classic | Action |
|---|---|
| Win tap | Toggle Start |
| Win+E | Configured file manager |
| Win+D | Minimize visible windows / restore that same set |
| Win+M | Minimize all visible windows |
| Win+L | Lock |
| Win+I | `classic.settings_command` |
| Win+R | Launcher in command/run style |
| Win+A | Quick settings |
| Win+N | Calendar and notification history |
| Win+Up | Maximize |
| Win+Down | Restore, then minimize |
| Win+Left / Win+Right | Snap left / right (requires Snap) |
| Win+1…9 | Activate taskbar app 1…9 |
| Win+Tab | Overview, or switcher when Overview is disabled |
| Alt+F4 | Close focused window; power menu on an empty desktop |
| Ctrl+Shift+Esc | `classic.task_manager` |

Start opens at the left edge above the taskbar. Taskbar icons default to left
alignment; `classic.align = "center"` centers the app group.

Start supports immediate search, arrows, Tab, Enter and Escape. Click All apps
for the alphabetical list and scroll to browse it. Clicking outside or launching
closes Start. Taskbar left click launches, focuses, minimizes a focused single
window or cycles a group; middle click launches a new instance; right click
opens the window list and New window, Pin/Unpin and Close actions. Hover an app
for 500 ms to see its window title.

### Classic desktop

Desktop input is active only in exposed desktop space; windows, titlebars,
resize bands, shell panels and menus retain input priority. The primary output
holds the desktop icons and widgets.

| Input | Action |
|---|---|
| Click | Select an icon, or clear selection on empty space |
| Ctrl+click | Toggle an icon in the selection |
| Shift+click | Select the range from the last clicked icon |
| Drag empty space | Rubber-band selection; Ctrl preserves the previous selection |
| Drag selected icons | Move the group, snapping to unoccupied grid cells |
| Double-click / Enter | Open the selected file, folder or desktop shortcut |
| Ctrl+A | Select all desktop icons |
| Delete | Move selected files to Trash with `gio trash` |
| F2 | Rename a file inline; Enter commits, Escape cancels |
| Escape | Close the desktop menu or clear selection |
| Right-click an icon | Open, reveal in Files, rename, or move selection to Trash |
| Right-click empty space | Terminal here, Files, New folder, Refresh, Display settings |
| Drag a widget | Move it; its position is saved with icon positions |
| Scroll empty desktop | Browse overflow icon pages |

Virtual Home and Trash shortcuts cannot be renamed or trashed. File operations
run asynchronously; failed renames keep the editable name and show an error.

Playback keys belong to `Media` (`[modules] media = true` by default), are handled
before shortcut inhibitors and the lock gate, and send once per physical press.
The bar's now-playing capsule also toggles playback. Quick settings and the
built-in lock screen offer previous / play-pause / next buttons. Track text on
the lock screen is read-only; these buttons do not change authentication state.

### Wi-Fi, Bluetooth and volume mixer

| Chord | Action |
| --- | --- |
| Mod+Shift+N | Open Wi-Fi controls. |
| Mod+Shift+B | Open Bluetooth controls. |
| Mod+Shift+V | Open the volume mixer. |

These chords work in both shells and in code mode. Within a popover, Up/Down or
j/k selects a row, Left/Right or h/l adjusts a slider by 5%, Space or Enter
activates or toggles, and Escape closes. A Wi-Fi password entry consumes text;
Enter connects and Escape clears it. See [top bar controls](bar.md) for backend
requirements and discovery lifetimes.
