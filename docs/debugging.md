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
