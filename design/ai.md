# Local AI for GAVNO

Status: v1 built (CPU only, 2026-11-19). The decisions the user approved are in
"Decisions for v1" at the end; the measurements are in "Measurements". Nothing is
downloaded: llama.cpp and the models were already on the laptop.

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
| `llama-gavno` (new) | Qwen3-4B-Instruct-2507 Q4_0 | CPU, `-t 6`, ctx 4096, q8_0 KV | 8090 |

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

**Decision for v1:** the CPU runs the interactive features. There is no NPU work in v1
at all: gavno-ai never touches `/dev/fastrpc*`, `cdsprpcd` or an NPU unit, and there
is no `backend` config key. The NPU may return later as an opt-in
backend for background jobs, once energy per token has been measured. This revises the
"NPU first" note from 2026-11-15, because the measurements contradict it for this
workload.

**Model (changed 2026-11-19):** the user chose Qwen3-4B-Instruct-2507 Q4_0 (2.4 GB) over
GigaChat 10B-A1.8B. It is smaller in RAM and has no thinking mode; the prompt gives
Russian examples and the JSON schema keeps the output valid.

## Architecture

```
launcher (GAVNO, compositor thread)
   | worker thread, calloop channel back; never blocks the compositor
   v
gavno-ai  (separate binary, session bus org.gavno.AI1, D-Bus activated, exits when idle)
   | systemd D-Bus StartUnit/StopUnit (system bus; polkit lets wheel do it)
   | HTTP to 127.0.0.1:<port> (/health, /v1/chat/completions)
   v
llama-server  (system unit llama-gavno.service, see "Decisions for v1")
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
| `Prewarm(context: s)` | Start the model unit if it is not running and evaluate the stable prompt prefix (system prompt, apps, windows) from the context JSON, which may be empty. Returns at once. |
| `Interpret(query: s, context: s) -> s` | Context and result are JSON (below). |
| `NameWorkspace(windows: s) -> s` | Returns `{"name": "..."}`, or an error when the model is not already running. |
| `Status() -> a{sv}` | `running`, `backend`, `model`, `last_latency_ms`. |

### Request path (`Interpret`)

1. **GAVNO builds the context.** It is small JSON:
   - windows: id, app_id, title cut to 60 characters, workspace;
   - workspaces: index and name;
   - the dock-pinned apps, then installed apps by name, 60 at most: desktop id and name;
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
- **Prewarm.** The first keystroke that makes a query a candidate calls
  `Prewarm(context)`, once per opening of the launcher. While the user finishes typing,
  the model loads (about 1 to 2 s) and its prompt prefix is evaluated (several seconds
  for 60 apps), so the first answer is not delayed by the whole prompt.
- **Latency.** With the model warm, Enter to preview row takes 0.7 to 1.1 s (measured).
  The row shows "Thinking…" until then.
- **Row behaviour as built.** The "Ask AI" row is the last row. It appears when the
  query has two or more words and no application matches by name prefix or word.
  When no application matches at all, or only weakly, the row starts selected; Enter on
  it sends `Interpret`, Enter on the preview runs it, and editing the query or Esc drops
  the answer. Errors show as the row's text and Enter retries.
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

## NPU rules (for a future NPU backend; v1 has none)

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
idle_stop_minutes = 10
workspace_names = true     # heuristic always; AI refinement only while the model runs
```

gavno-ai reads this `[ai]` table itself (`GAVNO_CONFIG` or `~/.config/gavno/config.toml`)
when it starts. The compositor shows the names in the bar (`Name · window title`) and
lists them per output as `workspace_names` in `gavno msg state`.

## Measurements (2026-11-19, the laptop, CPU only)

Setup: `llama-gavno` as installed (`-t 6 -c 4096 -np 1`, q8_0 KV, `Nice=10`,
`CPUWeight=20`), the user's desktop running. The request is the real one: 386-token
system prompt, 60 apps, 4 windows, a JSON-schema grammar. "Warm" means the prompt
prefix is in llama-server's cache.

**Enter to parsed action, Qwen3-4B-Instruct-2507 Q4_0, through gavno-ai** (D-Bus call
to validated result, 60 apps; ms):

| Command | ms | Result |
|---|---|---|
| move firefox to the second workspace | 1056 | move_to_workspace window 1 -> 2 |
| открой папку загрузки | 852 | open_folder ~/Downloads |
| поставь яркость на 30 | 1014 | set brightness 30 |
| play something calm | 902 | music play "calm" |
| выключи уведомления | 965 | set dnd on |
| switch to workspace 3 | 750 | switch_workspace 3 |
| запусти терминал | 775 | launch foot |
| перемести телеграм на четвертый рабочий стол | 1114 | move_to_workspace window 3 -> 4 |
| pause the music | 683 | music pause |
| включи ночной свет | 948 | set night_light on |
| delete all my files | 351 | `{"error": "I could not turn that into an action"}` |

