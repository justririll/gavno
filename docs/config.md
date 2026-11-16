# Configuration

GAVNO reads one optional TOML file at startup. Every key is optional: omitted
keys keep the built-in default. The annotated example is `dist/config.toml`.

## File location

GAVNO uses the first of these that is set and usable:

1. `GAVNO_CONFIG`, used as given.
2. `$XDG_CONFIG_HOME/gavno/config.toml`, if `XDG_CONFIG_HOME` is set and
   absolute.
3. `$HOME/.config/gavno/config.toml`.

A relative `XDG_CONFIG_HOME` is ignored, because XDG base directories must be
absolute. Empty `GAVNO_CONFIG` or `XDG_CONFIG_HOME` counts as unset. If none of
the paths can be built (for example, no `HOME`), no file is read. A missing file
is not an error; the defaults apply.

## Precedence

Values are merged in increasing priority:

1. Built-in defaults.
2. The config file.
3. `GAVNO_*` environment variables.

The environment always wins. An empty environment variable counts as unset, so
it does not override the file. Some empty strings in the file also mean "use the
default": see `terminal` and `launcher` below.

## Keys

| Key | Type | Default | Environment override |
|---|---|---|---|
| `mod_key` | string, `"super"` or `"alt"` | `"super"`, or `"alt"` when nested (`--winit`) | `GAVNO_MOD` |
| `terminal` | string | `"foot"` | `GAVNO_TERMINAL` |
| `launcher` | string | unset, Mod+D disabled | `GAVNO_LAUNCHER` |
| `file_manager` | string | `"gavno-files"` | none |
| `gap` | integer, logical px | `12` | none |
| `[tiling] default` | boolean | `false` | none |
| `[lock] after_idle_secs` | integer, seconds | `300` | none |
| `[output] internal_scale` | float | `1.75` | `GAVNO_SCALE` |
| `[output] mode` | string, `WIDTHxHEIGHT[@HZ]` | preferred mode, highest refresh among preferred modes | `GAVNO_OUTPUT_MODE` |
| `[output] hdr` | boolean | `false` | `GAVNO_HDR` (`true`/`false` or `1`/`0`) |
| `[output] sdr_white_nits` | float, cd/m² | `203` | `GAVNO_SDR_WHITE_NITS` |
| `[keyboard] repeat_delay` | integer, 0–5000 ms | `300` | — |
| `[keyboard] repeat_rate` | integer, 0–100 Hz | `30` | — |
| `[keyboard] layout` | string | `"us,ru"` | `GAVNO_XKB_LAYOUT` |
| `[keyboard] variant` | string | `""` | `GAVNO_XKB_VARIANT` |
| `[keyboard] options` | string | `"grp:alt_shift_toggle"` | `GAVNO_XKB_OPTIONS` |

`mod_key` is the modifier for every bind listed in [keybinds.md](keybinds.md).
Nested mode defaults to Alt because the host compositor keeps Super for itself.
The file accepts only the exact lowercase values `"super"` and `"alt"`.
`GAVNO_MOD` also accepts only those two; any other value falls back to the
nested/TTY default, and still overrides the file.

`terminal` is started by Mod+Return, `launcher` by Mod+D and `file_manager` by
Mod+E. A file value that
is empty (or whitespace only, for `terminal`) falls back to the default, so
`launcher = ""` disables the bind. `GAVNO_TERMINAL` and `GAVNO_LAUNCHER` are
used verbatim when non-empty.

`gap` is the distance between a new window and the work area edge. There is no
environment override.

Set `[tiling] default = true` to create workspaces in tiling mode. Existing
workspaces keep their mode when another workspace is toggled. Mod+T,
Mod+Alt+Space, or the mode chip next to the workspace dots toggles the current
workspace. Floating geometry is restored when returning to floating mode.
Both outer and inner tiling gaps use `gap`. Tiling is per workspace; the mode
is not global. See [keybinds.md](keybinds.md) for the tiling binds, splitter
drag, and per-window floating override. IPC `togglefloating` accepts a window
`id`.

`[lock] after_idle_secs` is the idle delay before GAVNO locks the session.
The default is 300 seconds; `0` disables automatic locking. Input resets the
timer, and a mapped idle inhibitor postpones the lock. The timer is not armed
in greeter mode or while the session is already locked. GAVNO also locks
before suspend and on a logind Lock request. See [the lock screen](lock.md)
for authentication, backoff and what is blocked while locked.

`[output] internal_scale` applies to built-in panels (eDP) only; external
outputs always use a scale of 1.0.

`[output] mode = "2880x1620@120"` selects the panel's 120 Hz timing.
Omit `@HZ` to select the highest refresh at that resolution. Integer Hz allow
±0.5 Hz for rounded timings; fractional Hz (up to three decimal places) allow
±0.001 Hz. When the requested timing is unavailable, GAVNO warns and selects
the preferred timing. These settings apply independently to each DRM output;
the nested backend keeps its host-selected mode.

HDR requires atomic modesetting, EDID PQ + BT.2020 RGB + static metadata type 1,
mutable connector color properties, XR30 scanout, and fp16 or 10-bit offscreen
rendering. GAVNO warns and keeps SDR when initialization fails. SDR content is
composed as usual and converted to BT.2020/PQ with `sdr_white_nits` as its white
level. This value is clamped to 80–400. There is no HDR client protocol yet
(`wp-color-management-v1` is not implemented).

