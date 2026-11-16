# Startup and event-loop timing

Startup checkpoints are INFO records on `gavno::startup`. The `ms` field is
milliseconds since the first operation in `main`, using a monotonic clock.
Subtract two checkpoint values to measure the work between them. Start with
`config.*`, `gavno.new.*`, `udev.session.*`, `udev.drm.ready`,
`udev.egl.*`, `udev.renderer.ready`, `udev.output.*`, `xwayland.*`,
`autostart.*`, and `listening`.

Finer checkpoints separate connector scans, EDID, output initialization and mode
attempts, dmabuf feedback, color probing, fonts (discovery/read/parse), icon theme
lookup, PNG/SVG decoding and rasterization, shader programs, cursor theme/images,
Classic construction, and D-Bus worker spawning. Cold asset paths log whenever
they run, including lazy initialization after `listening` and worker-thread
desktop-entry scans. Repeated stages refer to separate cache misses or attempts;
use `output`, `path`, `weight`, and mode fields where present to distinguish them.
Pairs ending in `.end` record return from the operation, including early failure;
they do not imply success. D-Bus spawn completion does not mean the bus is ready.
There is no built-in wallpaper image loader in this version: background layers
come from clients, with a clear color underneath.

For each udev output, `udev.first_commit.begin`/`.submitted` bracket the first
successful frame submission to KMS (including the initial modeset),
`udev.first_frame.queued` records that submission, and
`udev.first_frame.presented` records its successful completion in the DRM vblank
callback. These are separate from output initialization; `listening` does not
guarantee a displayed frame. `input.first_event` is the first input activity
received by the compositor, excluding device-added/removed notifications. It may
be arbitrarily late if nobody uses an input device.
`udev.libinput.first_event` also records the first libinput callback, including
device enumeration, to show when the initial input backlog begins draining.

WARN records on `gavno::stall` report `source` and elapsed callback `ms`, plus
`output` for rendering. The default threshold is 8 ms; set `GAVNO_STALL_MS` before
launch to override it (fractional milliseconds and zero are accepted; invalid
values use 8). All sources share a maximum of 20 warnings in any rolling minute;
further warnings are suppressed. The watchdog does not allocate or lock when
below the threshold.

`calloop.dispatch` includes event handling, idle callbacks (including udev
rendering), and `on_loop_idle`, starting **after** poll returns, so normal sleep
waiting for input/vblank is excluded. `calloop.on_loop_idle` measures the final
maintenance callback separately. Other watched sources are libinput, DRM vblank,
per-output udev rendering, Wayland client dispatch/accept, session and hotplug
callbacks, D-Bus reply channels, Xwayland exit handling, launcher/Classic worker
replies, and desktop inotify handling. All compositor-owned timer callbacks are
watched: frame pacing, bar/lock/greeter/widget clocks, media/launcher key repeat,
OSD/notification/launch expiry, taskbar hover, lock retry/idle (including its
deferred idle callback), widget sampling, and desktop scan debounce.
Smithay-owned timers are covered by the aggregate dispatch measurement.
Nested measurements overlap: a render warning and a dispatch warning for the
same iteration must not be added together. A long aggregate dispatch with short
individual callbacks can mean many callbacks ran in one batch.

Read both targets from the journal:

```sh
journalctl -t gavno | grep -E 'gavno::(startup|stall)'
```

The default `info` tracing filter includes both targets. If using a restrictive
`RUST_LOG`, include `gavno::startup=info,gavno::stall=warn`.

# Automated input and screenshots

Start a development compositor with `GAVNO_DEBUG_INPUT=1 gavno --winit`.
This prints one WARN at startup and enables the methods below in the existing
local JSON IPC. Without that exact environment value they return `unknown
method`. All debug methods, including `state` and `screenshot`, refuse requests
while locked or running the greeter. Greeter startup does not expose window IPC.
Deferred requests check the gate again before acting; a lock interrupts a
pending injected gesture and prevents a pending screenshot from being written.

Use `gavno msg <method> '[json object]'` to send one request and print its JSON
reply (`{"id":1,"result":...}` or `{"id":1,"error":...}`). Errors produce a
nonzero exit status. The client tries `GAVNO_SOCKET`, then the existing
`GAVNO_SOCK` advertised to compositor children, then
`$XDG_RUNTIME_DIR/gavno/ipc.sock`. The compositor also accepts `GAVNO_SOCKET` as
a socket-path override; use a private directory for a separate nested instance.
The socket remains local and restricted to its owner. No new dependencies are
needed for the client.

```sh
export GAVNO_SOCKET="$XDG_RUNTIME_DIR/gavno/ipc.sock"
gavno msg state
gavno msg inject_gesture '{"kind":"swipe","fingers":3,"dx":-240,"dy":0}'
gavno msg inject_key '{"key":"super","state":"press"}'
gavno msg inject_gesture '{"kind":"swipe","fingers":3,"dy":340}'
gavno msg inject_key '{"key":"super","state":"release"}'
gavno msg screenshot '{"path":"/tmp/gavno-workspace.png"}'
```

