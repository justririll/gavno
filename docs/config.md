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

The environment wins except for theme coordinates, where explicit file values take precedence. An empty environment variable counts as unset, so
it does not override the file. Some empty strings in the file also mean "use the
default": see `terminal` and `launcher` below.

## Keys

| Key | Type | Default | Environment override |
|---|---|---|---|
| `mod_key` | string, `"super"` or `"alt"` | `"super"`, or `"alt"` when nested (`--winit`) | `GAVNO_MOD` |
| `terminal` | string | `"foot"` | `GAVNO_TERMINAL` |
| `launcher` | string | unset, Mod+D disabled | `GAVNO_LAUNCHER` |
| `file_manager` | string | `"gavno-files"` | none |
| `polkit_agent` | string | `"builtin"`, a command, or `""` | none |
| `autostart` | string list | `["foot --server"]` | none |
| `gap` | integer, logical px | `12` | none |
| `[tiling] default` | boolean | `false` | none |
| `[lock] after_idle_secs` | integer, seconds | `300` | none |
| `[display] off_after_idle_secs` | u32 seconds, `0` = never | `600` | none |
| `[display] direct_scanout` | boolean | `true` | none |
| `[display] accessibility` | boolean | `false` | none |
| `[display] night_light` | boolean | `true` | none |
| `[display] night_light_temperature` | u32 kelvin, clamped to 1900-6500 | `3400` | none |
| `[display] brightness_control` | string, `"auto"`, `"backlight"` or `"software"` | `"auto"` | none |
| `[output] internal_scale` | float | `1.75` | `GAVNO_SCALE` |
| `[output] mode` | string, `WIDTHxHEIGHT[@HZ]` | preferred mode, highest refresh among preferred modes | `GAVNO_OUTPUT_MODE` |
| `[output] hdr` | boolean | `false` | `GAVNO_HDR` (`true`/`false` or `1`/`0`) |
| `[output] sdr_white_nits` | float, cd/m² | `203` | `GAVNO_SDR_WHITE_NITS` |
| `[keyboard] repeat_delay` | integer, 0–5000 ms | `300` | — |
| `[keyboard] repeat_rate` | integer, 0–100 Hz | `30` | — |
| `[keyboard] layout` | string | `"us,ru"` | `GAVNO_XKB_LAYOUT` |
| `[keyboard] variant` | string | `""` | `GAVNO_XKB_VARIANT` |
| `[keyboard] options` | string | `"grp:alt_shift_toggle"` | `GAVNO_XKB_OPTIONS` |
| `[keyboard] code_mode` | boolean | `false` | none |
| `[keyboard] bar_hints` | boolean | `true` | none |

`mod_key` is the modifier for every bind listed in [keybinds.md](keybinds.md).
Nested mode defaults to Alt because the host compositor keeps Super for itself.
The file accepts only the exact lowercase values `"super"` and `"alt"`.
`GAVNO_MOD` also accepts only those two; any other value falls back to the
nested/TTY default, and still overrides the file.

`autostart` commands start with a TTY session, each only when its program is
installed; `autostart = []` starts nothing. With `foot --server` running (the
default) and `terminal` left at `"foot"`, Mod+Return and the pinned foot icon
use `footclient --app-id foot`: a window in milliseconds instead of a new
process, and one shared process for all terminals. Add `"gavno-files --daemon"`
to keep a windowless file manager resident, so Mod+E opens instantly at the
cost of a few tens of MB of RAM.

`terminal` is started by Mod+Return, `launcher` by Mod+D and `file_manager` by
Mod+E. `polkit_agent` asks for passwords for `pkexec` and other polkit actions.
`"builtin"` uses the compositor's own dialog (no extra process; TTY sessions
only, or nested with `GAVNO_POLKIT_TEST=1`); any other value is a command
started with the session when its program is found, such as the GTK fallback
`gavno-polkit-agent`; `""` runs no agent. A file value that
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

