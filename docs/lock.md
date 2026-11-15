# Lock screen

GAVNO has a built-in lock screen. It is drawn by the compositor, not by a
client, and it authenticates the current user through Linux-PAM.

## Locking

A session locks when:

- Mod+L or Ctrl+Alt+L is pressed;
- Lock is chosen in the top bar power menu;
- the session has been idle for `[lock] after_idle_secs` seconds (default
  300; `0` disables automatic locking, but manual and pre-suspend locking
  still work). Input resets the timer, and a mapped idle inhibitor postpones
  it;
- logind reports that the machine is about to sleep. GAVNO holds a sleep
  delay inhibitor, locks, and releases the inhibitor only after a lock frame
  has been presented on every output;
- logind sends a Lock request for the session.

The lock UI shows the clock, the local user, the password field, the active
keyboard layout, and buttons for Suspend, Restart and Shut down.

## Authentication

The user name comes from the process's real uid (`getpwuid_r`), never from
the environment or a client. The password is sent to a worker thread, passed
to PAM, and wiped from compositor memory after the conversation.

GAVNO uses the PAM service `gavno-lock`. Install
`dist/pam.d/gavno-lock` as `/etc/pam.d/gavno-lock`; the shipped file includes
`system-local-login` for both `auth` and `account`. If, and only if, that
file is missing, GAVNO falls back to the `login` service. A permission or
read error does not trigger the fallback.

PAM runs with `PAM_DISALLOW_NULL_AUTHTOK`, followed by `pam_acct_mgmt`. A
successful check fades the lock out. The session stays protocol-locked until
the fade finishes; only then are desktop pixels drawn and input restored.
Replies for a check that was in flight when the machine suspended are
discarded.

## Failed attempts

A wrong password shows an error and blocks further input for 1, 2, 4, 8 and
16 seconds, then 30 seconds for each further failure. No authentication is
attempted while input is blocked.

## While locked

- The lock screen owns the keyboard and pointer; desktop clients receive
  neither.
- Compositor binds, window gestures, the overview, resize mode, the dock and
  the top bar do not run. VT switching still works.
- The desktop is not rendered: only the lock UI and the compositor's cursor
  are visible. Desktop pixels appear only after local PAM success, during
  the fade.
- The IPC socket answers `screen` with `{"locked": true}` and rejects every
  other method with `session locked`. The agent's mutating tools fail while
  locked, and its `screen` tool returns no outputs or windows.
- logind's `LockedHint` is set for the session.

## External session locks

GAVNO implements `ext-session-lock`. A client can lock the session and draw
its own lock surfaces; GAVNO then draws only those surfaces. A lock client
that crashes or disappears does not unlock the session: the locked flag stays
set, and the screen falls back to the clear colour. Requests for a second
lock are refused, and the logind Unlock signal is ignored. An external lock
ends only when its client sends unlock-and-destroy.

## Failure and recovery

The locked flag is set before any fallible UI work. If shader or font setup
fails, the lock UI panics, or the PAM worker cannot be started, GAVNO keeps a
solid-colour lock; authentication is refused and there is no fallback to the
desktop. A panic inside the PAM worker counts as a failed check. The same
fail-closed rule applies to an external lock client that crashes. To recover,
switch to another VT and restart GAVNO; Ctrl+Alt+Delete is not handled while
locked.

## Security properties and limits

- No compositor path clears the lock without local PAM success (internal) or
  the lock client's unlock request (external).
- The password is not logged. PAM message text is neither read nor logged,
  and the conversation only answers the user name and one password prompt.
- IPC has no side effects while locked and exposes no window or output data.
- Failed attempts are throttled, but account policy and password strength are
  PAM's responsibility.
- VT switching is not blocked. The lock protects the current GAVNO session;
  anyone who can switch VT can start another session or display server.
- The lock UI can Suspend, Restart or Shut down without authentication.
