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
| `gap` | integer, logical px | `12` | none |
| `[output] internal_scale` | float | `1.75` | `GAVNO_SCALE` |
| `[keyboard] layout` | string | `"us,ru"` | `GAVNO_XKB_LAYOUT` |
| `[keyboard] variant` | string | `""` | `GAVNO_XKB_VARIANT` |
| `[keyboard] options` | string | `"grp:alt_shift_toggle"` | `GAVNO_XKB_OPTIONS` |

`mod_key` is the modifier for every bind listed in [keybinds.md](keybinds.md).
Nested mode defaults to Alt because the host compositor keeps Super for itself.
The file accepts only the exact lowercase values `"super"` and `"alt"`.
`GAVNO_MOD` also accepts only those two; any other value falls back to the
nested/TTY default, and still overrides the file.

`terminal` is started by Mod+Return and `launcher` by Mod+D. A file value that
is empty (or whitespace only, for `terminal`) falls back to the default, so
`launcher = ""` disables the bind. `GAVNO_TERMINAL` and `GAVNO_LAUNCHER` are
used verbatim when non-empty.

`gap` is the distance between a new window and the work area edge. There is no
environment override.

`[output] internal_scale` applies to built-in panels (eDP) only; external
outputs always use a scale of 1.0.

`[keyboard]` values are passed to the seat keyboard. `layout` takes XKB layout
names such as `"us,ru"`; `variant` takes one variant per layout and is empty by
default. Set `options = ""` in the file to disable XKB options; an empty
`GAVNO_XKB_OPTIONS` counts as unset, so the file is the way to turn them off.

## Validation and clamping

- `gap` is clamped to `0..=64`. Out-of-range values are logged with the original
  and the clamped value.
- `internal_scale` is clamped to `0.5..=4.0`, from the file or from
  `GAVNO_SCALE`, with the same warning. `NaN` becomes `1.75`; infinities clamp to
  the nearest bound.
- A `GAVNO_SCALE` that is not a number becomes `1.75` without a warning. It
  still overrides the file.
- Unknown keys are ignored with one warning that lists all of them. Nested keys
  are reported as `output.<key>` and `keyboard.<key>`.
- A file `internal_scale` must be finite. `nan`, `inf` and `-inf` are treated as
  an invalid file, not clamped.

## Errors

A file is discarded as a whole, with one warning, when it cannot be used:

- invalid TOML syntax;
- a value of the wrong type, for example `gap = "big"` or `keyboard = 1`;
- an unknown `mod_key`, for example `"ctrl"`;
- a non-finite `[output] internal_scale`.

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

[output]
internal_scale = 1.75

[keyboard]
layout = "us,ru"
variant = ""
options = "grp:alt_shift_toggle"
```