All ten were correct, in Russian and English. The second Enter runs the action in
the compositor and has no model cost.

**Cold and cached prompt** (same 1606-token prompt with 100 apps): evaluated once it
takes 17.8 s at `-t 6` (13.3 s at `-t 10`); cached, only the query is read. Without
the cache every request took 19 s. This is why `Prewarm(context)` evaluates the
stable prefix while the user is still typing, and why the compositor sends at most 60
apps. With 60 apps, a first request 4 s after the first keystroke of a cold model
returned in 7.6 s (the model loads in about 1.1 to 2.2 s; the prefix takes the rest).
Later requests take under 1.2 s.

**Threads and KV cache** (median over the 10 commands, 100 apps):

| Variant | warm-up | median | max | RSS |
|---|---|---|---|---|
| `-t 6`, q8_0 KV (shipped) | 17.8 s | 997 ms | 1272 ms | 4.98 GB |
| `-t 10`, q8_0 KV | 13.3 s | 904 ms | 1177 ms | 4.98 GB |
| `-t 6`, f16 KV | 13.8 s | 869 ms | 1111 ms | 5.26 GB |
| `-t 6`, q8_0 KV, `--no-repack` | 38.1 s | 1139 ms | 1581 ms | 2.73 GB |

- RSS counts the mapped model once and llama.cpp's repacked CPU copy of the weights
  again, so it is about twice the 2.4 GB file; the cgroup (`MemoryCurrent`) showed
  2.85 GB after the run. The q8_0 KV cache saves about 280 MB at ctx 4096 and costs
  about 15 % in latency; f16 is faster if RAM allows.
- `--no-repack` halves RSS but doubles the cold prefix time; not used.
- More threads help little for 20-token answers. `-t 6` is kept so the desktop keeps cores.

**Other models** (same ten commands, `-t 6`): Gemma-4-E4B Q4_0 produced valid but
pretty-printed JSON, needed 3.6 to 5.8 s per command, cut two answers at the token
limit and used 7.6 GB RSS. Qwen3-4B is clearly better for this format and ships.
GigaChat was not measured (the user chose Qwen3-4B).

**Desktop impact:** no `gavno::stall` warning appeared during any run: model start,
the 10-command sets for every variant above, the Gemma run and the gavno-ai runs
(checked with `journalctl -b _COMM=gavno --since <start> | grep -c stall`, 0 each
time). `Nice=10` and `CPUWeight=20` were enough; no thread or weight change was
needed. The baseline already has about 180 stall lines per boot from unrelated 8 ms
render events.

## Still unmeasured

Energy per answer (`power_now`) on the NPU vs the CPU, on battery. Not needed for v1,
which is CPU only.

## Build plan (done)

1. `crates/gavno-ai`: D-Bus service, systemd unit control, a small blocking HTTP client,
   the schema and validation, idle stop. Tests run against a fake llama-server.
2. Launcher integration (`crates/gavno/src/ai`, `launcher.rs`): candidate detection, the
   Ask AI row, prewarm, the preview row, the action executor, a worker thread with a
   calloop channel back.
3. Workspace names: heuristic in GAVNO, AI refinement hook (30 s stable, model already running).
4. Docs: `docs/config.md` `[ai]`, `docs/debugging.md`.

Differences from the plan above: apps sent to the model are the dock-pinned ones first,
then alphabetical (there is no frecency data yet); `max_tokens` is 128 so three actions
cannot be cut off; music `play` with a query opens the music panel with the search
filled in (the user picks the track) rather than playing blindly.

## Decisions for v1 (approved)

1. No downloads.
2. New unit `dist/systemd/llama-gavno.service`: a copy of `llama-ru-giga-cpu` for
   Qwen3-4B-Instruct-2507 Q4_0 with `-t 6 -c 4096 --cache-type-k q8_0 --cache-type-v q8_0`,
   `Nice=10`, `CPUWeight=20`, port 8090. `dist/polkit-1/rules.d/50-local-llm.rules`
   adds `gavno` to the wheel regex. `scripts/install-greeter.sh` installs both, plus
   `gavno-ai` and its D-Bus service file.
3. CPU only. `SendSIGKILL=no` is not needed without the NPU and is not changed.
4. The CPU measurements below.
