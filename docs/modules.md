# Feature isolation

The core owns Wayland clients, input delivery, mapped windows, output lifetimes,
workspace ownership, and both session-lock systems. Shared geometry helpers and
basic surface rendering remain core even when their implementation lives beside
a feature. Lock and Greeter are always present security boundaries.

The optional features are Dock, Bar, Overview, Tiling, Snap, Switcher,
Gestures, WindowGestures, ResizeMode, Animations, Decorations, Ipc, Screencopy, Launcher and Notifications.
`module.rs` stores their startup state. A feature is enabled, disabled by config,
or crashed. A crash disables it until the compositor restarts. The registry has
no timer, animation, or periodic redraw source.

## Calling a feature

Use `gv.guard(Feature::Dock, default, |gv| ...)` when the operation needs mutable
compositor state. Entry methods guard themselves, so callers from another feature
get the callee's boundary too. Nested failures belong to the innermost guard.
`Features::call` supports disjoint field borrows, especially rendering. Immutable
hit tests use `Features::read`, the shared-borrow equivalent; it queues recovery
for the next mutable boundary or idle callback. Simple state decisions may check
`features.on(id)` first. An off feature must not reserve space, consume bindings,
render, or request animation frames.

Each boundary catches Rust unwind panics, records the panic message, logs the
feature once, and returns its default. Choose a default that lets core processing
continue: `false` for an unhandled input event, `None` for a hit or transform,
and an empty vector for optional UI. Decoration failure uses core surface
rendering, including scene transforms, so windows stay visible.

To add a feature, add its identifier and startup flag to `Feature::ALL`,
`ModulesConfig`, and `Features::new`. Add its reset and default cleanup to the
registry, guard every entry and cross-feature dependency, filter its bindings,
and add headless crash-injection and disabled-config tests. Check idle behavior
and document its config key. Do not put feature-owned timers in the core.

## Reset contract

Recovery runs after a mutable guard catches a panic and at loop idle for
split/shared borrows. Resets are individually caught; default cleanup is also
caught. They are idempotent because an outer feature call may need recovery again
after an inner call failed. Recovery requests one coalesced redraw.

- Tiling clears trees and splitters and leaves windows floating at their current
  displayed geometry, clamped to an output. Workspace ownership survives.
- Dock restores minimized windows on their owning workspaces and releases its
  reserve. Bar removes its reserve and cancels its timers.
- Overview closes and settles desktop transforms. Switcher clears its list and
  previews. Snap has no persistent state.
- Gesture recovery cancels streams and window manipulation, settles workspace
  motion, clears previews and balances delivered pointer buttons. Seat cleanup
  uses a one-shot idle callback: Smithay holds its pointer mutex inside grab
  callbacks, so trying to cancel that grab synchronously would deadlock.
- ResizeMode clears its session and restores the cursor. Animations finish at
  final geometry and discard closing snapshots. Decorations discard their cache.
- IPC removes its listener and socket and shuts down connected streams, giving
  clients EOF. Screencopy fails pending requests; later requests fail too.

- Launcher drops its panel. Notifications drop cards and history and remove the
  expiry timer. Their D-Bus worker remains alive, returning unique IDs and
  handling protocol calls, but no notification UI runs until restart. Bar reset
  preserves the shared notification state, so the features fail independently.

## Startup configuration

All keys default to `true`; omitted keys retain that default. Unknown keys warn.
There is no live reload.

```toml
[modules]
dock = true
bar = true
overview = true
tiling = true
snap = true
switcher = true
gestures = true
window_gestures = true
resize_mode = true
animations = true
decorations = true
ipc = true
screencopy = true
launcher = true
notifications = true
```

Legacy `[dock] enabled = false` and `[bar] enabled = false` still disable their
features: either switch can turn them off. IPC and Screencopy are initialized only
when enabled and are not advertised in greeter mode. Workspace switching and
basic floating move/resize remain available without tiling or window gestures.

## Lock and Greeter fail closed

Their key, pointer, rendering and animation UI operations also catch panics. A
failed lock UI invalidates pending authentication replies and is recreated behind
the existing lock gate. Repeated UI/rendering failures retain a solid dark
background. Greeter recovery clears secrets and invalidates old greetd replies.
A UI failure never clears the lock gate or forwards locked input to a client.
The ext-session-lock protocol remains core as well.

The installed panic hook preserves Rust's default output and appends timestamp,
location, message and a forced backtrace to `$XDG_STATE_HOME/gavno/crash.log`, or
`~/.local/state/gavno/crash.log`. Its directory is private (0700), and logs larger
than about 1 MiB are truncated before the next entry. Unit tests exercise the
writer directly without replacing the process-wide hook.

## Notification worker

`notify/dbus.rs` owns a blocking zbus connection with its internal executor.
It requests the session name with `DoNotQueue` only, without replacing an owner.
Methods validate input and allocate IDs atomically; the live registry makes a
valid replacement reuse its ID. A calloop channel carries notification data,
close requests and activation to the compositor. The compositor never waits for
a bus reply. An ordinary unbounded command channel carries action/closed signals
back to the blocking worker. The worker checks the sending application's unique
bus name before invoking history actions.

`notify/mod.rs` owns queue order, deadlines, hover pauses, DND and the 50-item
history. `notify/ui.rs` uses the shared Canvas, text renderer and springs. A
shared reference in TopBar lets `output_elements` render cards through the
Notifications guard without changing backend rendering APIs. History and DND
controls use snapshots in the bar and enter Notifications guards for mutations.
A single one-shot expiry timer tracks the earliest visible deadline. Springs
request frames only while a visible card enters or leaves.

## Classic

`[modules] classic = true` enables the alternate desktop shell, its taskbar,
Start menu, Windows-style shortcuts and the three-finger mode gesture. Windows
remain in their existing workspaces. The shell spring is shared by all outputs;
it stops scheduling frames when settled. Start discovers applications on a
worker thread. A Classic panic drops its menus, hover timer and mode state,
returns to GAVNO and refits fixed windows. The bar supplies the existing status,
quick settings, calendar, notification history and power actions; the dock
supplies application identity and minimize/restore operations.

Classic also owns the desktop directory watch, selection, icon positions and
widgets. They use the primary output, below every window and above wallpaper
layers. Inotify rescans are debounced and delivered through a calloop channel;
workers exit after each operation. Widget clocks and system sampling use
visible-only one-shot timers and stop when covered, locked or outside settled
Classic. The existing tree has no now-playing module, so the optional `media`
widget is skipped. Static desktop, taskbar and Start frames reuse prepared
render handles without allocating or laying out text again. Leaving Classic
releases desktop graphics, watchers and widget sources; panic recovery also
removes them before returning to the normal shell.
