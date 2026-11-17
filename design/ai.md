# Local AI for GAVNO

Status: design, not built. Nothing here is installed or downloaded until the user
approves the "Approval needed" list at the end.

## Goal

Small, local, on-demand language help inside the shell. Nothing leaves the laptop.

- **Natural-language commands in the launcher**: "move firefox to the second
  workspace", "play something calm", "open the downloads folder".
- **Workspace names**: short names for unnamed workspaces from the windows on them.
- **Later**: suggested window rules ("Telegram always goes to workspace 3").

Not wanted (decided 2026-11-15): screen Q&A or a chat window. The user uses
ChatGPT or Claude directly for that.

Hard rules come from the user's priorities:

- **Instant UI.** The compositor never waits on the model. A command shows a preview
  first and runs only on Enter.
- **Zero idle cost.** No model in RAM and no process running while the feature is unused.
- **NPU safety.** See "NPU rules" below.

## What already exists on the laptop

Nothing needs downloading. Everything below is under `/root/llm` and `/root/npu`.

**llama.cpp**
- Build b11517 in `/root/llm/llama/llama-b11517`, with the Hexagon backend
  (`libggml-hexagon.so`, `libggml-htp-v73.so`).
- `llama-server`, `llama-cli` and `llama-bench`.

**Models (GGUF)**
- GigaChat3.1-10B-A1.8B: Q4_0 6.1 GB, Q8_0 11.4 GB. A Russian-native MoE with
  1.8 B active parameters.
- Qwen3-4B-Instruct-2507 Q4_0, 2.4 GB.
- YandexGPT-5-Lite-8B: Q4_0 and Q8_0.
- Gemma-4-E4B Q4_0.
- Qwen2.5-Coder-7B Q4_0.
- Qwen3-Coder-30B-A3B Q4_K_XL.

**systemd system units**
- All are `static`, inactive by default, run as root and listen on 127.0.0.1.
- All use `KillSignal=SIGINT` and `TimeoutStopSec=30`.

| Unit | Model | Device | Port |
|---|---|---|---|
| `llama-ru-giga-cpu` | GigaChat 10B-A1.8B Q4_0 | CPU, `-t 10`, ctx 16384 | 8086 |
| `llama-ru-giga` | GigaChat 10B-A1.8B Q4_0 | NPU (HTP0,HTP1) | 8084 |
| `llama-npu` | Qwen2.5-Coder-7B Q4_0 | NPU | 8081 |
| `llama-coder` | Qwen3-Coder-30B-A3B | CPU | 8082 |

- The NPU units set `DSPQUEUE_MONITOR=1`, `GGML_HEXAGON_DEVICES=2`, the FastRPC
  library paths, and `WorkingDirectory=/root/npu/dsp/cdsp`, where the DSP shells live.
- They conflict with each other, so only one NPU model runs at a time.

**Access and recovery**
- polkit rule `50-local-llm.rules`: users in `wheel` may start and stop the `llama-*`
  units and `npu-reset` without a password.
- `npu-reset.service` restarts the CDSP to drop stale DSP mappings.

### Measured speed (the user's `npubench.log`)

`pp` is prompt tokens/s and `tg` is generated tokens/s.

| Model | NPU pp | NPU tg | CPU pp | CPU tg |
|---|---|---|---|---|
| Qwen2.5-Coder-7B Q4_0 | 373 | 5.7 | n/a | n/a |
| YandexGPT-5-Lite-8B Q4_0 | 344 | 4.7 | 162 | 22.5 |
| GigaChat3.1-10B-A1.8B Q4_0 | 175 | 10 | 145-180 | 52-55 |

Our requests have a short prompt (most of it cached) and a short answer of about
20–60 tokens, so generation speed decides the latency. With GigaChat:

- **CPU:** a 40-token answer takes about 0.8 s;
- **NPU:** the same answer takes about 4 s.

The NPU wins only on long prompts with dense models, and none of the v1 features
have long prompts.

**Decision for v1:** the CPU runs the interactive features. The NPU stays an opt-in
backend (`backend = "npu"`) for background jobs, once energy per token has been
measured. This revises the "NPU first" note from 2026-11-15, because the measurements
contradict it for this workload.

