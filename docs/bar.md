# Top bar status cluster and quick settings

Keyboard layout, Wi-Fi, volume, Bluetooth and battery share one capsule at the
right end of the bar. It has one hover highlight and one click target: a click
anywhere on it opens quick settings (the layout label keeps its own click,
which switches the keyboard layout). Scrolling over the capsule changes the
volume by five points. The battery slot shows an outlined cell with its fill
level, the percentage and a bolt while charging; on a machine without a
battery (a desktop, a nested session) it shows only a plug. Bluetooth device
batteries are not in the bar; they appear in the Bluetooth page.

While the webcam finger pointer runs (Mod+Shift+M), a camera icon leads the cluster: dim until a hand is in view, accent coloured while it tracks. The Classic taskbar keeps its own indicator.

In the Classic shell the quick settings entry of the tray opens the same panel.

## Quick settings

One 360 px panel, anchored under the cluster (above the taskbar in Classic). It
grows from the cluster (scale 0.96 to 1 and fade, 120 ms); pages slide
horizontally (160 ms). Nothing animates, and no timer runs, while it is settled.

- **Tiles:** Wi-Fi (subtitle: SSID or Off), Bluetooth (connected device or
  Off), Night light (`display.night_light`, applied at once, not written to the
  config file), Do not disturb (silences notification popups, history is kept)
  and Code mode (hidden in Classic). A click toggles; the chevron of Wi-Fi and
  Bluetooth opens their page.
- **Sliders:** volume (the icon mutes, the chevron opens the mixer) and
  brightness (backlight, or the software dimming when that is configured).
- **Footer:** battery percentage with the time estimate when the firmware
  reports a rate, the power-profiles-daemon profile when it runs, and buttons
  for Settings (`classic.settings_command`, `gavno-settings` by default), Lock
  and a Power page (lock, suspend, log out, restart, power off).
- **Media:** while a player is active its title and the previous, play and next
  buttons appear at the top of the panel.

### Pages

- **Wi-Fi:** the title and the radio switch share one row. The connected
  network comes first, highlighted with a check mark and a Disconnect button;
  then Saved and Nearby networks. Signal is drawn as one to four bars and
  secured networks carry a lock. Saved profiles activate directly. New personal
  networks request a masked password; Enter connects and Escape clears the
  entry. Enterprise networks need an existing NetworkManager profile. A scan is
  requested once when the page opens; GAVNO starts no background scans.
- **Bluetooth:** the title and adapter switch, a Search row (up to 30 seconds of
  discovery, which stops when the page closes) and the Connected, Paired and
  Found sections. Device batteries are shown in the row when BlueZ supplies
  them. New devices use Pair, Trust and Connect with a temporary
  NoInputNoOutput agent; passkey entry and confirmation are unsupported and
  show an inline error.
- **Sound:** Output (slider and the list of output devices, with speaker,
  headphone, Bluetooth and display icons), Input (slider with the device name)
  and Apps (one slider per application stream). Application icons come from the
  stream's icon name or, when it has none, from the desktop entry matching its
  application id or binary; the lookup runs on the audio worker thread.

Escape or a click outside closes the panel. The back arrow or Backspace returns
from a page. Mod+Shift+N, Mod+Shift+B and Mod+Shift+V open the panel directly on
the Wi-Fi, Bluetooth or sound page. Up/Down or j/k selects, Left/Right or h/l
adjusts a slider, Enter toggles or activates, Space opens the page behind a
tile or the volume slider. The key hint footer is shown in code mode, or after
the panel was operated with the keyboard.

A small camera icon left of the status capsule (in the Classic tray, the first
icon) shows while the [finger pointer](finger-pointer.md) camera is on: dim
while no hand is in view, accent-coloured while one is tracked. It has no
action; Mod+Shift+M turns the mode off.

NetworkManager and BlueZ run on system D-Bus. Audio uses pipewire-pulse through
`pactl` JSON queries and one long-lived `pactl subscribe` child. All queries,
commands and subscriptions run on worker threads. Queries follow service events
or user actions; no periodic panel refresh timer exists. Unchanged snapshots
produce no redraw. No operation plays a sound. No additional dependency or
configuration key is required.

For safe development, launch a private nested compositor with
`GAVNO_PANEL_FAKE=1 GAVNO_DEBUG_INPUT=1`. Every panel backend then uses fixtures
and in-memory changes only, and the bar gets a fixture battery, backlight and
power profile (`GAVNO_PANEL_FAKE_BATTERY=none` or `low` changes the battery);
the legacy bar status worker is disabled in this mode. Never exercise live
radio, pairing or mixer actions on a build host.

If the audio subscription is interrupted by a server restart, reopening the
panel retries the subscription. There is no background retry timer.
