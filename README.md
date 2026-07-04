# box3d-stdb

[box3d](https://github.com/erincatto/box3d) (3D rigid-body physics, C) built for
[SpacetimeDB](https://spacetimedb.com) server modules (`wasm32-unknown-unknown`).

The published [`box3d`](https://crates.io/crates/box3d) wrapper crate can't run inside a
SpacetimeDB module: its `box3d-sys` builds the C library with cmake (which doesn't cross-compile to
bare wasm) and the target has no libc, no allocator, and no libm. This repo provides a **drop-in
replacement for `box3d-sys`** — consumed via `[patch.crates-io]` — that compiles the vendored C
with `cc`, ships the missing symbols, and commits pre-generated bindings. The upstream `box3d`
wrapper is used unmodified.

On top of that sits **`box3d-stdb`**, a persistence layer built for SpacetimeDB's semantics:
reducers are transactional (a panic rolls back your tables but *not* wasm linear memory) and
instance memory dies on republish/restart — so a live physics world can never be the source of
truth. `box3d-stdb` treats tables as the record and the in-memory world as a generation-guarded
cache: every tick validates the cache against a durable stamp, steps, and commits the delta.

## Usage

Your module's `Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
spacetimedb = "2.6"
box3d = "0.1.14"
box3d-stdb = { git = "https://github.com/suirad/box3d-stdb", tag = "v0.1.14-r1" }

# Reroute the wrapper's box3d-sys to the wasm-ready build in this repo.
# Cargo only honors [patch] in the workspace root manifest. Pin the same tag.
[patch.crates-io]
box3d-sys = { git = "https://github.com/suirad/box3d-stdb", tag = "v0.1.14-r1" }
```

> **Why git, not crates.io:** the drop-in `box3d-sys` must carry the exact upstream package name
> for `[patch.crates-io]` to substitute — a name crates.io already owns — so this repo is
> consumed as a git dependency by design. Pin a release tag (`v<box3d-sys version>-rN`), never a
> branch.

Register the world once, then one guarded call per tick — reconcile the cache, run your game
logic, step, commit:

```rust
use box3d::{BodyDef, Vec3};
use box3d_stdb::{create_world, with_world_paced, WorldDef};

// once, e.g. match setup — stores the definition (gravity -10z, mirrored);
// the C world itself is built lazily by the first with_world call
create_world(ctx, world_key, &WorldDef::default())?;

#[spacetimedb::reducer]
fn tick(ctx: &ReducerContext, timer: TickTimer) -> Result<(), String> {
    let res = with_world_paced(ctx, timer.world_key, 1.0 / 60.0, 4, /*max_catchup*/ 4,
        // rebuild: construction only — recreate bodies from YOUR tables after a cache drop;
        // the glue restores each body's transform/velocity from its mirror row
        |w| { w.spawn(BALL, BodyDef::dynamic_at(Vec3::new(0.0, 0.0, 5.0)))?; Ok(()) },
        // game: per-tick logic, before integration
        |w| { /* inputs, impulses, spawns */ Ok(()) },
    )?;
    // res.events: simulation moves + contact/sensor/hit events; res.steps_run: 0..=4
    Ok(())
}
```

For event-style reducers (a player input outside the tick), `with_world` runs exactly one step
with the same closures — note each such call adds one `dt` of simulation the pacing clock
doesn't account for.

Body state lives in the private `b3_body` mirror table (transform, velocities, sleep flag) —
expose it to clients through your own `#[view]` (projected/filtered as you like) or flip the
`public-mirror` feature for whole-table subscription. Raw `box3d` API stays available
(`pub use box3d`, plus a `w.world()` escape hatch inside closures).

**The construction contract:** the glue persists *dynamics* (pose/velocity/sleep); everything
constructive — shapes, densities, joints, and any runtime changes to them — is yours to persist
in your own tables and re-apply in `rebuild_fn`, exactly like spawns. Joints are raw-API
(`w.world()` + `JointId`); their force-threshold events arrive in
`StepResult.events.joint_overloads` for breakage logic.

### Realtime pacing

SpacetimeDB's scheduler drifts (~8% under-firing measured against a 60 Hz
`ScheduleAt::Interval`), and a naive one-step-per-firing tick silently loses that time —
a 10-minute match ends at ~9m15s of simulation. `with_world_paced` repays wall-clock time in
whole fixed-`dt` steps (0 to `max_catchup` per firing) from a replay-stable timestamp
accumulator: determinism keeps its fixed `dt`, wall-clock fidelity comes from the step *count*.
Measured: 0.3% sim-vs-wall deviation; 12 concurrent 60 Hz worlds all hold rate on ~54 Hz
scheduler firings (catch-up absorbs the contention), and a 15k-body overload degrades to its
maximum sustainable rate then self-recovers to realtime once the scene settles — pacing needs
no per-world tuning as world counts grow.

Under sustained overload (step cost approaching `dt`), catch-up is capped and the excess
backlog is dropped — the sim runs at its maximum sustainable rate instead of death-spiraling,
and returns to realtime by itself when load falls (verified with a 15k-body world). During an
N-step catch-up the game closure runs once (table-borne inputs must not re-apply per step),
events are collected from every step, and the mirror commits once from each body's final state.

### Ephemeral worlds

Worlds that don't need durable physics (lobby matches, cosmetic sims) opt out of the mirror
entirely with `persistence: Persistence::Ephemeral` in their `WorldDef` — stepping cost drops
to raw box3d
(measured: 87 ms ephemeral vs 88 ms raw vs 138 ms mirrored, 512 bodies × 300 steps). The
abort-safety guard still applies; a cache drop just respawns the world fresh. Per-tick positions
come back in `StepResult.events.moves` — pipe them to a broadcast event table for clients
(`examples/ephemeral-demo` shows the pattern). Persistence is fixed at world creation.

### Choosing a configuration

The knobs compose — pick by what the world is for:

| Goal | Configuration | What you pay |
| --- | --- | --- |
| **Max sim speed** (lobby matches, cosmetic physics) | `Persistence::Ephemeral` + `with_world_paced` | state resets to spawn on any cache drop; clients need an event-table pipe for positions |
| **Durability** (persistent zones, resumable matches) | `Persistence::Mirrored` (default) | ~1.5–1.8× stepping while bodies are awake (0.3–0.5 µs per moving body per step; sleeping bodies are free — measured 512 asleep bodies stepping at pure call overhead) |
| **Realtime accuracy** | `with_world_paced` on the scheduled tick (both modes) | negligible — one row field + integer math per firing |
| **Zero-boilerplate client visibility** | `public-mirror` feature | whole mirror visible to every subscriber; use consumer `#[view]`s instead for filtering/interest management |
| **Cheap idle worlds** | leave sleeping enabled (box3d default) | none — asleep bodies skip solver, commit, and broadcast (a settled world is measured-silent); `w.wake(key)` before mutating a resting body |
| **Burst absorption vs latency** | `max_catchup` (we use 4) | higher = repays longer stalls in one firing (bigger tx); lower = smoother per-firing cost, drops backlog sooner |
| **Determinism auditing** | `BOX3D_FORCE_SCALAR=1` build | ~1.7× slower stepping; scalar and SIMD are bit-identical, so this is for isolating suspicion, not correctness |

Rule of thumb: start Mirrored + paced everywhere; flip worlds to Ephemeral when their state
genuinely doesn't need to outlive a crash — that single switch buys back the entire mirror cost
(measured: ephemeral ≡ raw box3d within noise at every body count).

## Configuration

| knob | default | effect |
| --- | --- | --- |
| `BOX3D_MAX_WORLDS` env var at build time | `1024` | overrides box3d's world cap (upstream 128; must stay < 65535) |
| `simd` cargo feature | **on** | wasm SIMD128 codegen via emscripten's SSE2 compat headers. Measured ~1.7× faster on contact-heavy scenes with bit-identical results vs scalar |
| `BOX3D_FORCE_SCALAR=1` env var at build time | unset | forces the scalar path even with `simd` enabled (cargo features are additive, so scalar can't be selected by feature once the wrapper pulls our defaults) |
| `public-mirror` cargo feature | off | makes the `b3_body` mirror table public — zero-boilerplate client subscription instead of consumer `#[view]`s |

These crates are **wasm-only** — native builds fail fast with a pointer to upstream `box3d-sys`.

## Examples

- `examples/demo-module` — the canonical mirrored lifecycle: `create_world` → 60 Hz scheduled
  `tick` → `jump` (grounded check) → `teardown_world`, with a `#[view]` projecting the mirror.
  A ball republished mid-flight resumes its arc — position *and* velocity restored from tables.
- `examples/ephemeral-demo` — same ball on an ephemeral world, positions broadcast through a
  SpacetimeDB event table (rows are never stored; clients receive `onInsert` only).
- `examples/test-module` + `scripts/test-c-mem.fish` — guard/mirror/poison probes, settle and
  bench reducers, and a 10-assertion C-heap leak suite (every error path frees to exact baseline;
  a poisoned world leaks exactly one world's allocations by design — torn C state is never
  destroyed).

## How it works

- `crates/box3d-sys` — same package identity as crates.io `box3d-sys` (name / version / `links`),
  so the patch substitutes cleanly. `build.rs` compiles all vendored C TUs with `cc`; no cmake, no
  bindgen, no libclang needed by consumers.
- `shims/` — minimal libc headers for the bare-wasm C compile, plus C stubs (`qsort`, `str*`,
  inert file I/O). Rust side (`src/stdb.rs`) exports `aligned_alloc`/`free`/`malloc` over the
  module's global allocator, the libm functions clang can't lower to native wasm ops, and a
  zero-dependency `vsnprintf` (`core::fmt` over a wasm32 va_list walk) so box3d's log messages
  format for real.
- `src/bindings.rs` — pre-generated by `scripts/regen-bindings.fish`, committed. Two non-obvious
  flags it encodes: `-fvisibility=default` (wasm clang hides functions; bindgen silently emits
  zero fns without it) and an enum `c_uint`→`c_int` rewrite (newer libclang reports C enums
  unsigned; the published wrapper expects signed).
- `vendor/box3d` — upstream submodule, pinned. Clone with `--recurse-submodules` (cargo git deps
  fetch it automatically).
- `crates/box3d-stdb` — re-exports `box3d` plus the persistence layer: `with_world` (generation
  guard, poison handling, post-step delta commit), `WorldCtx` mutators (`spawn`/`destroy`/
  `set_transform`/`wake` keep the C world and the mirror consistent in one call), the `b3_world`/`b3_body`
  tables, and `StepResult` events. Also `install_box3d_logging()` — call once from your `init`
  reducer to route box3d's internal warnings (`b3Log`) to the module log.

## Releasing

This repo is consumed as a pinned git dependency (see the Usage note — crates.io is not an
option for a `[patch]`-target crate). To cut a release:

1. Verify the runtime gates (Maintenance step 6) and that `CHANGELOG.md` covers the changes
2. Move the `[Unreleased]` changelog section under a `v<box3d-sys version>-rN` heading
3. Tag: `git tag v0.1.14-rN && git push --tags` — consumers pin this tag in both their
   dependency and `[patch]` lines
4. Consumers must clone with submodules (`--recurse-submodules`); cargo git deps handle this
   automatically

## Maintenance (upstream version bump)

Version policy: `crates/box3d-sys` tracks the upstream `box3d-sys` release exactly (the `[patch]`
substitution requires it — name/version/`links` are the contract); `crates/box3d-stdb` tracks the
`box3d` wrapper version it re-exports.

1. Bump the `vendor/box3d` submodule to the commit the new upstream `box3d-sys` vendors (or a
   header-compatible one)
2. Diff `wrapper.h` + `src/lib.rs` against [Tebarem/box3d-rs](https://github.com/Tebarem/box3d-rs)
   (a ~15-line tracked surface) and fold in any changes
3. Set both crate versions per the policy above, and bump the `box3d` dep in `crates/box3d-stdb`
4. `crates/box3d-sys/scripts/regen-bindings.sh` (or the `.fish` twin; needs `bindgen-cli`)
5. `cargo build -p box3d-stdb --release` — the cdylib is the link gate: it must produce a
   fully-linked `.wasm` (its only imports are the SpacetimeDB host ABI; SIMD is the default
   path); then rebuild with `BOX3D_FORCE_SCALAR=1` to gate the scalar variant too
6. Publish `examples/demo-module` to a local `spacetime start` and run the lifecycle
   (`create_world` / `jump` / `teardown_world`), plus `scripts/test-c-mem.fish` — the runtime
   gates

## License

MIT. Upstream components:

- [box3d](https://github.com/erincatto/box3d) (vendored submodule) — MIT, Erin Catto
- [box3d / box3d-sys](https://github.com/Tebarem/box3d-rs) crates (wrapper consumed from crates.io;
  `wrapper.h` + `src/lib.rs` skeleton mirrored in `crates/box3d-sys`) — MIT
- SSE2 compat headers under `crates/box3d-sys/shims/include/wasm-sse2/` — MIT/NCSA, from
  [emscripten](https://github.com/emscripten-core/emscripten) (see its README)
