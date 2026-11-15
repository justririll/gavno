# Global instructions for AI agents on this laptop

This file is for any coding agent on this machine: Codex, OpenCode, Claude
Code and others. It is installed once as `~/.config/agents/AGENTS.md`, and each
tool's global instruction file points to it. Its source lives in the ds
repository at `agent/AGENTS.md`; edit it there.

## The machine

- ASUS Vivobook S15, Snapdragon X Elite (x1e80100, aarch64, 12 cores, 30 GB RAM).
- Gentoo Linux with systemd. The host name is `vivobook`.
- The desktop user is `kirill`. The user does sudo and polkit themselves:
  - never type, guess or store passwords;
  - never change system or security settings unless the user asked you to.
- GPU: Adreno X1-85 via freedreno (Mesa). Display: msm DPU, `/dev/dri/card1`.
  The internal panel is eDP-1, 2880x1620 at 120 Hz, scale 1.75.
- The desktop is **ds**, the user's own Wayland compositor, still in
  development. Its sources are in `~/Projects/ds`:
  - it is built natively here with `cargo build`;
  - `scripts/laptop.sh` drives it from the user's PC;
  - a dev instance runs as the systemd unit `ds-dev` on VT 7;
  - do not stop or restart a running ds session unless the user asks: it
    holds their open windows.

## Hard rules for the NPU (Hexagon CDSP)

The details are in `~/.config/agents/NPU_GUIDE.md`. Section 3 has the
current Gentoo setup for LLMs:
- `llama-npu.service`;
- `/root/llm/npu-env.sh`;
- `npu-reset.service`.

Parts that still mention NixOS describe the old install, so check those paths
before you rely on them. In short:

1. **Never kill a process that is inside an NPU call.** No `timeout`, no
   `SIGKILL`. An interrupted FastRPC call leaks kernel buffers and often needs
   a reboot. Stop NPU daemons only with SIGTERM (`systemctl stop`).
2. **Never read `/proc` of a process that may be stuck in the FastRPC driver.**
   That means no `pgrep -f`, no `ps` with wchan, no scanning `/proc/*/fd`: it
   has locked the CPUs before. To find processes, list one unit's cgroup
   (`/sys/fs/cgroup/.../cgroup.procs`) or use `systemctl status <unit>`.
3. **Only one owner of the NPU at a time.** Stop the other NPU services before
   running tests.
4. **llama.cpp on the Hexagon backend needs `DSPQUEUE_MONITOR=1`.** Without
   it the DSP silently never answers. Big models also need
   `GGML_HEXAGON_DEVICES=2`.
5. **Firmware:**
   - do not swap `cdsp.mbn` casually;
   - unsigned modules need the ASUS Windows-driver firmware;
   - restarting the CDSP through remoteproc is fine.

## Builds and load

- Run builds with `nice -n 19 ionice -c3` and a limited `-j` (8 of 12 cores),
  so the desktop stays smooth.
- Do not delete user data. Prefer moving to trash over `rm` for anything you
  did not create yourself.

## Reading the screen and driving the desktop (ds-agent)

ds will ship `ds-agent`, an MCP server that exposes the screen as text and
lets you control the desktop. Until the `ds-agent` binary exists:

- you cannot see the screen;
- ask the user, or read logs (`journalctl -u ds-dev`).

Once it exists, register it in your tool:

- Claude Code: `claude mcp add ds-agent -- ds-agent mcp`
- Codex: add to `~/.codex/config.toml`:

  ```toml
  [mcp_servers.ds-agent]
  command = "ds-agent"
  args = ["mcp"]
  ```

- OpenCode: in `~/.config/opencode/opencode.json`:

  ```json
  {"mcp": {"ds-agent": {"type": "local", "command": ["ds-agent", "mcp"]}}}
  ```

Use it instead of screenshots: text reads cost a few hundred tokens, while
images cost thousands.

### How to look

1. Start with `screen()`. It lists every window with id, app, title,
   geometry, workspace and focus. Often that alone answers the question.
2. To read one window, call `read_window(id)`.
   - You get a pruned accessibility tree with refs like `[e12] button "Save"`.
   - Terminals come back as plain text.
   - Narrow the result with `filter` (e.g. `"button,entry"`) or `max_chars`.
3. To search across windows, use `find("Save")` instead of reading everything.
4. In loops, call `changes(since=<token>)` instead of reading again. Every
   tool result includes a fresh token.
5. For apps that expose no tree, use `read_text(id)` (local OCR).
6. Call `screenshot(...)` only when layout or visuals really matter. Scope it
   to a region and downscale it.

### How to act

- Prefer refs: `click("e12")`, `type("hello", ref="e3")`.
- Use `key("ctrl+s")` for shortcuts.
- Start apps with `launch("foot", cwd="~/Projects/ds")`.
- For window management, use the ds tools (`snap`, `move`, `workspace`,
  `minimize`), not simulated shortcuts.
- After an action, use `wait_for(...)` instead of sleeping or polling.

### Limits ds enforces

- You cannot read or type into password fields, the lock screen, or
  polkit/sudo dialogs. If an action needs root or a password, open the dialog
  and ask the user to confirm it. Never try to work around this.
- A per-app policy may make an app read-only (`policy: read`) or hidden from
  you (`policy: none`). If you hit one, tell the user and stop.
- The user can press `Super+Esc` at any time to stop you. If your session is
  revoked, do not reconnect on your own: say what you were doing and wait.
- Every action is logged to `~/.local/state/ds/agent.log`.
- Before anything destructive or outward-facing, confirm with the user. This
  covers deleting files, sending messages, submitting forms and purchases.

### Token habits

- One `screen()` and one targeted `read_window` is usually enough.
- Don't re-read a window you just acted on. Use the returned `changes` token.
- Quote only what matters from a tree. Never dump whole trees into a reply.
