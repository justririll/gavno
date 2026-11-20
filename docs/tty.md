# TTY mode

A third look for the shell besides GAVNO (GNOME-like) and Classic (KDE-like): the
Linux text console, but beautiful. Black, monospace, no chrome, keyboard first,
and as light as the rest of GAVNO: nothing runs while the screen is settled.

It is a session flag on the GAVNO shell, like [code mode](keybinds.md#code-mode),
not a third shell mode. Classic never enters it; switching to Classic leaves it
and it resumes when the GAVNO shell returns.

## Turning it on

- Mod+Shift+T, or the **TTY** chip in the quick settings next to the Code mode chip.
- `gavno msg tty_mode` toggles; `gavno msg tty_mode '{"enable":true}'` sets it.
- `[tty] start = true` starts in it; see [config.md](config.md#tty-mode).

Entering plays a CRT power-on: the screen grows out of a bright horizontal line
in 200 ms. Leaving plays it backwards and ends the mode when it is over. The
animation costs frames only while it runs.

## What changes

- **No chrome.** No bar, dock, window decorations, shadows, rounded corners,
  focus ring, wallpaper or living-theme background. The gap is 0. The background
  is `#0a0a0c` with a very subtle radial vignette, rendered once.
- **Consoles are workspaces in monocle layout.** Each workspace shows its
  focused window over the whole output; the other windows of that console stay
  behind it and are not drawn. Mod+J / Mod+K (aliases of Mod+Down / Mod+Up, with
  the usual Ctrl, Shift and Alt variants) and the other focus binds cycle the
  windows of a console; H and L are not aliased, so Mod+L still locks. Every workspace's previous floating/tiling state comes
  back when the mode ends.
- **Alt+F1 ... Alt+F6** switch to console 1-6 (only while the mode is on), next
  to Mod+1 ... Mod+9. A console that does not exist yet means the last one.
  Ctrl+Alt+Fn remains real VT switching. Switching does not slide: the console
  changes at once and a 120 ms dip through black fades in over it.
- **Empty console.** A workspace without windows shows, top left in monospace:

  ```
  GAVNO 0.1.0 vivobook tty3

  vivobook login: kirill
  [kirill@vivobook ~]$ █
  ```

  The host name comes from `/etc/hostname`, the user from `$USER`. The block
  cursor blinks every 530 ms, and the timer exists only while an empty console is
  on screen.
- **Terminal autostart.** With `[tty] autostart_terminal = true` (the default),
  entering the mode on an empty console or switching to one starts the configured
  `terminal` once. Nothing restarts it: a terminal that fails or is closed again
  is started only by the next switch into that empty console, and never twice for
  one console within two seconds.
- **Status line on demand.** There is no permanent bar. While Mod is held for more
  than 150 ms, and for 1.5 s after a console switch, one line shows at the bottom:
  `tty2  foot` on the left (console number in the accent colour, app id of the
  focused window) and `wifi 87%  bat 64%  14:32` on the right, from the same
  status sources as the bar (a trailing `+` after the battery means charging;
  `eth` and `net off` replace Wi-Fi when wired or offline). It fades in and out
  in 80 ms. Pressing another key while Mod is held (a chord) does not bring it up.
  The status worker only runs while the line is visible.
- **Notifications** are kernel messages at the top left, at most three lines,
  `[ 1234.567] Telegram: Alice: message text`, each gone after 4 s (the time is
  seconds since the compositor started). Do not disturb and the history still work.
- **Launcher** is a command line at the bottom: `$ query` with the matches above
  it as plain lines and the selected one in the accent colour. The ranking,
  frecency, calculator, files and AI row are the regular launcher.
- **Lock screen** is a console login: `host login: user`, a silent `Password:`
  prompt (nothing is echoed, no dots) and `Login incorrect` on failure. The
  authentication is the regular one.
- **Pointer** hides at the first key press that reaches a client or a bind, like
  code mode, and returns on the next pointer event.
- The overview and the quick settings popovers are not available on a console.

## Font

`[tty] font` is a fontconfig family (default `monospace`), used for every TTY
text. It is resolved once with `fc-match` when the first text is drawn and falls
back to Liberation Mono or Noto Sans Mono. The text has no glow: that would cost
a blurred second pass, and the console should stay free when nothing changes.

## Cost

When settled, TTY mode arms no timer and requests no frame. The only timers are
the 150 ms Mod-hold timer, the 1.5 s status linger, the 200 ms end of the
power-off, and the cursor blink on a visible empty console; each exists only
while its thing is on screen. The living theme, the bar clock and the bar status
worker are not running while the mode is on.
