# GAVNO polkit agent

`gavno-polkit-agent` registers a polkit authentication agent for the whole
login session, so `pkexec` and other polkit clients can ask for a password.
It stays windowless until needed and opens a separate dark GTK4/libadwaita
window for each request. Passwords are checked by the system's setuid
`polkit-agent-helper-1`; no polkit policy changes are needed.

`dist/gavno-session` starts it through GAVNO's `-c` command after the Wayland
socket and first output are ready. Install the updated session wrapper as
`/usr/local/bin/gavno-session` and install the binary:

```sh
install -Dm755 /home/kirill/HDD/ds-build/target-polkit/debug/gavno-polkit-agent /usr/local/bin/gavno-polkit-agent
```

For a manually started compositor, use `gavno --tty -c gavno-polkit-agent`,
or run `gavno-polkit-agent` from a terminal in the existing session.
The agent uses `XDG_SESSION_ID`, falling back to logind's session for its PID.
An existing session agent makes it exit successfully. SIGTERM and SIGINT
cancel active prompts and unregister before exiting.
