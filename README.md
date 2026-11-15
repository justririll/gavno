# ds

A lightweight hybrid Wayland compositor in Rust on [Smithay](https://github.com/Smithay/smithay):
- Windows-style floating with snap;
- GNOME-style dynamic workspaces and overview;
- optional dwindle tiling;
- touchpad-first input;
- spring animations, with rendering only when something changes.

Primary target: Snapdragon X Elite laptops (aarch64, freedreno).

- `crates/ds`: the compositor (`ds --tty` on a VT, `ds --winit` nested).
- `crates/ds-agent`: the window management MCP server (`ds-agent mcp`, stdio).
- `design/`: interactive mockups and specs (input, agent, login).
- `agent/AGENTS.md`: global instructions for AI agents on the laptop.
- `scripts/laptop.sh`: build and run on the laptop over SSH.

The window grabs and the initial structure started from Smithay's `smallvil`
example (MIT, © Smithay contributors).

## Window management IPC

ds listens on `$XDG_RUNTIME_DIR/ds/ipc.sock` and exports `DS_SOCK` to its
children. The socket directory is private (0700). Requests and replies are
one JSON object per line, with a numeric request `id`:

```json
{"id":1,"method":"screen","params":{}}
{"id":2,"method":"focus","params":{"id":3}}
{"id":3,"method":"move","params":{"id":3,"x":100,"y":80,"w":820,"h":560}}
```

Methods are `screen`, `focus`, `close`, `move` and `raise`. Window ids are
stable for each toplevel's lifetime and never reused during a ds run.
`screen` returns outputs and mapped windows, topmost first; geometry is in
logical pixels. `raise` preserves keyboard focus. Width and height are
optional for `move`; resizing asks the client to configure the new size.
Replies contain either `result` or an `error` string, plus the request id.
Validated action requests, including unknown window ids, are appended to
`~/.local/state/ds/agent.log`; reads are not logged. Actions fail if the audit
log cannot be written.

Register the MCP server with `claude mcp add ds-agent -- ds-agent mcp`.
It connects via `DS_SOCK`, falling back to `$XDG_RUNTIME_DIR/ds/ipc.sock`,
and exposes the same five tools. Window tool arguments use numeric ids;
compact screen output labels them `w3`, `w4`, etc. If ds is unavailable,
tool calls return an error while the MCP server stays running.
