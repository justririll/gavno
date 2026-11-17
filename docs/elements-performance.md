# Element collection measurements, 2026-11-17

SHM imports dominate the reproduced workload. On this PC, eight clients each
committing a 2880×1620 XRGB8888 SHM buffer produce 145,800 KiB of committed damage
per output frame. In the uncapped measurement run, 124 such frames had median
`winit.elements=15.245 ms`, p95 18.149 ms, with almost all time in
`elements.space`. The final binary with the normal warning limit also captured
`elements.space=10.9–11.0 ms`, `shm_kb=145800`. This reproduces the magnitude and
location of the reported stalls, but does not establish the exact cause of the
original laptop frames without their new stage measurements.

The small safe cut is an unnecessary full upload of *unchanged* SHM attachments.
A new attachment clears Smithay's imported-element cache; its GLES cache still
has the texture, but Smithay 0.7 calls `TexImage2D` when damage is empty. The local
patch returns that matching cached texture before waiting on GPU readers or
uploading. It also checks format and alpha as well as size before reuse. First
imports, size/format changes, and damage-history fallback still upload in full.
Actual full-frame client damage still requires the full transfer.

## Measurements

The nested compositor used Mesa 26.2.6, OpenGL ES 3.2, llvmpipe (LLVM 23.1.4,
256 bits), on x86_64. No `/dev/dri` is exposed on this PC. The nested host output
is smaller than the buffers; imported buffer dimensions remain 2880×1620. These
are CPU wall times, not GPU timestamps or measurements of the Adreno driver.

| Single 2880×1620 SHM client | Damage KiB/frame | Elements median before → after | p95 before → after | Space median before → after |
| --- | ---: | ---: | ---: | ---: |
| Whole-buffer redraw | 18,225 | 1.567 → 1.950 ms | 1.941 → 2.203 ms | 1.4 → 1.8 ms |
| 64×64 redraw | 16 | 0.235 → 0.194 ms | 0.353 → 0.250 ms | 0.1 → 0.1 ms |
| Alternating unchanged attachments | 0 | 1.470 → 0.224 ms | 1.808 → 0.285 ms | 1.4 → 0.1 ms |

The before/after upload runs used the same initial profiling instrumentation;
the final accounting refinement subsequently removed its per-frame surface walk.
Each single-client run committed 180 frames. Statistics use 175 output frames
after the first buffer frame and four warmup frames; initial shader compilation
and texture allocation are excluded. Full/partial runs occasionally contain an
extra shell frame with no client commit. The unchanged case improves about 85%.
The full-redraw path is unchanged by the patch; its slower after measurement
shows run/load variability, not an improvement. Some measurements overlapped
compilation, so small differences between full/partial runs are not conclusive.

For sampling only, the benchmark binary temporarily bypassed the shared warning
limit for `winit.elements`, with `GAVNO_STALL_MS=0`. This bypass is removed from
the final source and binary; the original 20 warnings/minute limit remains.
Cold shader compilation can itself produce a large `elements.space` sample with
zero SHM damage, so cold and steady-state frames must be distinguished.

After clients closed and animations settled, `rendered_frames` stayed constant
across two seconds: 256→256 (full), 252→252 (partial), 242→242 (unchanged).
The final eight-client run also stayed at 220→220 frames during its quiet check.
The new code has no timers, workers, idle callbacks, probes, or I/O. Counters run
only on client commits; stage timestamps run only while collecting/rendering a
frame. `Stall::mark` writes to a fixed 24-entry stack array without allocating.
Warning formatting and output-name allocation remain after the threshold and
rate-limit checks.

## Reading the new warnings

