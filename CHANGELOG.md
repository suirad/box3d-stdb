# Changelog

All notable changes to this project. Versions track the upstream `box3d`/`box3d-sys` releases
they build on (see README maintenance policy).

## [Unreleased]

## v0.1.14-r3 — 2026-07-09

### Activity-adaptive ticking (crate-native)

- **BREAKING — `with_world_paced` is now policy-driven:** signature is
  `with_world_paced(ctx, world_key, rebuild, game)`; dt/substeps/max_catchup live in
  `WorldDef.tick: TickPolicy`, stored durably at `create_world` (the stored policy is the only
  source — callers can't diverge, extending the WorldDef contract to time itself).
  `TickPolicy::realtime(dt, substeps, max_catchup)` is the old behavior exactly and is the
  default; `TickPolicy::adaptive(tiers, slow_v, k_slow, k_quiet, max_catchup)` runs a tier
  ladder: demote after `k_slow` consecutive slow steps (fewer substeps, then bigger fixed dt
  behind the velocity gate that doubles as the tunneling-safety proof), promote to full
  instantly on any fast mover, and return `TickDirective::Park` once asleep for `k_quiet`
  steps. Adaptive state (tier/counters/parked) rides the row write the tick already does —
  zero extra I/O; realtime policies and 0-step firings skip the pass entirely.
- **BREAKING — `b3_world` schema:** the row gains `tick_policy` (nested type) +
  `tick_tier`/`slow_ticks`/`quiet_ticks`/`parked`. Existing databases republish with
  `--delete-data` or migrate by hand (internal table, pre-1.0).
- `resume_full_rate(ctx, key) -> bool(was_parked)` — snap a world back to full rate at the top
  of wake-capable reducers; generation-neutral (never invalidates the cache).
- `set_tick_policy(ctx, key, &TickPolicy)` — replace a live world's policy (e.g. 60 Hz → 30 Hz
  between rounds); validated, effective next firing, generation-neutral (changes *time*, not
  *physics* — the cache stays valid). Tier/hysteresis reset to full; `parked` untouched.
- Hardened after adversarial (Codex) review: policies are validated at `create_world` (an empty
  tier ladder or sub-µs dt inside the scheduled tick would be a permanent error loop that leaks
  a poisoned cache per firing); the adaptive speed signal is the per-step max, not each body's
  final velocity (a fast-then-slow catch-up batch can't fake calm); park additionally requires a
  moveless batch, so at least one tick always runs after the last event — work your `post` hook
  queues is never stranded by a same-tick park; the macro's `ensure` replaces timer rows
  wholesale on unpark (row presence can race a concurrent Park delete — the parked flag is the
  transactional truth). Closure contract documented: never edit `b3_world`/call
  `resume_full_rate` inside `rebuild`/`game`.
- `tick_schedule!` — declarative macro generating the consumer's tick reducer (scheduler-only
  guard, paced call, `post` hook for events/metering, Park→timer-delete) and an `ensure` helper
  (resume + re-arm). The scheduled table stays consumer-declared (a proc-macro hygiene limit in
  spacetimedb 2.6.1 prevents generating it from macro_rules). Viewer connections should
  arm-only, not `ensure` — resetting hysteresis on every connect would keep a busy lobby from
  clocking down (the sandbox shows both paths).
- `WorldCtx::awake_count()` — the live settle signal (`b3World_GetAwakeBodyCount`).
- Live-measured on the crate engine (10-ball pile): 60.0 steps/s FULL, demote cascade exactly at
  `k_slow`, 20.0 steps/s sustained CRAWL, two instant promotes under real pile collapses, park
  with the generated timer-delete, zero cost hands-off. Substep decimation −32% fuel/step at 16
  awake (346k → 236k measured); bottom tier meters 6× less work than fixed 60 Hz/4-substep.
  `examples/sandbox-module` shrank ~90 lines by moving onto the crate engine; demo-module uses
  the macro; 12-assertion leak suite re-verified.

### Procedure execution mode

- The tick can run inside a SpacetimeDB `#[procedure]` instead of a scheduled reducer — for both
  mirrored and ephemeral worlds, with no crate changes. `TxContext` (from `ctx.try_with_tx`)
  derefs to `ReducerContext`, so `with_world`/`with_world_paced`/`create_world` accept the tx via
  deref coercion. The generation guard stays abort-safe as long as the whole tick lives in one
  `try_with_tx` (not `with_tx`, which commits on `Err`); the commit-conflict retry self-heals a
  double-step via the existing generation mismatch → rebuild path. Procedures also enable a
  scheduler-free `sleep_until` self-pacing loop.
- `examples/procedure-demo` — the bouncing ball ticked by a `#[procedure]`, with the `sleep_until`
  loop and a mirror `#[view]`. README gains a *Ticking from a procedure* section.

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
