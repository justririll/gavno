# Finger pointer

Point at the screen with your index finger in front of the laptop webcam and
the cursor follows; pinches click. It is off by default and costs nothing while
off: no process, thread, descriptor or timer exists.

Press **Mod+Shift+M** to turn it on or off (Mod is Super on the TTY, Alt when
nested). A camera icon in the top bar (the Classic tray) shows while it runs.
Disable the feature entirely with `[pointer] camera = false`
([configuration](config.md#finger-pointer)).

## Using it

The camera image is mirrored like a mirror. The index fingertip inside the
central 70% x 70% of the frame (the *active box*, `[pointer] active_box`) maps
linearly to the whole screen, so the edges and corners are reachable without
stretching your arm. Pointing is absolute: where the finger is, the cursor is.
Keep the hand 30-60 cm from the camera with good light, palm facing it.

| Gesture | Effect |
| --- | --- |
| Index finger out | Move the cursor. |
| Thumb tip + index tip pinch | Left button. Hold the pinch to drag. |
| Thumb tip + middle tip pinch | Right button. |
| Index and middle fingers up together (ring and little finger folded), moving up or down | Scroll. Fingers up scrolls down, like a touchpad. |
| Fist | Pause: the cursor stays where it is. Open the hand to continue; the cursor glides back to the fingertip. |

Details that make it usable:

- While you pinch, the cursor follows the base knuckle of the index finger
  instead of its tip, so closing the fingers does not shove the cursor. The
  cursor also stays put for 120 ms after a pinch starts, and the click lands at
  the position the finger had a moment before the pinch.
- Pinch distances are measured relative to your hand size and have hysteresis
  (press below 30% of the hand size, release above 45%).
- A One-Euro filter (`min_cutoff`, `beta`) removes jitter while the finger is
  slow and adds little lag while it moves, and moves under 2 px are ignored.
- Losing the hand for a quarter of a second releases any held button and dims
  the camera icon. With no hand for 60 seconds the helper exits and the mode
  turns itself off.
- The mode refuses to start, and stops, while the session is locked or the
  greeter runs; clicks and motion are ignored while an authorization (polkit)
  dialog is visible.

## Privacy

- The camera is on only while the mode is on, which is exactly while the camera
  icon is in the bar. Turning the mode off, locking the session, disabling
  `[pointer] camera` or quitting the compositor ends the helper process, which
  releases the camera.
- Frames are processed in memory by the helper and never saved, logged or sent
  anywhere. The only output is pointer events over a private socket pair the
  compositor created for its own child. The helper does not touch the network.
- While no hand is visible the camera branch drops to 6 frames per second.

## How it works

The compositor starts `gavno-cam` (installed next to `gavno`) with a private
socket as its standard input and reads it with an event-loop source, so the
compositor thread never waits on the camera. The helper captures 640 px wide
RGB frames from `libcamerasrc` (falling back to `pipewiresrc`) with
GStreamer, runs MediaPipe's palm detector only while no hand is tracked and
the 21-point landmark network on the tracked region every frame (tract, CPU
only), and turns landmarks into the line protocol of
`crates/gavno-cam-proto/src/lib.rs`: pointer position as 0..1 fractions of the
active output, button and scroll events, and hand present or lost. Messages
enter the same synthetic input path the [debug IPC](debugging.md) uses, with
no environment gate; the gate is the private socket. Held buttons are released
when the mode ends.

The helper exits on SIGTERM and when the compositor dies. The model files and
their licenses are described in `crates/gavno-cam/MODELS.md`; they install to
`/usr/local/share/gavno/models/` (override with `GAVNO_CAM_MODELS`).

## Troubleshooting

- Nothing happens on Mod+Shift+M: check the compositor log for
  `finger pointer not started` (helper or models missing, camera disabled in
  the config, session locked).
- `gavno-cam --probe 10` prints only frame rate, inference time, hand
  confidence and the fingertip position for ten seconds, without moving the
  cursor. Frames are never stored.
- `GAVNO_CAM_PIPELINE` replaces the capture pipeline for debugging; it must end
  in `appsink name=sink`.