`elements.space` includes client imports, window wrapping and decorations;
`elements.decor` isolates decoration setup. `elements.layers.front` and
`elements.layers.back` isolate layer-shell imports. `elements.cursor` isolates
cursor imports. `elements.shell`, `elements.desktop`, `elements.background`,
`elements.lock` and `elements.color` identify shell/UI, lock and color work.
`elements.state` includes accent/color-state collection, while `elements.tail`
includes the final brightness element and bookkeeping. `prepare`, `hdr`,
`scanout`, `fence`, `queue`, and `rest` remain separate. Stage values are disjoint
intervals, rounded to tenths of a millisecond. Sum the `elements.*` values to
compare with the former aggregate `elements` stage.

`shm_kb` is buffer-pixel damage × 4 / 1024, gathered after Smithay applies
commits and synchronized children. Damage is assigned after mapping to the
window's owning/overlapping outputs, layer output, lock output, or cursor
output. Each output has an independent pending counter, reset on its next
rendered frame attempt. Captures do not consume it. Non-SHM and repeated
unchanged damage commits add zero. Overlapping rectangles and multiple commits
are summed, including commits to an output's hidden/occluded owned windows;
this estimates committed damage, not exact GL transfer bytes or visible pixels.
Initial/forgotten damage history uses Smithay's full-buffer fallback. No scale
factor is applied twice; damage is already in buffer coordinates.

Smithay already reuses a texture per surface/context and skips import entirely
when there is no new buffer attachment. With nonempty damage and a matching
texture, GLES uses `TexSubImage2D` for the damaged rectangles. It sets
`UNPACK_ROW_LENGTH` from the SHM stride and `UNPACK_SKIP_PIXELS/ROWS` for each
rectangle, then restores them. GLES 2 requires `GL_EXT_unpack_subimage`; GLES 3
uses the core functionality. It does not repack rows or swizzle pixels in Rust.
ARGB8888/XRGB8888 use BGRA uploads, and Smithay requires
`GL_EXT_texture_format_BGRA8888`; ABGR/XBGR use RGBA. Driver-internal conversion,
copying and synchronization still depend on Mesa/the actual GPU. The patch does
not change those formats or bypass synchronization for damaged uploads.

## The Smithay patch is not applied

GAVNO still builds against crates.io Smithay 0.7. The measurements above used a
workstation-local `[patch.crates-io]` override to an unpacked copy of the crate;
that override is not committed, because the laptop builds from this repository
alone. The two files in `patches/` keep the change. Applying it means vendoring
Smithay into the repository (or a fork), which is worth doing only if the laptop's
new `elements.space` and `shm_kb` fields show unchanged-attachment uploads in real
stalls. First imports and full redraws, the likely cause of stalls while foot
windows open, are not helped by it.

In GAVNO the changed files are:

- `crates/gavno/src/perf.rs`
- `crates/gavno/src/backend/udev.rs` (render path and capture argument only)
- `crates/gavno/src/render.rs`
- `crates/gavno/src/handlers/compositor.rs`
- `crates/gavno/src/render_sanitize.rs`
- `crates/gavno/src/render_sanitize/tests.rs`
- `crates/gavno/src/decor.rs` (small timing hook around layers/windows)
- `crates/gavno/src/backend/winit.rs` (same profiling for reproduction)
- `crates/gavno/src/ipc.rs` and `crates/gavno/src/ui/background.rs` (capture/test call arguments)
- `patches/smithay-shm-empty-damage.patch`
- `patches/smithay-local-build-warnings.patch`
- `docs/elements-performance.md` (this report)
- `docs/debugging.md` (link to this report)

In `/home/kirill/HDD/ds-build/smithay-0.7.0`, the substantive fix is confined to
`src/backend/renderer/gles/mod.rs`. Exposing this old crate as a path dependency
also exposes warnings that Cargo caps for registry dependencies. Seven small
compatibility edits remove five unused `error` imports, cast the SIGBUS handler
through a pointer, and locally allow the deprecated `fetch_update` spelling to
retain Smithay 0.7's MSRV. These are captured separately in
`smithay-local-build-warnings.patch`; affected files are
`src/backend/drm/surface/gbm.rs`, `src/backend/session/libseat.rs`,
`src/backend/winit/mod.rs`, `src/input/keyboard/mod.rs`,
`src/wayland/seat/keyboard.rs`, `src/wayland/shm/pool.rs`, and
`src/backend/allocator/swapchain.rs`. No broad warning suppression was added.

