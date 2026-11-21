# TTY mode

A terminals-only look on the GAVNO shell: a permanent status line on top, no
mouse, and every workspace tiled like tmux panes. Black, monospace, keyboard first,
and as light as the rest of GAVNO: nothing runs while the screen is settled.

It is a session flag on the GAVNO shell, like [code mode](keybinds.md#code-mode),
not a third shell mode. Classic never enters it; switching to Classic leaves it
and it resumes when the GAVNO shell returns. Gesture strip: TTY - GAVNO - Classic.

## Turning it on

- Mod+Shift+T, the **TTY** chip in the quick settings, or Super and a three-finger
  swipe down in GAVNO (swipe up returns); see [keybinds.md](keybinds.md#tty-mode).
- `gavno msg tty_mode` toggles; `gavno msg tty_mode '{"enable":true}'` sets it.
- `[tty] start = true` forces it at start; otherwise the last mode is remembered,
  see [config.md](config.md#start-mode).

Entering and leaving is one transition, like the Classic switch: the bar and the
dock slide away, the wallpaper sinks into the console black and the status line
slides in from the top. A Super swipe drags it under the fingers and springs
back if released early; Mod+Shift+T plays it on its own. The mode itself (tiling,
no mouse) switches at once. The transition costs frames only while it moves.

## Its own windows

TTY mode is a world of its own. Entering it hides the GUI windows of the
monitor and shows its consoles; leaving it hides the consoles and brings the
GUI windows back exactly as they were. Terminals opened in TTY mode never
show up in GAVNO or Classic (not in the dock or Alt+Tab either). Any other
app started from a console (Firefox from a shell, say) opens in the GUI world
and is waiting there when you leave TTY mode.

## Several monitors

The mode belongs to one monitor: Mod+Shift+T, the swipe and the chip act on
the monitor under the pointer (Mod+. and Mod+, move there from the keyboard).
One monitor can be a console while another stays GAVNO or Classic. Each
monitor's mode is remembered by connector name in `$XDG_STATE_HOME/gavno/mode`
and comes back when that monitor is plugged in again. IPC `tty_mode` and
`shell_mode` take an optional `"output": "HDMI-A-1"`, which switches that
monitor at once, without the animation.

## The status line

28 logical pixels on top, reserved from the work area, in the `[tty] font` family:

- **Left:** workspace tabs, `1 fish  2 nvim  3 ... 10`; workspaces 1-10 always exist. The active tab is a pill in the
  accent colour with dark text and slides to the new tab on a switch (a spring that
  settles to zero cost). Other tabs are dim; an empty workspace shows only its
  number, fainter. The label is the workspace name when it has one (see the AI
  workspace names), else the focused window's app name.
- **Centre:** the focused window title, dim, cut with an ellipsis.
- **Right:** Wi-Fi strength, volume, battery and the date and time, each with a small
  icon and separated by hairlines, from the same sources as the bar. The battery
  turns amber at 20 % and red at 10 %, green while charging.
- A 1 px hairline closes it at the bottom; the background is a touch lighter than
  the console. It redraws when its content changes; the clock is one one-shot timer
  aligned to the next minute, never a per-second tick.

## Panes

Windows tile with no gaps except a 1 px divider; the focused pane has an accent
frame, the others a faint one. No rounded corners, shadows or decorations.
Mod+Enter opens a terminal as its own window next to the focused one, Mod+Q closes
it, Mod+H/J/K/L move focus, Mod+Shift+1..9 and 0 sends it to a workspace.
**Mod+L does not lock here**: use Ctrl+Alt+L. Alt+F1 ... Alt+F10 (and Mod+1 ... 9, 0)
switch workspaces, with a 120 ms dip through black instead of a slide.

Terminals are bare: GAVNO draws no title bar, shadow, rounded corners or focus ring
for any window in this mode, only the 1 px pane frame. The `footclient` it starts
(`footclient -o csd.preferred=none -o pad=8x6`) draws no client-side decorations
either and keeps 8x6 px of padding so text does not touch the divider.

An empty workspace starts a terminal (`[tty] autostart_terminal`): `footclient`
when a foot server socket exists, else the configured `terminal`. It happens only
on a switch into the empty workspace, never from a refresh, and never twice within
two seconds, so a terminal that fails is not started in a loop. Meanwhile you see a
dim `Mod+Enter  terminal` hint on the background, which is `#0a0a0c` with a static
vignette.

## No mouse

The cursor is never drawn and pointer motion and buttons are dropped before
anything sees them: no clicks, hover or focus-follows-mouse. Two-finger scroll still
reaches the focused window (the invisible pointer sits on it), so terminal scrollback
works, and 3/4-finger gestures keep working. Leaving restores the pointer.

## Also restyled

- **Notifications** are kernel messages, up to three lines at the top right under
  the status line, `[ 1234.567] Telegram: Alice: text`, gone after 4 s.
- **Launcher** is a command line at the bottom: `$ query` with the matches above
  it, the selected one in the accent colour.
- **Lock screen** is a console login (`host login: user`, a silent `Password:`,
  `Login incorrect`); authentication is the regular one.
- The overview and quick settings popovers are not available.

## Cost

Settled, TTY mode arms one timer (the minute clock) and requests no frame. The
living theme, the bar clock and the bar are not running while it is on.
