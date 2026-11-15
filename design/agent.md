# gavno-agent: let an AI read and drive the laptop as text

Goal: an AI agent (Claude Code, Claude Desktop, any MCP client, or the local
`gavno-ai`) sees what is on screen and controls the laptop **without
screenshots**. Images are a last resort. A typical screen read should cost a
few hundred tokens, not a few thousand.

`gavno-agent` is an out-of-process extension (see the modularity notes). It talks
to GAVNO over the IPC socket and to applications over AT-SPI, and exposes an MCP
server. GAVNO itself stays small; if the agent crashes, the session does not.

## Why GAVNO can do this better than a screenshot tool

- **GAVNO is the compositor.** It knows every window, its app id, title,
  geometry, workspace, focus, minimized/fullscreen state and stacking order
  exactly, with no vision model involved.
- **GAVNO owns input.** It can deliver keys, clicks and scrolls straight to a
  client surface. It needs no uinput and no virtual keyboard, and nothing can
  leak into the wrong window.
- **GAVNO knows where windows are.** AT-SPI on Wayland only gives element
  positions relative to their window, and screen coordinates are unknown to
  apps. GAVNO adds the window's position and scale, so element refs map to exact
  screen points.

## Where the text comes from

| Source | Covers | Cost |
|---|---|---|
| GAVNO window list | every window: app, title, geometry, workspace, state | ~10 tokens per window |
| AT-SPI accessibility tree | GTK 3/4, Qt 5/6, Firefox, Chromium/Electron (with a11y forced on), LibreOffice | pruned tree, default cap 3,000 characters per read |
| terminal integration | terminals without AT-SPI: `foot`, `kitty`, `wezterm` | visible text plus the last N lines of scrollback |
| local OCR on the NPU | anything else (games, canvas apps, remote desktops) | text only, still far cheaper than an image |
| screenshot | explicit last resort | image, region-scoped and downscaled |

Requirements, set by the GAVNO session itself:
- the at-spi2-core bus runs (on Gentoo: `USE=accessibility`);
- `QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`;
- `ACCESSIBILITY_ENABLED=1` for Electron;
- Chromium started with `--force-renderer-accessibility` through a launcher
  wrapper.

### Terminals

`foot` has no accessibility support. Two routes:
1. `gavno-agent` launches terminals through a small shim that keeps a scrollback
   mirror. This is the preferred route for `foot`.
2. For `kitty` and `wezterm`, it uses their own CLIs (`kitty @ get-text`,
   `wezterm cli get-text`).

Either way, `read_window` on a terminal returns plain text, not a tree.

## Tree format (what the model reads)

Pruned to visible, meaningful nodes, one line per node, stable refs:

```
window w7 "gavno-files — ~/Projects/ds" app=gavno-files ws=1 focused 1280x760+0+30
  [e1] toolbar
    [e2] button "Back"
    [e3] entry "Path" value="~/Projects/ds"
    [e4] toggle "root" off
  [e5] list "Files" 6 items, 1 selected
    [e6] item "src" folder selected
    [e7] item "Cargo.toml" file 1.2 KB
    ...
  [e9] terminal "Terminal · ~/Projects/ds" lines=3
```

Pruning rules:
- **Dropped:** off-screen and zero-size nodes, purely decorative containers,
  and the subtrees of collapsed menus.
- **Folded:** long lists become the first 20 items plus `…N more`.
- **Masked:** password fields always read `value="••••"`.
- **Kept until the window closes:** refs (`e12`), so the model can act on them
  without re-reading.

## MCP tools

### Reading
| Tool | Returns |
|---|---|
| `screen()` | outputs, workspaces, every window (id, app, title, geometry, workspace, state), focus, the dock and top bar state. The cheapest call. |
| `read_window(id, depth?, filter?, max_chars?)` | the pruned tree (or terminal text) of one window |
| `find(query, scope?)` | elements whose name, role or value matches, across windows, with refs |
| `changes(since)` | only what changed since a previous token: new/closed windows, focus moves, tree diffs of the focused window. Use it in loops instead of re-reading. |
| `read_text(id, ocr=true)` | OCR text of a window via the local NPU model |
| `screenshot(id?, region?, scale?)` | an image. Only when text is not enough. |

### Acting
| Tool | Effect |
|---|---|
| `click(ref | x,y, button?, count?)` | the AT-SPI action when the element has one, otherwise a pointer click at the element centre |
| `type(text, ref?)` | focuses `ref` if given, then types (or sets the value through EditableText when the app supports it) |
| `key(combo)` | key chord delivered to the focused window, e.g. `ctrl+s` |
| `scroll(ref | window, dx, dy)` | smooth scroll |
| `focus(id)`, `close(id)`, `minimize(id)` | window management |
| `move(id, x, y, w?, h?)`, `snap(id, zone)` | geometry, through GAVNO's own layout so animations and anchoring rules hold |
| `workspace(n | "next" | "prev")`, `move_to_workspace(id, n)` | workspaces |
| `launch(app | desktop_id, args?, cwd?)` | starts an app, returns its window id once it maps |
| `wait_for(condition, timeout)` | e.g. a window with a title appears, an element becomes enabled. Avoids polling. |

Every action returns a short result plus a `changes` token, so the next read
can be a cheap diff.

## Safety (enforced by GAVNO, not by the model)

- **Visible while active:** when an agent session is acting, the top bar
  shows an "agent" pill and the window being driven gets a distinct border
  glow.
- **Kill switch:** `Super+Esc` stops all agent input immediately and revokes
  the session until it is reconnected.
- **Off-limits surfaces:** GAVNO never injects input into, and never reads, the
  lock screen, polkit / sudo dialogs (including gavno-files root confirmation),
  or password fields. The agent can open a dialog, but a human confirms it.
- **Per-app policy:** each app is `full` (read + act), `read` (read only) or
  `none`. Defaults: password managers and banking apps get `none`, browsers
  `read`, everything else `full`. Changes go through GAVNO settings, not through
  the agent.
- **Audit log:** every action, with time and target, goes to
  `~/.local/state/gavno/agent.log`.
- **Local first:** the MCP server listens on a Unix socket owned by the user.
  Nothing leaves the laptop unless the MCP client itself is a cloud model;
  that is the user's choice per client.

## Wiring it into Claude Code

```bash
claude mcp add gavno-agent -- gavno-agent mcp
```

`gavno-agent mcp` speaks MCP over stdio and connects to the running session's
socket (`$GAVNO_SOCK`, defaulting to `$XDG_RUNTIME_DIR/gavno/ipc.sock`). The companion
`agent/AGENTS.md` (installed globally on the laptop for Claude Code, Codex and
OpenCode) tells agents how to use the tools economically.

## Build order

1. `screen()` + window management actions over GAVNO IPC (no AT-SPI yet).
2. Input injection: `key`, `type`, `click(x,y)`, `scroll`.
3. AT-SPI reader with pruning, refs, `read_window`, `find`, `click(ref)`.
4. `changes()` diffs and `wait_for`.
5. Terminal text routes.
6. Local OCR on the NPU, `screenshot` as last resort.
7. Safety layer from day one of step 2: indicator, kill switch, off-limits
   surfaces, audit log.