## Reproduction and checks

The temporary client and runner are retained in `/tmp/gavno-elements/`:
`shm-client.c`, `xdg-shell-client-protocol.h`, `xdg-shell-protocol.c`, `run.py`,
`eight.sh`, `summary.py`, raw per-run logs, state replies and build/test logs.
The client uses two SHM buffers and Wayland frame callbacks; it writes only the
damaged region, or the full buffer in full mode. No installed SHM demo was
available, so this tiny C client was compiled with:

```sh
wayland-scanner client-header /usr/share/qt6/wayland/protocols/xdg-shell/xdg-shell.xml /tmp/gavno-elements/xdg-shell-client-protocol.h
wayland-scanner private-code /usr/share/qt6/wayland/protocols/xdg-shell/xdg-shell.xml /tmp/gavno-elements/xdg-shell-protocol.c
cc -O2 -Wall -Wextra -Werror /tmp/gavno-elements/shm-client.c /tmp/gavno-elements/xdg-shell-protocol.c -lwayland-client -o /tmp/gavno-elements/shm-client
```

`run.py` creates a private home/config/runtime, forwards the host Wayland socket
by absolute path, starts `gavno --winit` with debug IPC, and cleans up its
compositor. The workload is `/tmp/gavno-elements/shm-client 2880 1620 -1 180`;
use `64` for a small damaged square or `0` for unchanged attachments. `eight.sh`
starts eight copies. The runner's `GAVNO_BENCH_BIN`, `GAVNO_BENCH_COMMAND`, and
`GAVNO_BENCH_THRESHOLD` select the binary, client command, and warning threshold.
These are runner-only options, not new compositor settings.

The required build/test commands use exactly:

```sh
export CARGO_TARGET_DIR=/home/kirill/HDD/ds-build/target-elements
export CARGO_HOME=/home/kirill/HDD/ds-build/cargo-home
cargo build -j 4 -p gavno
cargo test -j 4 -p gavno
```

Final verification: build passed without warnings; tests passed without warnings
(435 unit tests passed, 2 ignored; 4 IPC integration tests passed).
`git diff --check` passed in both repositories. The known polkit flaky test
passed in the full suite, so no isolated retry was needed. Unit coverage includes
buffer-pixel damage sums, duplicate commit visits, zero-area damage, non-SHM
buffers, empty damage, independent output resets and the complete new stage
format/capacity. An initial test attempted to construct a negative Smithay
`Size`, which Smithay rejects before accounting; that invalid fixture was
removed.

TTY/DRM, Adreno upload/fence behavior, the HDR panel at scale 1.35, and the exact
foot opening sequence cannot be verified on this PC. Genuine 18.7 MB full redraws
still cost a transfer. The new fields should be used in the real session to
separate those from zero-damage attachment waste or unrelated shell work.

An optional screenshot/pixel check of the unchanged-buffer case could not be
completed: nested capture returned an error and the log recorded
`EGL_BAD_SURFACE` from `eglQuerySurface`. No pixel-level screenshot validation is
claimed. The SHM workloads themselves completed, including all eight clients'
180 frame commits in the production-limit reproduction. An early eight-client
runner completion check expected `elapsed_s=` in its shell marker and timed out
after the clients had finished; the marker check was corrected and the run was
repeated successfully as `eight-production-verified`.

One final relink initially exhausted the dedicated target filesystem. Only the
obsolete registry-Smithay GAVNO binary/test artifacts and their incremental
caches created during this task were removed; the target/home settings and
source files were retained, and the required commands were rerun.
