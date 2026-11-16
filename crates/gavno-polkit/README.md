# GAVNO polkit agent (external fallback)

GAVNO has a built-in polkit agent (`polkit_agent = "builtin"`, the default)
that draws its dialog in the compositor without a resident process. This
crate is the optional external GTK agent, for other compositors or when the
built-in one is disabled.

`gavno-polkit-agent` registers a polkit authentication agent for the whole
login session, so `pkexec` and other polkit clients can ask for a password.
It stays windowless until needed and opens a separate dark GTK4/libadwaita
window for each request. Passwords are checked by the system's setuid
`polkit-agent-helper-1`; no polkit policy changes are needed.

GAVNO starts it once its Wayland socket is up, using the `polkit_agent`
config key when it is set to `gavno-polkit-agent` (an empty string runs no
agent), if the binary is in `PATH`. Install a release build:

```sh
install -Dm755 "$CARGO_TARGET_DIR/release/gavno-polkit-agent" /usr/local/bin/gavno-polkit-agent
```

polkit's default administrator is root. When the root account is locked,
let wheel members authenticate admin actions with their own password:

```sh
install -Dm644 dist/polkit-1/rules.d/49-gavno-wheel-admin.rules /etc/polkit-1/rules.d/49-gavno-wheel-admin.rules
```

Outside GAVNO, run `gavno-polkit-agent` from a terminal in the existing session.
The agent uses `XDG_SESSION_ID`, falling back to logind's session for its PID.
An existing session agent makes it exit successfully. SIGTERM and SIGINT
cancel active prompts and unregister before exiting.
