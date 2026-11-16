# GAVNO Settings

`gavno-settings` configures connected displays and XKB keyboard layouts over the
GAVNO JSON-lines IPC socket at `$XDG_RUNTIME_DIR/gavno/ipc.sock`.

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