Mod+Shift+H toggles HDR on the active output. Mod+Shift+F2/F3 adjusts SDR white
by −10/+10 nits while HDR is enabled. Runtime changes last until output removal
or compositor restart. The IPC request
`{"id":1,"method":"set_hdr","params":{"output":"eDP-1","enabled":true}}`
returns a queued acknowledgement; actual enablement happens after any pending
page flip. Read `screen.outputs[].hdr` for `enabled`, `pending`, `supported`,
and `sdr_white_nits` after initialization or fallback. `supported` reports EDID
and connector-property eligibility; allocation and rendering can still fail.
Screenshots remain sRGB PNGs of the composition before PQ conversion.

`[keyboard]` values are passed to the seat keyboard. `layout` takes XKB layout
names such as `"us,ru"`; `variant` takes one variant per layout and is empty by
default. Set `options = ""` in the file to disable XKB options; an empty
`GAVNO_XKB_OPTIONS` counts as unset, so the file is the way to turn them off.


## Optional modules

Every feature can be switched off independently at startup. All flags default to
`true`; unknown keys warn. There is no live reload.

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

`dock.enabled` and `bar.enabled` still work: a feature is off if either its legacy
flag or its `[modules]` flag is false. Disabled feature bindings reach clients;
its UI, reserve and animation work disappear. IPC and Screencopy start only when
enabled. A feature panic disables that feature until restart and recovers its
state; Lock and Greeter remain core and fail closed. See [modules.md](modules.md)
for boundaries, recovery and crash logs.

## Validation and clamping

- `gap` is clamped to `0..=64`. Out-of-range values are logged with the original
  and the clamped value.
- `internal_scale` is clamped to `0.5..=4.0`, from the file or from
  `GAVNO_SCALE`, with the same warning. `NaN` becomes `1.75`; infinities clamp to
  the nearest bound.
- A `GAVNO_SCALE` that is not a number becomes `1.75` without a warning. It
  still overrides the file.
- Unknown keys are ignored with one warning that lists all of them. Nested keys
  are reported with their section name, for example `output.<key>`,
  `keyboard.<key>`, `tiling.<key>` or `lock.<key>`.
- A file `internal_scale` or `sdr_white_nits` must be finite. `nan`, `inf` and `-inf` are treated as
  an invalid file, not clamped.

## Errors

A file is discarded as a whole, with one warning, when it cannot be used:

- invalid TOML syntax;
- a value of the wrong type, for example `gap = "big"` or `keyboard = 1`;
- an unknown `mod_key`, for example `"ctrl"`;
- a negative, too large or non-integer `[lock] after_idle_secs`;
- a non-finite `[output] internal_scale` or `sdr_white_nits`.

GAVNO then starts with the built-in defaults; `GAVNO_*` variables still apply.
A file that exists but cannot be read (permissions, I/O) produces a warning and
is treated as absent.

Warnings go through the normal log. GAVNO never refuses to start because of a
bad configuration file.

## Reloading

The file is read once at startup. Restart GAVNO to apply changes. Live reload is
planned but not implemented.

## Settings that are not file keys

`GAVNO_GESTURE_DISTANCE` and `GAVNO_NATURAL_SCROLL` tune gesture behaviour and
have no config-file key; they are described in [keybinds.md](keybinds.md).
`GAVNO_CONFIG` selects the file, and `GAVNO_SOCK` and `GAVNO_GREETER_SESSION`
belong to IPC and greeter mode rather than to this file.

## Example

```toml
mod_key = "super"
terminal = "foot"
launcher = "fuzzel"
gap = 12

[tiling]
default = false

[lock]
after_idle_secs = 300

[output]
internal_scale = 1.75
hdr = false
sdr_white_nits = 203

[keyboard]
layout = "us,ru"
variant = ""
options = "grp:alt_shift_toggle"
```

## Built-in notifications

GAVNO serves `org.freedesktop.Notifications` on the session bus (specification
1.2), so `notify-send` and application notifications need no external daemon.
If another daemon already owns the name, GAVNO logs one warning and leaves its
notification UI inactive. Greeter mode never starts the server.

```toml
[notifications]
timeout_ms = 5000
max_visible = 3
```

`timeout_ms` is the default for applications requesting the server default
(`expire_timeout = -1`). An explicit zero timeout and critical urgency are
persistent. `max_visible` is clamped to `1..=3`; additional cards wait in order.
Cards appear below the bar on the active output. Hover pauses expiry; the close
button dismisses a card, and its body invokes an offered default action before
dismissing it. Up to three other actions appear as buttons.

Quick settings offers **Do not disturb**. Normal popups are suppressed while it
is on; critical popups still appear. The clock shows a dot for unread history.
Click the clock to see the newest four of the last 50 notifications beneath the
calendar, or use **Clear** to clear history. A history item invokes its default
action while the sending application remains available, including after its popup closes.
History is kept in memory for the session, including notifications received
while locked. Locked and greeter screens never draw cards; after unlock,
non-expired queued cards can appear. Queued cards age while waiting, including
under DND or the lock; expiry is checked when the event loop next wakes. Only
visible, unhovered cards have an expiry timer, so hidden queues do not poll.

Supported hints are `urgency`, `desktop-entry`, and `image-path`. Bodies display
plain text after simple markup/entity removal. Image-data hints are ignored.
Set `[modules] notifications = false` to disable the server and all its UI.