<a id="x11"></a>
X11 clients run through `xwayland-satellite` when it is in `PATH`. GAVNO claims
the X display and sets `DISPLAY` at login, but starts nothing: the satellite and
Xwayland (about 75 MB) start when the first X11 client connects, which makes
that first connection a little slower. If the satellite exits, GAVNO watches
the display again and the next client starts it. After three exits within 60
seconds it stops retrying, logs a warning and removes `DISPLAY`. There is no
configuration key; without the satellite in `PATH`, X11 is disabled.

`[display] accessibility = false` (the default) keeps the AT-SPI accessibility
bus from starting: the TTY session sets `GTK_A11Y=none` and `NO_AT_BRIDGE=1`
for itself and imports both into the D-Bus and systemd user environments, which
saves roughly 25 MB of RSS. Set it to `true` if you use a screen reader. It is
read once at startup, so a change takes effect at the next login and is not
applied on reload. Nested instances never export anything.

On the TTY backend, `[display] direct_scanout = true` allows client and cursor
buffers on hardware planes in SDR. This setting reloads live. A rejected page
flip switches that output to composition for the rest of the session, including
across VT switches, reloads and reconnects; the retry retains refresh-period
pacing. Setting it back to `true` does not clear a driver rejection. HDR always
composes clients, the cursor and software brightness before PQ encoding.
Screenshots still render the original scene offscreen.

Configured HDR is prepared during output initialization. Its initial black XR30
frame commits mode, planes, BT.2020 colorspace and HDR metadata together; Smithay
then adopts that mode and format before the first scene frame. XR30 is preferred for SDR outputs as well
(32 bits per pixel, with SDR transfer unchanged), because Smithay 0.7's output
manager shares format preferences across outputs, including later hotplugs. SDR
client buffers with other formats remain eligible for direct scanout after
Smithay tests their plane assignment. Unsupported formats or
HDR setup failures retain the SDR fallback. Runtime HDR toggles remain available.

