# GAVNO input design: touchpad first

Target: a Snapdragon X Elite laptop, used mostly without a mouse. Every window
operation must be comfortable with the touchpad alone, and the keyboard is a
shortcut layer on top, not a requirement.

## Principles

1. **Fingers are the API.** Moving, resizing, snapping, minimizing, switching
   workspaces and opening the overview all have a touchpad gesture. No
   operation requires "hold a key + press the pad + drag" (the Hyprland/niri
   `Super+RMB` resize contortion).
2. **2 fingers belong to apps, 3 and 4 fingers belong to GAVNO.** Plain
   two-finger scroll and pinch always reach the client (browser zoom, maps,
   image viewers). GAVNO only takes two-finger input while `Super` is held.
   - 3 fingers act on the desktop: workspaces, overview, show desktop.
   - 4 fingers act on the window under the pointer: move, throw, resize.
3. **1:1 and interruptible.** Every gesture tracks the fingers exactly while
   they are down. On release a spring finishes the motion with the finger
   velocity. A 3-finger hold catches an animation mid-flight (GNOME behaviour).
4. **Only the grabbed edge moves.** Any resize keeps the opposite edge
   anchored. In tiling only the splitter being dragged moves. Neighbours never
   jump. If a client answers with a different size, GAVNO still keeps the anchor.
5. **Big targets.** The pointer never has to hit a 1-2 px border.
6. **Content follows fingers.** Direction follows the natural-scroll setting:
   swipe left and the workspace on the right slides in, like GNOME and macOS.

## Touchpad defaults (libinput)

| Setting | Default | Why |
|---|---|---|
| tap-to-click | on | light taps instead of physical clicks |
| tap-and-drag + drag-lock (timeout) | on | double-tap-hold drags a title bar without pressing the pad |
| click method | clickfinger | 2-finger click = right, 3-finger click = middle, anywhere on the pad |
| 3-finger tap | middle click | close in overview, paste in terminals |
| natural scroll | on | content follows fingers |
| disable-while-typing | on | palm safety |
| three-finger drag (libinput 1.27+) | **off** | conflicts with 3-finger swipes |
| scroll method | two-finger | edge scroll off |

All of these live in `input { touchpad { ... } }` in `config.kdl`.

## Gesture map

### 3 fingers: desktop

| Gesture | Action | Details |
|---|---|---|
| swipe left / right | previous / next workspace | 1:1, rubber band at the ends, velocity decides the target |
| swipe up | open overview | 1:1. Swipe up again inside the overview goes to the app grid |
| swipe down (in overview) | close overview | 1:1 |
| swipe down (on desktop) | App Exposé | only the focused app's windows, spread out. Pick one |
| pinch out (spread) | show desktop | windows slide to the screen edges, 1:1; pinch in brings them back |
| pinch in | app launcher | grid of apps with search focused |
| hold (fingers rest still) | catch | freezes any running workspace or overview animation under the fingers |
| tap | middle click | standard libinput |

### 4 fingers: the window under the pointer

The window under the pointer is grabbed when 4 fingers land, or the focused
window if the pointer is over the desktop.

| Gesture | Floating workspace | Tiling workspace |
|---|---|---|
| drag (any direction) | moves the window 1:1, the same as dragging its title bar. Snap previews appear at edges and corners | moves the tile 1:1. Drop on a tile's centre swaps; drop on its edge splits there (preview shows where) |
| throw (fast release) | the spring carries the window along the release velocity. The landing point picks the zone: side = half, corner = quarter, top = maximize, bottom centre = **minimize into the dock** | throw to a side = move the tile one slot that way |
| pinch out / in | grow / shrink around the anchor edge nearest to the pointer, 1:1 with the pinch scale | grow / shrink the tile's share of its split |

Throw makes the Windows `Win+arrows` snap a single flick of four fingers:
flick right = right half, flick up-right = top-right quarter, flick down =
minimize to the dock.

### 2 fingers + Super: precision

| Gesture | Action |
|---|---|
| `Super` + two-finger scroll over a window | **scroll-resize**: the horizontal component moves the left or right edge, the vertical one moves the top or bottom edge. The edge that moves is the one nearest the pointer (pointer in the right half moves the right edge); the opposite edge stays put. In tiling it moves the nearest splitter |
| `Super` + two-finger pinch over a window | proportional resize around the window centre (floating) / share of split (tiling) |
| `Super` + two-finger scroll over the desktop or overview | switch workspace |

Scroll-resize is the main answer to "resizing is always awkward". It is as
precise as scrolling, needs no button held and works the same in floating
and tiling.

### Other surfaces

| Where | Input | Action |
|---|---|---|
| dock icon | two-finger scroll | cycle that app's windows |
| dock icon | 3-finger tap / middle click | new window of that app |
| top bar | two-finger scroll | switch workspace |
| volume / brightness icon | two-finger scroll | adjust |
| overview window | two-finger flick up | close the window |
| overview window | 3-finger tap | close the window |
| top-left hot corner | push into the corner (pressure barrier, 100 px) | overview. Off-limits to accidental brushes: the pointer must keep pushing |
| bottom edge | push | reveal the dock when auto-hide is on |

## Pointer targets

- **Resize band:** 12 px outside each window edge, 28 px squares at corners,
  plus 4 px inside the edge. The band sits in the gaps, so content stays
  clickable. Cursor shapes come from cursor-shape-v1.
