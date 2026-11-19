# Top bar controls

Click the Wi-Fi, Bluetooth or volume icon to open its built-in popover. The
Classic shell uses the same controls: open quick settings from the tray and
choose Wi-Fi, Bluetooth or the volume icon. No settings application is needed.

- **Wi-Fi:** toggle the radio, disconnect the current network, or choose a nearby
  network. Networks are deduplicated by their raw SSID and sorted by signal.
  Saved profiles activate directly. New personal networks request a masked
  password; Enter connects and Escape clears the entry. Enterprise networks need
  an existing NetworkManager profile. A scan is requested once on opening; there
  are no background scans initiated by GAVNO while the popover is closed.
- **Bluetooth:** toggle adapter power, connect or disconnect known devices, and
  use Search for up to 30 seconds of discovery. Closing the popover stops its
  discovery session. New devices use Pair, Trust and Connect with a temporary
  NoInputNoOutput agent. Passkey entry and confirmation are unsupported and show
  an inline error. Device batteries appear when BlueZ supplies them. Power
  failures explain that rfkill or permissions may prevent the operation.
- **Volume mixer:** drag the output, microphone or application volume sliders;
  click Mute beside a slider to toggle it. Choose an output to set the default
  sink and move existing application streams. Scrolling over the bar's volume
  icon changes master volume by five percentage points.

Mod+Shift+N opens Wi-Fi, Mod+Shift+B opens Bluetooth, and Mod+Shift+V opens the
mixer in either shell. Up/Down or j/k selects rows; Left/Right or h/l changes a
selected slider by five percentage points (or selects adjacent rows otherwise).
Space or Enter activates a row or toggles mute. Escape closes the popover.
Scroll to reach additional rows on small displays. Code mode displays these
popover keys in its bar hints.

A small camera icon left of the status capsule (in the Classic tray, the first
icon) shows while the [finger pointer](finger-pointer.md) camera is on: dim
while no hand is in view, accent-coloured while one is tracked. It has no
action; Mod+Shift+M turns the mode off.

NetworkManager and BlueZ run on system D-Bus. Audio uses pipewire-pulse through
`pactl` JSON queries and one long-lived `pactl subscribe` child. All queries,
commands and subscriptions run on worker threads. Queries follow service events
or user actions; no periodic panel refresh timer exists. Unchanged snapshots
produce no redraw, and changes to invisible list rows do not redraw the closed
popover. No operation plays a sound. No additional dependency or configuration
key is required.

For safe development, launch a private nested compositor with
`GAVNO_PANEL_FAKE=1 GAVNO_DEBUG_INPUT=1`. Every panel backend then uses fixtures
and in-memory changes only; the legacy bar status worker is also disabled in
this mode. Never exercise live radio, pairing or mixer actions on a build host.

If the audio subscription is interrupted by a server restart, reopening the
mixer retries the subscription. There is no background retry timer.
