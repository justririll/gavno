# GAVNO Settings

`gavno-settings` edits the entire GAVNO configuration, including live displays
and XKB keyboard layouts, over `$XDG_RUNTIME_DIR/gavno/ipc.sock`. It also works
offline and exits when its last window closes; no daemon is added.

The sidebar contains Displays, Keyboard, one page per schema section, and Edit
config file. Search filters by key or description. Forms use typed controls,
editable string lists and reset buttons. Startup-only values show an "after
re-login" tag. Per-output overrides can be added by connector name.

Text rows commit with Enter/the checkmark, when leaving the field, or when
closing the window. Incomplete mode, keymap and theme names are not applied on
every keystroke. Other controls save immediately when changed.

Each committed control edit writes atomically and calls `reload_config`. Pending
edits are coalesced and writes are serialized. Reload errors restore the previous
file; offline edits apply at next login. The raw monospace editor validates
syntax and known values before writing and keeps `config.toml.bak`. Errors show
and select the reported line. Raw drafts survive switching pages.

The single schema lives in `crates/gavno/src/config_schema.rs`; the app includes
that same source as its offline fallback. GTK widgets exist only for the current
page. List editors render at most 32 items, search at most 64 results, connector
selection at most 128 names, and config/raw buffers are limited to 1 MiB. Worker
threads are short-lived and exit after each I/O operation; there are no idle
workers, file watchers, polling or per-frame Rust callbacks. Only an active
display preview runs a countdown timer.

Polkit/autostart commands and connector enable/disable require re-login. Enabling
a module resets its normal disabled state; crashed modules still require restart.
Disabling IPC also disconnects live settings until the next session.

Build with the workspace and install the binary plus
`dist/applications/org.gavno.Settings.desktop`. Requires GTK 4.18, libadwaita 1.6,
and XKB rules (`evdev.lst` or `base.lst`) in `/usr/share/X11/xkb/rules`.

Displays offer advertised modes, scales in steps of 0.05, common scale presets,
and HDR on supported outputs. Apply previews changes for 15 seconds. Keep saves;
Revert, closing the window, or the deadline restores the preceding live state.
The rollback worker runs independently of GTK. Killing the process cannot run a
rollback; restart the session to load the last saved settings in that case.
Configured disabled outputs are listed but cannot be enabled from this app.

Keyboard supports up to four layouts, ordered with the arrow buttons, and
Alt+Shift, Super+Space or Caps Lock switching. Existing layout variants and
unrelated XKB options are retained. Apply and save validates the keymap live and
then persists the selection.

Writes use `toml_edit`, preserving comments and unrelated keys, then atomically
replace the file. Configuration follows `$GAVNO_CONFIG`, `$XDG_CONFIG_HOME`, then
`~/.config/gavno/config.toml`, matching the compositor. Environment overrides can
still take precedence at the next login. An error saving is shown without
silently discarding the accepted live settings.

Tests: `cargo test -j 4 -p gavno-settings`.

## Resource measurement

Local x86_64 GTK/Broadway measurements (one mock eDP output, default window
sizes, after startup settled) were approximately **59.5 MiB RSS before** and
**59.2 MiB after** this change on the Displays page. Both had six native GTK
threads after its startup work settled; the added Rust I/O workers had exited.
The app added no CPU ticks during a two-second idle sample. Visiting generated
forms, raw editing and search peaked around 63 MiB in the same fixture. Closing
the window exited the process normally.

The isolated compositor harness measured **192 KiB RSS** for the first schema
request and approximately **1.7 MiB total** after a reload, including first-use
parser/code/allocator pages. Twelve more reload/restore cycles caused no further
RSS growth; thread count stayed at two (the test runner and test thread). Run:

```sh
cargo test -j 4 -p gavno config_schema_tests::rss_probe -- --ignored --nocapture
```

These are headless development measurements, not ARM/DRM/GPU memory estimates.
Exact RSS varies between processes. Native Broadway occasionally emits frame
timing warnings, also observed with the pre-change binary; this is separate from
compiler warnings and the compositor's render path.