- **Tiling splitters:** the whole gap between two tiles is a splitter. Its hit
  area is at least 12 px even when `gaps` is 0 (it then overlaps 6 px into each
  tile, only within the shared edge). Hover shows a 2 px accent line, and the
  drag moves only that splitter.
- **Title bar drag:** works with tap-and-drag (double-tap-hold), a physical
  press, or 4 fingers anywhere on the window.
- **Snap layouts flyout:** hovering the maximize button shows the layout
  picker, so snapping takes one tap.
- **Double-tap title bar:** maximize / restore.

## Resize mode (keyboard + touchpad, no buttons)

`Super+R` enters resize mode for the focused window:
- **One finger, no click:** pointer motion resizes the window. The nearest
  corner or edge moves and the opposite one stays.
- **Arrow keys:** resize by 10 px; with `Shift` by 50 px.
- **Overlay:** shows the size (`1240 × 760`) and which edge is active.
- **Tab:** cycles the active edge.
- **Exit:** `Enter`, `Esc`, a tap or a click. Leaving the window also exits.

In tiling, resize mode moves the splitters around the focused tile.

## Keyboard

The base is KooL Hyprland-Dots (see the project memory and the compositor
mockup). Additions and changes made for touchpad and laptop use:

| Keys | Action | Note |
|---|---|---|
| `Super` tap | overview | release without other keys |
| `Super+R` | resize mode | new |
| `Super+M` | minimize to the dock | new, the dock exists now |
| `Super+Ctrl+arrows` | move: snap (floating) / move tile (tiling) | KooL `movewindow`, extended with Windows snap |
| `Super+Shift+arrows` | resize ±50 px | KooL |
| `Copilot` key | overview | X Elite laptops send `Shift+Super+F23`; GAVNO maps it, configurable |
| `Fn` brightness / volume / mic / media | system | handled in GAVNO, with an on-screen indicator |
| lid close | suspend unless an external display is connected | |

Nothing important requires both hands except chord shortcuts that already
existed in KooL.

## Arbitration rules

1. Finger count is decided when the gesture begins. A swipe locks its axis
   after 12 units of motion, and diagonal motion counts only for 4-finger
   drags and throws.
2. 3- and 4-finger swipes, pinches and holds are always taken by GAVNO. They are
   never forwarded to clients.
3. Two-finger scroll and pinch are forwarded to the client through
   pointer-gestures-unstable-v1 and `wl_pointer.axis`, unless `Super` is held
   at gesture begin.
4. A gesture that begins over a fullscreen window still works, except 4-finger
   input inside a game that holds a pointer lock: there the client wins.
5. Release logic: `target = position + velocity * 0.18 s`, then snap to the
   nearest step (workspace index, overview open/closed, snap zone). If the
   target is under 30% of the way, cancel and spring back.
6. Gesture input runs on the input thread with no frame queued while idle.
   Frames start only when something moves, so this keeps the idle-is-free rule.

## Config sketch

```kdl
input {
    touchpad {
        tap on
        tap-and-drag on
        drag-lock "timeout"
        click-method "clickfinger"
        natural-scroll on
        disable-while-typing on
        three-finger-drag off
    }
}
gestures {
    swipe fingers=3 horizontal="workspace" up="overview" down="app-expose"
    pinch fingers=3 out="show-desktop" in="launcher"
    hold  fingers=3 action="catch"
    drag  fingers=4 action="move-window" throw=on
    pinch fingers=4 action="resize-window"
    super-scroll action="resize-window"
    sensitivity 1.0
    hot-corner "top-left" action="overview" pressure=100
}
```

Every row in the gesture tables is a remappable action, and actions can also
come from extensions (for example `gavno-arcade` binding a game to a gesture).

## Implementation notes (Smithay)

- libinput events arrive as `InputEvent::GestureSwipeBegin/Update/End`,
  `GesturePinchBegin/Update/End`, `GestureHoldBegin/End`, each with
  `fingers()`. Use them directly; do not emulate gestures from raw touch
  points.
- Forward 2-finger gestures to clients with Smithay's `PointerGestures` support
  (pointer-gestures-unstable-v1) when GAVNO does not claim them.
- Gesture handlers drive the same springs as keyboard actions. A gesture sets
  the spring value directly (`jump`) while fingers are down and hands velocity
  to the spring on release.
- Resize anchoring: store the anchor edge at grab time; on each client commit
  recompute the window position from the anchor and the committed size, never
  from the old top-left corner.
- Per-device config: apply touchpad settings in the libinput device-added
  handler; re-apply on config reload.

## Test checklist

- [ ] 3-finger swipe left/right switches workspaces, follows the fingers, rubber-bands at both ends.
- [ ] Release at 40% with high velocity commits; release at 40% slowly cancels.
- [ ] 3-finger hold during a workspace animation stops it under the fingers.
- [ ] 4-finger drag moves the window under the pointer, not the focused one.
- [ ] 4-finger flick down minimizes into the dock icon of that app.
- [ ] `Super` + 2-finger scroll in the right half of a window moves only the right edge.
- [ ] In tiling, dragging a gap moves only that splitter; the other tiles do not move.
- [ ] Plain 2-finger pinch in a browser zooms the page, GAVNO does nothing.
- [ ] Copilot key opens the overview.
- [ ] No frames are rendered while fingers rest on the pad without moving.
