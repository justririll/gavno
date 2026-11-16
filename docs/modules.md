# Feature isolation

The core owns Wayland clients, input delivery, mapped windows, output lifetimes,
workspace ownership, and both session-lock systems. Shared geometry helpers and
basic surface rendering remain core even when their implementation lives beside
a feature. Lock and Greeter are always present security boundaries.

The optional features are Dock, Bar, Overview, Tiling, Snap, Switcher,
Gestures, WindowGestures, ResizeMode, Animations, Decorations, Ipc and Screencopy.
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
