# Login manager: greetd + GAVNO greeter

## Choice

| Part | What |
|---|---|
| Daemon | **greetd** (`gui-libs/greetd`, in ::gentoo). It is small, written in Rust, handles PAM and logind sessions, and talks JSON over a Unix socket. It is the only part that runs as root. |
| Greeter | **`gavno --greeter`**: GAVNO itself in a greeter mode. It runs as the unprivileged `greetd` user and draws the login UI with the compositor's own renderer and springs. |
| Fallback | **tuigreet** (`gui-apps/tuigreet`) on the TTY. It is used while the GAVNO greeter does not exist yet, and later as the recovery path. |

Rejected alternatives:
- SDDM and GDM pull in Qt or GNOME Shell for one screen.
- ly is a TUI only.
- Using gtkgreet with cage means a second toolkit and a look that does not
  match GAVNO.

Running GAVNO in greeter mode gives four things:
- The login screen looks exactly like the desktop: living theme, wallpaper
  and springs.
- No extra toolkit is needed.
- The hand-off to the session is seamless: the greeter fades out and greetd
  starts the user's `gavno` on the same VT.
- The lock screen (ext-session-lock) can reuse the same UI.

## Flow

```
boot -> greetd (root, VT 1)
          `-> gavno --greeter (user greetd)  -- greetd IPC socket ($GREETD_SOCK)
                 user picks account, types password
                 create_session -> post_auth_message_response(password)
                 start_session(["gavno-session"]) -> greeter exits (fade out)
          `-> greetd starts `gavno-session` for the user (PAM + logind, VT 1)
```

`gavno-session` is a small script:
- import the environment into the systemd user manager;
- start `gavno` with `--tty`;
- on exit, stop `graphical-session.target`.

## Greeter UI (English, like all of GAVNO)

- Clock and date, large, with the living-theme background (season + time of day).
- User avatar and name, taken from AccountsService if present, else
  `/etc/passwd` (uid >= 1000).
- The password field, which shows the keyboard layout (`US`/`RU`; Alt+Shift
  switches it).
- A Caps Lock warning.
- The session picker: GAVNO (default), or a plain shell (TTY).
- Power controls: Suspend, Restart, Shut down, all through logind.
- Errors (a wrong password) show as a shake animation plus a message. No
  dialogs.

## Security rules

- The greeter never runs as root. The password goes straight from the text
  field to greetd and is never logged or stored.
- gavno-agent cannot read or drive the greeter or the lock screen (see
  design/agent.md).
- There is no autologin by default. It can be enabled explicitly in
  `/etc/greetd/config.toml` (`initial_session`).

## Steps

1. **Now (interim):**
   - Install greetd and tuigreet.
   - Add `/etc/greetd/config.toml` with
     `command = "tuigreet --time --remember --asterisks --cmd gavno-session"`.
   - Install `gavno-session` and `/usr/share/wayland-sessions/gavno.desktop`.
   - Enable `greetd.service` and set `graphical.target` as the default.
   - This needs root and changes how the laptop boots, so the user approves
     it first.
2. **With GAVNO phase 2 (text rendering in the compositor):**
   - Add the `--greeter` mode using the `greetd_ipc` crate.
   - Replace tuigreet in the config, and keep tuigreet installed as the
     fallback.
3. **With the lock screen:**
   - ext-session-lock in GAVNO.
   - The same UI, plus PAM through a small setuid-free helper (`unix_chkpwd`
     path, the same as swaylock).