## Architecture

```
launcher (GAVNO, compositor thread)
   | worker thread, calloop channel back; never blocks the compositor
   v
gavno-ai  (separate binary, session bus org.gavno.AI1, D-Bus activated, exits when idle)
   | systemd D-Bus StartUnit/StopUnit (system bus; polkit lets wheel do it)
   | HTTP to 127.0.0.1:<port> (/health, /v1/chat/completions)
   v
llama-server  (an existing or new system unit, see "Approval needed")
```

### Why a separate binary

The compositor gains no HTTP client, no JSON-schema code and no new failure modes.
A crash or hang in the AI path cannot touch the desktop.

gavno-ai is a small Rust crate (`crates/gavno-ai`) using zbus and ureq, both blocking
and on their own thread. Its D-Bus `.service` file lets the bus start it on the first
call.

### D-Bus interface `org.gavno.AI1` (`/org/gavno/AI1`)

| Method | Purpose |
|---|---|
| `Prewarm()` | Start the model unit if it is not running. Returns at once. |
| `Interpret(query: s, context: s) -> s` | Context and result are JSON (below). |
| `NameWorkspace(windows: s) -> s` | Returns `{"name": "..."}`, or an error when the model is not already running. |
| `Status() -> a{sv}` | `running`, `backend`, `model`, `last_latency_ms`. |

### Request path (`Interpret`)

1. **GAVNO builds the context.** It is small JSON:
   - windows: id, app_id, title cut to 60 characters, workspace;
   - workspaces: index and name;
   - the top 150 installed apps by frecency: desktop id and name;
   - the music state;
   - the action list (below).
2. **gavno-ai prepares the model.**
   - It starts the unit if needed and waits for `/health`.
   - It sends a fixed system prompt, so llama-server's prompt cache keeps that prefix
     warm, then the context and the query.
   - It uses `temperature 0`, `max_tokens 96`, and `response_format` with a
     `json_schema` for the action list, so the output is always valid JSON of the
     right shape.
3. **gavno-ai validates the result.**
   - Every action must be in the whitelist.
   - Every id must exist in the context, and every argument must be in range.
   - If anything fails, it returns `{"error": "..."}`, never a guessed action.
4. **GAVNO shows the result.**
   - The launcher shows one preview row, e.g. "Move Firefox → workspace 2".
   - Enter runs it through the same internal functions as keybinds. Esc drops it.
   - Nothing runs without the preview.

### Action whitelist (v1)

| Action | Arguments |
|---|---|
| `launch` | desktop id |
| `focus` | window id |
| `move_to_workspace` | window id, workspace index (1–10) |
| `switch_workspace` | workspace index |
| `name_workspace` | workspace index, name (≤ 24 chars) |
| `music` | `play` with a query string, or `pause`, `next`, `previous` |
| `open_folder` | a path under `$HOME` that exists |
| `set` | a fixed allow-list of live config keys and ranges: brightness, night light on/off, DND |

There is no shell command, no file deletion, no window close, no lock-screen,
polkit or password surface, and no network.

### When the launcher asks the model

- **When a query is treated as a command.** The launcher keeps its normal instant
  search. A query becomes a command candidate when:
  - it has at least 2 words and nothing matches strongly (top score below a
    threshold); or
  - the user presses Tab on the "Ask AI" row, which appears as the last row.
- **Prewarm.** The first keystroke that makes a query a candidate calls `Prewarm()`.
  While the user finishes typing, the model loads: about 0.5 s from the page cache,
  2–3 s cold.
- **Latency target.** With the model warm, Enter to preview row should take under 1.5 s.
  The row shows a spinner until then.
- **Idle exit.** gavno-ai stops the unit gracefully after `idle_stop_minutes` without a
  request (default 10), then exits. The model's pages stay in the page cache, which the
  kernel can reclaim, so the next start is warm.

### Workspace names

- Cost-free heuristic in GAVNO, always on:
  - the dominant app's name ("Firefox", "Terminal");
  - a small category table for mixes ("Code" for editor + terminal, "Chat" for
    messengers).
