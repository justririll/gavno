# ds

A lightweight hybrid Wayland compositor in Rust on [Smithay](https://github.com/Smithay/smithay):
- Windows-style floating with snap;
- GNOME-style dynamic workspaces and overview;
- optional dwindle tiling;
- touchpad-first input;
- spring animations, with rendering only when something changes.

Primary target: Snapdragon X Elite laptops (aarch64, freedreno).

- `crates/ds`: the compositor (`ds --tty` on a VT, `ds --winit` nested).
- `design/`: interactive mockups and specs (input, agent, login).
- `agent/AGENTS.md`: global instructions for AI agents on the laptop.
- `scripts/laptop.sh`: build and run on the laptop over SSH.

The window grabs and the initial structure started from Smithay's `smallvil`
example (MIT, © Smithay contributors).
