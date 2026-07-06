# Changelog

All notable changes to this project. Versions track the upstream `box3d`/`box3d-sys` releases
they build on (see README maintenance policy).

## [Unreleased]

## v0.1.14-r2 — 2026-07-06

### Gameplay helpers

- `WorldCtx` game-query/dynamics tier: `cast_ray_closest` → `KeyedRayHit` (closest hit resolved
  to a consumer body key — the upstream wrapper drops the shape id on this path), linear
  impulses/forces at a point or center of mass, torque, angular impulse, linear/angular velocity
  getters, `mass`, and `overlap_sphere` (deduped body keys). All mutators wake the body: box3d
  silently ignores forces on sleepers.
- `examples/sandbox-module` + `examples/sandbox-client` — multiplayer sandbox showcase: raycast
  shooting, per-body impulses, pit-sensor scoring with a height backstop, presence-gated 60 Hz
  ticking (first connection starts the timer, last one out stops it — a maincloud-energy saver),
  body cap; vanilla three.js client on the `public-mirror` table with a GitHub Pages deploy
  workflow and a live per-body energy-cost estimate. Gotcha the module encodes: box3d sensors
  only report visitor shapes that set `enable_sensor_events` themselves.

## v0.1.14-r1 — 2026-07-04

### Foundation

- box3d (C, 3D rigid-body) compiled for `wasm32-unknown-unknown` with `cc` — no cmake, no
  bindgen/libclang at consumer build time. Drop-in `box3d-sys` replacement consumed via
  `[patch.crates-io]`; upstream `box3d` wrapper used unmodified.
- Missing target symbols shipped from the crate: `aligned_alloc`/`free`/`malloc` over the
  module's global allocator, libm re-exports, minimal libc shims, and a zero-dependency
  `vsnprintf` (`core::fmt` over a wasm32 va_list walk) so box3d log messages format for real.
- wasm SIMD128 on by default: ~1.7× on contact-heavy scenes, bit-identical results vs scalar
  (`BOX3D_FORCE_SCALAR=1` escape hatch). `BOX3D_MAX_WORLDS` build-time knob (default 1024).
- `install_box3d_logging()`: box3d internal warnings (`b3Log`) → module log.

### Persistence layer (`box3d-stdb`)

- Generation-guarded world cache: tables are the record of truth; the in-memory `b3World` is
  validated against a durable stamp on every entry. Reducer aborts, panics, republishes, and
  restarts all force a rebuild — the cache is never silently trusted.
- Poison discipline: a world possibly torn mid-mutation is abandoned (`mem::forget`), never
  destroyed — `b3DestroyWorld` on torn C state is UB through the module-global allocator. The
  leak is bounded and measured: exactly one world's allocations (164,380 B empty) per event;
  every other error path frees C heap to exact baseline (12-assertion test suite).
- `b3_body` mirror table: per-body transform, velocities, and sleep flag, delta-committed from
  box3d's post-step move events — sleeping/static bodies cost nothing. Private by default;
  expose via consumer `#[view]`s or the `public-mirror` cargo feature.
- `WorldCtx` mutators — `spawn`, `destroy`, `set_transform`, `wake`, `body_id` — keep the C
  world, the mirror row, and the id↔key maps consistent in a single call (user mutations never
  appear in box3d's event stream, so they must write the mirror themselves). `wake` unblocks
  velocity sets on resting bodies, letting worlds keep sleeping enabled — a settled world costs
  zero mirror traffic and zero broadcast bandwidth (measured).
- Rebuild overlay: surviving mirror rows restore position *and* velocity onto freshly spawned
  bodies — a ball republished mid-flight resumes its arc (verified against ballistics). Stale
  rows are swept; orphaned cache slots from aborted create+step transactions are reconciled.

### World lifecycle API

- `create_world(ctx, key, &WorldDef { gravity, capacity, persistence })` — registers a world,
  storing its definition durably; rebuilds read the row, so callers can never accidentally
  rebuild with different physics. The C world builds lazily on first use.
- `with_world(ctx, key, dt, substeps, rebuild, game)` — one guarded tick: reconcile → eager
  generation bump → game closure (pre-integration) → step → post-step delta commit. Unknown
  keys are a hard error. `destroy_world` completes the lifecycle (idempotent).
- `StepResult` events: simulation moves (with linear/angular velocities — the id-tier exposes
  no velocity getters, so move events are the consumer's only velocity channel), contact
  begin/end, hits (point/normal/speed), sensor begin/end, and joint force-threshold overloads
  (`joint_bits`, consumer-correlated); body keys remapped, `None` for bodies not spawned
  through the glue.

### Ephemeral worlds

- Per-world mirror opt-out (`Persistence::Ephemeral` in `WorldDef`): no mirror I/O at all —
  stepping cost equals raw box3d (87 ms vs 88 ms raw vs ~140 ms mirrored; 512 bodies × 300
  steps). The abort-safety guard still applies; a cache drop respawns fresh. For lobby-style
  worlds that don't need physics state to survive.
- Per-tick positions for ephemeral worlds via `StepResult.events.moves` — pipe to a broadcast
  event table (rows never stored, clients receive `onInsert` only); `examples/ephemeral-demo`
  ships the pattern.

### Realtime pacing

- `with_world_paced(ctx, key, dt, substeps, max_catchup, rebuild, game)`: repays wall-clock
  time in whole fixed-`dt` steps from a replay-stable timestamp accumulator. Sub-`dt` remainder
  carries; backlog beyond the cap is dropped (carrying it would death-spiral an overloaded
  scheduler). Measured: 0.3% sim-vs-wall deviation over 10 s against ~8% drift unpaced.
- Catch-up semantics: game logic runs once before the loop (table-borne inputs must not
  re-apply per step); events are harvested per step and moves merged latest-per-body, so a body
  that sleeps mid-catch-up still reaches the single post-loop mirror commit.

### Examples & verification

- `examples/demo-module` — mirrored bouncing-ball lifecycle with a projecting `#[view]`.
- `examples/ephemeral-demo` — same scene, ephemeral + event-table broadcast.
- `examples/test-module` + `scripts/test-c-mem.fish` — guard/poison/mirror probes, settle and
  bench reducers, 12-assertion C-heap leak suite.
- Every unit independently reviewed and runtime-verified on local SpacetimeDB; one external
  adversarial review pass (2 confirmed findings, both fixed); clippy-clean across all targets.

### Known limitations

- Cross-rebuild determinism is physically plausible, not bit-exact (solver warm-start state is
  not persisted). A lossless blob checkpoint was investigated and parked: mirror restore is
  measured-indistinguishable for game workloads, and true exactness would cost per-tick
  whole-world serialization plus internal-ABI coupling. Revisit only for replay-grade
  determinism consumers.
- Joints and post-spawn shape mutation are raw-API territory by design: they are construction
  state, so persist them in your own tables and re-apply in `rebuild_fn` (same contract as
  spawns). Joint force-threshold events ARE surfaced (`StepEvents.joint_overloads`).