- **AI refinement:** gavno-ai refines a heuristic name only when the model is already
  running for another reason. The workspace's window set must also have been stable
  for 30 s. Naming alone never starts a model.
- **User names win:** a name the user set is never replaced.

## NPU rules (only for `backend = "npu"`)

The user found these the hard way:

- **Never SIGKILL** an NPU llama process, and never kill one while it is inside a DSP
  call. SIGKILL leaks DSP mappings, and recovery then needs `npu-reset`.
  - gavno-ai stops units only through systemd (SIGINT).
  - It stops a unit only when no request is in flight.
  - Today the units have `TimeoutStopSec=30` with the default `SendSIGKILL=yes`, so
    the approval list asks for `SendSIGKILL=no` on them.
- **Client timeouts never kill.** gavno-ai stops waiting and closes the HTTP
  connection, but leaves the server alone. `max_tokens` bounds the work left.
- **No `/proc` reads** of NPU processes, which can block while they are in a FastRPC
  call. Health comes only from HTTP `/health` and the systemd unit state.
- **One NPU model at a time.** The units already declare `Conflicts=`. gavno-ai never
  starts an NPU unit while the user runs another one.
- **Recovery stays manual.** gavno-ai never runs `npu-reset` on its own. It reports
  "NPU not answering" and leaves recovery to the user.

## Config

```toml
[ai]
enabled = false            # nothing starts until this is true
backend = "cpu"            # "cpu" or "npu"
idle_stop_minutes = 10
workspace_names = true     # heuristic always; AI refinement only while the model runs
```

## Measurements before building

These are CPU only and need the user's OK, because they run a model.

1. Latency from Enter to the parsed action, for 10 sample commands (Russian and
   English), at `-t 6` and `-t 10`:
   - GigaChat3.1-10B-A1.8B Q4_0 vs Qwen3-4B-Instruct-2507 Q4_0;
   - with and without the cached system prompt.
2. GAVNO `gavno::stall` warnings and the input latency during a request, so the
   model must not make the desktop stutter. If it does, use fewer threads or a
   `CPUWeight=` limit on the unit.
3. RSS and KV memory at ctx 4096, with an f16 vs a q8_0 KV cache.
4. Later, for the NPU backend: energy per answer from `power_now`, on the NPU vs the
   CPU, on battery.

## Build plan (codex, after approval)

1. **`crates/gavno-ai`.** D-Bus service, systemd unit control, the HTTP client, the
   schema and validation, and idle stop.
   - Tests against a fake llama-server that returns canned JSON;
   - whitelist rejection tests: unknown ids, out-of-range values, extra fields.
2. **Launcher integration in GAVNO.**
   - Candidate detection, the "Ask AI" row, prewarm, the preview row.
   - An action executor that maps onto existing functions.
   - A worker thread for D-Bus with a calloop channel back.
   - Tests: nothing runs without Enter, and an error shows as a row.
3. **Workspace-name heuristic in GAVNO**, then the AI refinement hook.
4. **Docs:** `docs/config.md` `[ai]`, and a short section in `docs/debugging.md`.

## Approval needed (nothing done yet)

1. **No downloads.** The models and llama.cpp are already on the laptop.
2. **New unit `llama-gavno.service`** for the interactive path:
   - a copy of `llama-ru-giga-cpu` with `-t 6 -c 4096 --cache-type-k q8_0
     --cache-type-v q8_0`, `Nice=10` and `CPUWeight=20`, on port 8090;
   - the polkit regex in `50-local-llm.rules` gains `gavno`.

   Without it, gavno-ai uses `llama-ru-giga-cpu` as is: 10 threads, ctx 16384, more
   RAM, and more risk of desktop stutter.
3. **`SendSIGKILL=no`** on `llama-npu`, `llama-ru-giga` and `llama-ru-yandex`.
   This is only needed for the NPU backend.
4. **The CPU measurements** above.

Open questions for the user:

- CPU first for v1? (Recommended by the numbers.)
- Which model: GigaChat 10B-A1.8B (Russian-native, faster generation, 6 GB) or
  Qwen3-4B (2.4 GB)?