`inject_gesture` supports `swipe`, `pinch`, and `hold`, with `fingers` from 2 to
5 (default 3), total `dx`/`dy` (default 0), `duration_ms` (default 250, range
1..60000), `steps` (default `max(1, duration_ms / 8)`, range 1..10000), and
`cancel` (default false). Pinch `scale` is the final positive scale relative to
begin (default 1); `rotation` is the total angle in degrees (default 0). Updates
split motion and rotation evenly; pinch scale interpolates from 1. Calloop timers
space updates over the duration, and the reply arrives after end. Holds have
begin and end only, as on libinput. Only one injected gesture runs at a time;
keys can be held or released while it runs. The compositor thread never sleeps.
A gesture reply means end was handled; springs can still be settling.

Synthetic events enter the same input dispatcher as hardware, including XKB,
shortcuts, client forwarding, modifier taps, gesture routing, velocity projection,
and window gestures. Positive deltas mean right/down with the default natural
scroll direction. A fast horizontal swipe can project across multiple workspaces.
Super must be held for a mode-changing vertical gesture. With Overview enabled,
swipe down enters Classic and up returns; with Overview disabled, up enters and
down returns. Nested mode normally uses Alt as its configured shell modifier;
set `mod_key = "super"` (or `GAVNO_MOD=super`) to test Super taps opening the
launcher in GAVNO or Start in Classic.

Other methods:

- `inject_key`: `key` is `super`, `ctrl`, `alt`, `shift`, `return`, `escape`,
  `tab`, `space`, `backspace`, an arrow name, `a`..`z`, `0`..`9`, `f1`..`f12`,
  or an integer Linux evdev keycode (0..767, without the XKB offset).
  `state` is `press`, `release`, or `tap` (press then release).
- `inject_pointer`: required finite `x` and `y` in global logical coordinates,
  including output offsets. Uses the normal absolute pointer path and constraints.
- `inject_button`: `button` is `left`, `right`, or `middle`; `state` is `press`,
  `release`, or `click` (press then release).
- `inject_scroll`: finite `dx`/`dy` (default 0), `source` is `finger` or `wheel`.
  Values use the ordinary logical scroll units; a zero finger axis sends its stop.
- `screenshot`: optional nonempty `path`, default
  `$XDG_RUNTIME_DIR/gavno/shot.png`, as for SIGUSR1. Queues a redraw and captures
  after the next drawn frame of the first output using the same half-resolution
  PNG encoder. Replies with `path`, `width`, and `height` after writing the file;
  times out after five seconds if no frame is drawn. Only one capture can wait.
- `state`: shell mode, settled mode and progress, Overview visibility, active
  workspace per output, focused window title/app ID, launcher/Start/music/polkit
  visibility, pointer coordinates, and pressed modifiers.

Run `scripts/input-demo.sh /tmp/gavno-input-demo` after building (`GAVNO_BIN`
can select a binary, otherwise `${CARGO_TARGET_DIR:-target}/debug/gavno`). It
starts and stops a private nested instance, uses a temporary home/runtime/config
and a clean environment, prints state replies, and saves four screenshots plus
`compositor.log`. Its config uses Super and disables Overview so swipe up enters
Classic. It opens foot or zenity if installed so the horizontal swipe has an
occupied workspace to leave; `GAVNO_DEMO_CLIENT` can supply another command.
A host Wayland or X display and Python 3 (only for safe path encoding in
the script) are required. Debug input adds no recurring idle timers or workers.

# Output power

These normal IPC methods work without `GAVNO_DEBUG_INPUT`, including while locked:

```sh
gavno msg power_off_monitors
gavno msg power_on_monitors
```

They return `{"outputs_powered_on":false}` or `true` inside `result`. Power-off
lasts until input, explicit power-on, session resume/VT activation, or reloading with
`display.off_after_idle_secs = 0`. The wake key/button press, repeats and its
matching release are consumed; pointer motion is forwarded. Locking while off
keeps outputs off. Visible idle inhibitors prevent automatic power-off, but do
not prevent an explicit IPC request.

On TTY, power-off disables the CRTCs and their planes. Pending client frame
callbacks on visible surfaces are delivered at most once per second, with no
rendering and no periodic timer when no callbacks are pending. Nested mode keeps
the host window and its last image, and stops rendering. Debug `state` reports
`outputs_powered_on`, cumulative `rendered_frames`, and
`power_frame_timer_firings`. To check sleeping behavior, power off, record these
counters, wait, and read state again: rendered frames must remain constant;
callback timer firings increase only when clients request callbacks. Injecting
any supported input wakes the outputs and rendering resumes. INFO records
`outputs powered off` and `outputs powered on` include the transition reason.