`[display] night_light = true` (the default) warms the white point of HDR outputs
in the evening. Its strength follows the living theme's sun model and its
`[theme] latitude`/`longitude` (or the theme's own fallback): none with the sun
3 degrees above the horizon or higher, full at 6 degrees below or lower, smooth
in between. At full strength the white is `night_light_temperature` kelvin
(1900-6500; 6500 changes nothing). The gain is folded into the sRGB to BT.2020
matrix of the final PQ encoding pass, so it costs no extra GPU pass, covers
every HDR pixel (clients, solid colors, the cursor, the greeter and the
software brightness overlay) and keeps the largest channel at 1.0, so nothing
clips and nothing gets brighter. The matrix changes in steps of at least 50 K
(about 60 steps over the evening, each a single full-output redraw) at
deadlines on the theme's shared one-shot clock; there is no periodic timer, and
nothing wakes at night or midday. Outputs powered off do not schedule anything.
Changes reload live. SDR outputs are not adjusted (INFO log once); screenshots
and screencopy capture the scene before the PQ pass, so they keep original
colors. `gavno msg night_light '{"preview": true}'` forces full strength until
`{"preview": false}`; see `docs/debugging.md`.

For OLED panels that ignore backlight writes, select software brightness:

```toml
[display]
brightness_control = "software"
```

`"auto"` uses a backlight device when one exists, otherwise software dimming.
`"backlight"` requires a backlight device; without one the brightness slider is
unavailable. `"software"` sends no backlight writes and dims every output,
including external monitors, in both the TTY and nested backends. Brightness
keys change the global level in 5% steps; both shell sliders and the OSD use the
same level. The minimum is 5%, and transitions settle in about 150 ms. The
setting reloads live; switching to backlight immediately removes software
dimming and shows the hardware level. Switching back restores the saved
software level.

Software brightness is restored at startup from
`$XDG_STATE_HOME/gavno/brightness`, defaulting to
`~/.local/state/gavno/brightness`. A relative `XDG_STATE_HOME` is ignored.
Changes are saved atomically by a worker after 200 ms without further changes;
input never waits for disk writes. The display overlay covers all content,
including the cursor, lock screen and greeter. HDR composition includes it
before PQ encoding. Screenshots (including SIGUSR1) and screencopy retain the
original, undimmed pixels. Once the transition settles, brightness requests no
frames and causes no idle damage.

`[output] internal_scale` applies to built-in panels (eDP) only; external
outputs default to a scale of 1.0. Connector-specific `[outputs."name"]`
settings can override these fallbacks (see Live display and keyboard settings).

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
level. This value is clamped to 80–400. The optional
[`wp_color_management_v1` client protocol](modules.md#color-management) accepts
parametric HDR and wide-gamut surface descriptions; tagged HDR content needs
FP16 composition. Untagged surfaces retain their existing sRGB behavior.

Mod+Shift+H toggles HDR on the active output. Mod+Shift+F2/F3 adjusts SDR white
by −10/+10 nits while HDR is enabled. Runtime changes last until output removal
or compositor restart. The IPC request
`{"id":1,"method":"set_hdr","params":{"output":"eDP-1","enabled":true}}`
returns a queued acknowledgement; actual enablement happens after any pending
page flip. Read `screen.outputs[].hdr` for `enabled`, `pending`, `supported`,
and `sdr_white_nits` after initialization or fallback. `supported` reports EDID
and connector-property eligibility; allocation and rendering can still fail.
Screenshots capture the composition before PQ conversion into 8-bit PNGs;
HDR-tagged pixels may clip in this legacy capture path.

`[keyboard]` values are passed to the seat keyboard. `layout` takes XKB layout
names such as `"us,ru"`; `variant` takes one variant per layout and is empty by
default. Set `options = ""` in the file to disable XKB options; an empty
`GAVNO_XKB_OPTIONS` counts as unset, so the file is the way to turn them off.

`code_mode = true` starts in keyboard-only code mode and applies live on reload.
It tiles every workspace on every output, makes H/J/K/L alias the arrow keys,
hides the pointer while typing and hides the dock; Mod+Shift+C toggles it. See
[keybinds.md](keybinds.md#code-mode). `bar_hints = true` (the default) shows the
compact key hint strip next to the code-mode chip; set it to `false` to hide the
strip while keeping the chip. Both are live settings.


## Living theme

All theme settings reload live in both TTY and nested sessions:

```toml
[theme]
background = "living" # "living" or "solid"
solid_color = "#0e1016"
accent = "living" # or any "#rrggbb"
# latitude = 55.75
# longitude = 37.62

[modules]
theme = true
```

The living sky follows the local sun and season without network access. Each
coordinate is optional: an explicit value wins, then a valid `GAVNO_LAT` or
`GAVNO_LON`, then the time-zone city guess (or longitude derived from UTC offset).
Latitude must be finite and within −90..90; longitude within −180..180.
The lock screen and greeter retain their own skies with these same coordinates.
`app_color_scheme` (`"dark"` by default, `"light"` or `"none"`) is the colour scheme
GAVNO reports to apps through its built-in Settings portal backend (below); it reloads
live and apps are notified at once. The same backend publishes the bar accent as
`accent-color`, at most once per 15 minutes and only after an OKLab shift above 0.02.

Settings exposes a Theme page, color swatches and empty coordinate entries labeled
“use time zone”. Empty entries remove the override; environment fallbacks still apply.

A mapped background-layer wallpaper surface covering an output suppresses the
built-in sky on that output. Classic icons and widgets remain above both kinds
of wallpaper. Fixed accents apply to every shell control, including the OSD.
Disabling `modules.theme`, or a guarded theme panic, uses `solid_color` and the
fixed default accent until re-enabled (panic recovery requires restart).

Theme scheduling shares one one-shot clock with the panel. It predicts the first
future solar sample whose OKLab color difference reaches 0.02, comparing sky
keyframe colors, the accent and sampled glow/field colors. Samples are at least
one minute apart; flat periods can sleep for six hours. The visible panel clock
still updates at minute boundaries, without resampling the theme on skipped minutes.

Ordinary clock updates apply directly and request at most one frame per exposed
output. Only large jumps (OKLab distance at least 0.08) after configuration,
coordinates, startup or resume/late wake use the approximately two-second spring.
Opaque coverage uses the actual render elements' regions, including unions,
transparency and rounded corners. Covered outputs receive no theme frame requests,
and keep their cached accent. HDR preserves composition damage/commit and skips
PQ scanout when nothing visible changed; retries remain pending until a vblank.
No theme image buffer, worker, network access or separate animation timer is used.
See [the measurement report](theme-report.md) for hourly counts and validation.

## Settings portal backend

GAVNO serves `org.freedesktop.impl.portal.Settings` itself as
`org.freedesktop.impl.portal.desktop.gavno` (real TTY session only, never the nested
backend or the greeter). Apps then see `org.freedesktop.appearance` `color-scheme`
(from `theme.app_color_scheme`), `accent-color` (the living-theme accent, sRGB) and
`contrast` (0), so libadwaita/GTK4, Firefox and Qt 6 follow the dark scheme.
`dist/xdg-desktop-portal/gavno.portal` and `gavno-portals.conf` route the interface
to it; `scripts/install-greeter.sh` installs both, and xdg-desktop-portal reads them
at the next login. The service runs on its own thread and costs nothing when idle.

## Optional modules

Every feature can be switched off independently at startup or through an explicit
reload. All flags default to `true`; unknown keys warn. Normal disable/enable
cycles run the feature registry resets; a crashed feature stays disabled until
restart.

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
media = true
color_management = true
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

`gavno-settings` edits every supported key using pages generated from the
compositor's `config_schema` IPC method. Its sidebar includes Displays, Keyboard,
each configuration section, and **Edit config file**. Search matches key names
and descriptions across sections. Rows show descriptions, numeric bounds,
reset-to-default controls, and an **after re-login** tag for startup-only values.
Output overrides can be added by connector name, even without connected outputs.
String lists support add/remove/reorder and show at most 32 editable items per
page. Existing comments and unknown keys survive form edits.

Text rows commit with Enter/the checkmark, when leaving the field, or when
closing the window. Incomplete mode, keymap and theme names are not applied on
every keystroke. Other controls save immediately when changed.

Control edits are saved immediately, then the app calls `reload_config` using
the usual JSON-lines envelope. Writes are serialized and pending edits are
coalesced by key; there is no debounce timer, file watcher or resident helper.
Leaving a section drops its GTK widgets and text layouts. Only the schema and a
bounded config model remain; the file/editor limit is 1 MiB. Search shows up to
64 matching rows, and the connector selector lists up to 128 outputs. The raw
editor remains available for larger collections within the file-size limit.

`reload_config` takes no parameters and returns
`{applied: ["gap", ...], needs_relogin: ["autostart", ...]}` for changed keys.
Disk reads, TOML validation and XKB compilation run on a short-lived worker.
A parse/type error includes its line and column and changes no runtime settings;
a bad keymap also rejects the reload before other changes. Output changes use
the existing live modeset/scale/HDR path, rolling back earlier outputs if a later
output fails. A busy page flip can return `output is presenting; retry`; the app
retries it. HDR still reports asynchronous completion through `outputs`.

Live values include bindings and application commands, gaps, keyboard layout and
repeat, dock/bar, tiling defaults for new/empty workspaces, lock timeout,
notification defaults, output timing/scale/HDR/SDR white, and module switches.
Disabling a module runs its reset and drops its state. Media/notification workers
stop on disable, and Screencopy removes its advertised global. Disabling IPC
closes live control after the reload reply; re-enable it in the file and restart
the session. Features that previously crashed still require a restart.

`polkit_agent`, `autostart`, and per-connector `enabled` apply after re-login;
reload never spawns autostart commands or replaces the authorization agent.
Environment overrides still take precedence. No cursor/theme keys are currently
read, apart from `dock.icon_theme`.

The raw editor uses a plain monospace `gtk::TextView`. Save validates TOML and
known value types before writing, keeps `config.toml.bak`, then requests reload.
Errors select the indicated line. A reload rejection restores the previous file;
the invalid draft remains available for correction. If IPC is unavailable, a
banner says **GAVNO is not running; changes apply at next login** and validated
file editing continues without a polling/reconnect worker. **Refresh** explicitly
retries the connection. Only active display previews have a countdown timer.

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

## Classic shell

The Classic shell shares windows and workspaces with the normal shell. Enable
it with `[modules] classic = true` (the default). Disable that module to keep
the normal shell and its gestures exclusively.

```toml
[classic]
start_in_classic = false
open_maximized = true
align = "left"                   # "left" (default) or "center"
# pinned = ["foot", "firefox"]   # omitted: inherit [dock].pinned; []: no pins
start_pinned = ["@terminal", "@files", "firefox", "@settings"]
settings_command = "gavno-settings"
task_manager = "foot -e btop"
```

Start always opens at the left edge above the Start button, including when
app items are centered.

Pins name desktop-entry IDs, with or without `.desktop`. Missing applications
are skipped. Start also accepts `@terminal`, `@files` and `@settings`, which use
the configured commands. Taskbar Pin/Unpin changes the current session; edit
`classic.pinned` to keep it across restarts. The taskbar is 48 logical pixels
high on every output, and fullscreen windows hide it. Fixed window layouts
refit when the shell transition settles.

Classic floats every workspace, maximizes existing tiles, and restores their
saved tiling trees when returning to GAVNO. `open_maximized` defaults to `true`
and applies immediately on reload to new normal windows. Set it to `false`
to let clients choose their size. Parented dialogs and fixed-size windows are
exempt. xdg-shell does not provide a utility or splash window type; those
clients should supply a parent or fixed-size constraints.

On close, Classic remembers each application's last maximized state or floating
geometry relative to its output work area. Remembered states take precedence
over `open_maximized`, use an available output, and are clamped to fit. The
store holds at most 128 application IDs in least-recently-used order and lives
at `$XDG_STATE_HOME/gavno/classic-windows.json` (default
`~/.local/state/gavno/classic-windows.json`). Loading and changed-state writes
run on a worker without polling. Missing or corrupt state starts empty.

IPC `shell_mode` reads the requested mode, settled mode and transition progress.
Passing `{"mode":"classic"}` or `{"mode":"gavno"}` changes it. A disabled
Classic module rejects changes. Desktop icons and widgets are configured below.

### Desktop icons and widgets

```toml
[classic]
align = "left"                   # default; "center" centers app items only
# Start and its menu always stay at the left edge.
desktop_home = true
desktop_trash = true
widgets = ["clock", "system", "media", "music"]
```

Desktop icons and widgets use the primary output (the compositor's first
output). Icons come from `xdg-user-dir DESKTOP`, with `user-dirs.dirs` and
`~/Desktop` as fallbacks. The directory is created when needed. Dotfiles are
hidden. Desktop shortcuts use their desktop-entry Name and Icon; other files
use themed icons selected by extension. Home and Trash are virtual shortcuts.

Icons occupy a column-major 96×104 logical-pixel grid. The grid avoids widget
panels. Positions are stored by filename in
`$XDG_STATE_HOME/gavno/desktop.json` (default `~/.local/state/gavno/desktop.json`),
together with widget positions. Writes are atomic and happen only after changes.
Renaming refuses existing destinations and names containing `/`; it renames the
file, including the filename of a `.desktop` shortcut. Trash operations use
`gio trash`, never permanent deletion.

Up to four panels follow the order in `widgets`; duplicates and unknown
names are skipped. `media` shows the active MPRIS player (title, artist and
previous / play-pause / next buttons) from the Media module, or "Nothing
playing". `music` shows Spotify from the music panel daemon or its dedicated
MPRIS player, with cover art and playback buttons. Click its body to browse Spotify
in a panel beside the widget; drag its body to move it. Browsing requires the
`org.gavno.Music` daemon and `[media] music_panel = true`. The widget does not
start the music worker: it uses existing state and decoded covers, with no polling.
The clock refreshes at minute boundaries, and
system CPU/memory data every two seconds. Both stop refreshing when Classic is
not settled, the primary desktop is covered by a maximized/fullscreen window,
or the session is locked. Network and battery data reuse the bar's status.

Desktop directory updates use inotify with a 100 ms debounce. There is no file
polling or resident desktop helper. File operations and scans run in short-lived
workers. Scans are bounded to 4096 non-hidden entries; at most 256 grid cells
are visible in a page. Scrolling over the desktop browses overflow pages.
Prepared text/image caches are bounded and released on leaving Classic.

## Live display and keyboard settings

`gavno-settings` includes live Displays and Keyboard pages alongside the full
configuration editor. Display changes have a
15-second confirmation deadline: Keep persists them; Revert, closing the window,
or expiry restores the previous live state. Keyboard's Apply and save validates
and applies the keymap before writing it. The app preserves comments and
unrelated TOML keys using an atomic replacement. It follows the same config path
as the compositor. Environment overrides still affect the next session.

Connector tables override individual global output fallbacks:

```toml
[outputs."eDP-1"]
mode = "2880x1620@120" # Hz; fractional values such as 59.94 are supported
scale = 1.75
hdr = false
enabled = true

[outputs."DP-1"]
scale = 1.25
```

Every key is optional. Absent mode/HDR inherit `[output] mode`/`hdr`; absent scale
inherits `internal_scale` for internal panels, or 1.0 for external outputs.
Invalid connector modes/scales warn and use those fallbacks. Connector scales
must be finite and within 0.5–3.0. Enabled defaults to true. Configured enabled
outputs are initialized first; if none work, one configured disabled output is
kept enabled so the session retains a display. Enabling/disabling an output is
currently a startup configuration operation.

IPC uses the envelope `{"id":1,"method":"outputs","params":{}}` on
`$XDG_RUNTIME_DIR/gavno/ipc.sock`, one JSON request/response per line. Replies
contain `id` and either `result` or `error`. All methods reject locked sessions.

- `outputs`: array with `name`, `make`, `model`, `enabled`, `mode`, `modes`,
  `scale`, `position: {x,y}`, `hdr: {supported,enabled,pending}`, and `internal`.
  Each mode has `w`, `h`, `refresh` in **millihertz**, and `preferred`.
- `set_output`: `{output, mode?: {w,h,refresh?}, scale?, hdr?}`. Refresh uses
  millihertz; omitting it selects the highest advertised refresh for the size.
  Scale accepts 0.5–3.0 in steps of 0.05. The compositor tests modes through
  Smithay's DRM modeset path, restores the previous mode on failure, refits
  windows, and redraws layers, bar and dock. A flip in progress returns the
  transient error `output is presenting; retry`. HDR follows `set_hdr`'s deferred
  application path; `pending` reports the requested value until applied.
- `keyboard`: `{layout,variant,options,active_group,group_names}` with a
  zero-based active group.
- `set_keyboard`: `{layout,variant?,options?}`; omitted variant/options are
  empty. Supports up to four comma-separated layouts. XKB compilation runs in a
  worker before the seat keymap changes. Invalid input changes nothing.

Mutations are audited like `set_hdr`. The compositor does not write live display
or keyboard changes back to disk. These controls belong to the existing IPC
module and are disabled with `[modules] ipc = false`.

## Now playing (MPRIS)

`[modules] media = true` (the default) discovers session-bus MPRIS players such
as Firefox, Spotify and mpv. A playing or paused player appears in a capsule
between the clock and status area. Click it to toggle playback. Quick settings
shows its title, artist, 48 px art and previous / play-pause / next controls.
Unavailable actions are dimmed and ignored. The built-in lock screen shows the
same track and controls beneath its clock; media keys work while locked too.

The most recently started playing player is active. If none is playing, the
most recently updated paused player is active. Stopped and vanished players do
not appear. Startup cannot recover historical playback order, so simultaneous
already-playing players use a deterministic discovery order.

Only local `file://` album art is loaded, with percent escapes decoded. Remote
art URLs are ignored; the desktop application's icon is used instead, with a
music glyph as the final fallback. The existing PNG/SVG icon loader decodes art
and app icons on the worker thread. No position tracking or polling is used.
Disable `modules.media` to remove the controls and pass playback keys to clients.

## Complete key reference

The compositor schema is the source for the following keys. A null default means
unset/inherited; resetting such a row removes the key. `*` denotes a connector
name such as `eDP-1`. Numeric steps describe the editor increments.

| Key | Type | Default | Live |
|---|---|---|---|
| `mod_key` | enum (super, alt) | `"super"` | yes |
| `terminal` | string | `"foot"` | yes |
| `launcher` | string | `unset` | yes |
| `file_manager` | string | `"gavno-files"` | yes |
| `polkit_agent` | string | `"builtin"` | after re-login |
| `autostart` | string_list | `["foot --server"]` | after re-login |
| `gap` | int (0–64, step 1) | `12` | yes |
| `output.internal_scale` | float (0.5–4.0, step 0.05) | `1.75` | yes |
| `output.mode` | string | `unset` | yes |
| `output.hdr` | bool | `false` | yes |
| `display.direct_scanout` | boolean | `true` | yes |
| `display.night_light` | boolean | `true` | yes |
| `display.night_light_temperature` | int (1900-6500, step 100) | `3400` | yes |
| `display.accessibility` | boolean | `false` | after re-login |
| `output.sdr_white_nits` | float (80.0–400.0, step 10.0) | `203.0` | yes |
| `keyboard.repeat_delay` | int (0–5000, step 1) | `300` | yes |
| `keyboard.repeat_rate` | int (0–100, step 1) | `30` | yes |
| `keyboard.layout` | string | `"us,ru"` | yes |
| `keyboard.variant` | string | `""` | yes |
| `keyboard.options` | string | `"grp:alt_shift_toggle"` | yes |
| `keyboard.code_mode` | bool | `false` | yes |
| `keyboard.bar_hints` | bool | `true` | yes |
| `modules.dock` | bool | `true` | yes |
| `modules.bar` | bool | `true` | yes |
| `modules.overview` | bool | `true` | yes |
| `modules.tiling` | bool | `true` | yes |
| `modules.snap` | bool | `true` | yes |
| `modules.switcher` | bool | `true` | yes |
| `modules.gestures` | bool | `true` | yes |
| `modules.window_gestures` | bool | `true` | yes |
| `modules.resize_mode` | bool | `true` | yes |
| `modules.animations` | bool | `true` | yes |
| `modules.decorations` | bool | `true` | yes |
| `modules.ipc` | bool | `true` | yes |
| `modules.screencopy` | bool | `true` | yes |
| `modules.launcher` | bool | `true` | yes |
| `modules.notifications` | bool | `true` | yes |
| `modules.media` | bool | `true` | yes |
| `dock.enabled` | bool | `true` | yes |
| `dock.pinned` | string_list | `["foot", "firefox"]` | yes |
| `dock.magnify` | bool | `true` | yes |
| `dock.autohide` | bool | `false` | yes |
| `dock.icon_theme` | string | `"Adwaita"` | yes |
| `bar.enabled` | bool | `true` | yes |
| `tiling.default` | bool | `false` | yes |
| `lock.after_idle_secs` | int (0–4294967295, step 1) | `300` | yes |
| `notifications.timeout_ms` | int (0–9223372036854775807, step 1) | `5000` | yes |
| `notifications.max_visible` | int (1–3, step 1) | `3` | yes |
| `outputs.*.mode` | string | `unset` | yes |
| `outputs.*.scale` | float (0.5–3.0, step 0.05) | `unset` | yes |
| `outputs.*.hdr` | bool | `unset` | yes |
| `outputs.*.enabled` | bool | `unset` | after re-login |

## Idle output power

`[display] off_after_idle_secs = 600` powers off every output after ten minutes
of inactivity. Set it to `0` to disable automatic power-off. It is independent
of `[lock] after_idle_secs`: either timeout can be disabled while the other
remains active. Keyboard, pointer, touch, gesture and switch activity reset the
timeout; active idle inhibitors on visible surfaces prevent automatic power-off,
using the same policy as idle locking. `org.freedesktop.ScreenSaver` inhibits
from apps (games, Steam, Firefox, Electron, X11 clients) count the same way,
except while the session is locked; they end when the app calls `UnInhibit` or
leaves the bus. The timeout also applies on the lock
screen and greeter. Changes apply on reload; reloading with the value `0` wakes
sleeping outputs. See [Output power](debugging.md#output-power) for IPC controls
and wake behavior.
