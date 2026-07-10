# box3d-stdb

[box3d](https://github.com/erincatto/box3d) (3D rigid-body physics, C) built for
[SpacetimeDB](https://spacetimedb.com) server modules (`wasm32-unknown-unknown`).

**[API docs](https://suirad.github.io/box3d-stdb/docs/)** ·
[live sandbox demo](https://suirad.github.io/box3d-stdb/) — rustdoc is self-hosted on Pages
because a `[patch.crates-io]` crate can't publish to crates.io, so docs.rs can't build it.

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
box3d-stdb = { git = "https://github.com/suirad/box3d-stdb", tag = "v0.1.14-r2" }

# Reroute the wrapper's box3d-sys to the wasm-ready build in this repo.
# Cargo only honors [patch] in the workspace root manifest. Pin the same tag.
[patch.crates-io]
box3d-sys = { git = "https://github.com/suirad/box3d-stdb", tag = "v0.1.14-r2" }
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

// once, e.g. match setup — stores the definition (gravity -10z, mirrored) AND the tick policy
// (default: fixed 60 Hz, 4 substeps, catch-up 4). The stored policy is the only source of
// dt/substeps — callers can't diverge. The C world builds lazily on first use.
create_world(ctx, world_key, &WorldDef::default())?;

#[spacetimedb::reducer]
fn tick(ctx: &ReducerContext, timer: TickTimer) -> Result<(), String> {
    let res = with_world_paced(ctx, timer.world_key,
        // rebuild: construction only — recreate bodies from YOUR tables after a cache drop;
        // the glue restores each body's transform/velocity from its mirror row
        |w| { w.spawn(BALL, BodyDef::dynamic_at(Vec3::new(0.0, 0.0, 5.0)))?; Ok(()) },
        // game: per-tick logic, before integration
        |w| { /* inputs, impulses, spawns */ Ok(()) },
    )?;
    // res.events: moves + contact/sensor/hit events; res.steps_run: 0..=max_catchup;
    // res.directive: Park when an adaptive policy settled the world (see Dynamic tick rate)
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

### Gameplay queries & forces

Inside any `with_world`/`with_world_paced` closure, `WorldCtx` exposes a keyed query/dynamics tier
so game code never has to touch raw ids:

- `cast_ray_closest(origin, translation) -> Option<KeyedRayHit>` — closest ray hit, with the struck
  shape already resolved to its consumer body key (the upstream wrapper drops that mapping). Aim +
  shoot in two lines: cast, then impulse the returned key.
- `apply_impulse` / `apply_impulse_to_center`, `apply_force` / `apply_force_to_center`,
  `apply_torque`, `apply_angular_impulse` — one-shot and continuous forces (linear and angular) by
  key. All wake the body first (box3d ignores forces on a sleeper), so you never get a silent no-op.
- `linear_velocity(key)` / `angular_velocity(key)` / `mass(key)` — the id tier exposes none of
  these; read them straight from the solver. `mass` scales an impulse to a target launch speed
  (`impulse = mass * velocity`).
- `overlap_sphere(center, radius) -> Vec<u64>` — every glue-spawned body overlapping the sphere,
  deduped, for AOE/proximity logic.

`examples/sandbox-module` drives all of these (raycast shooting, per-body impulses, a pit sensor),
and `examples/sandbox-client` renders it in the browser.

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

### Ticking from a procedure

A scheduled reducer is the default tick driver, but the same tick can run inside a SpacetimeDB
`#[procedure]` — for both mirrored and ephemeral worlds, with **no crate changes**. Procedures
aren't auto-transactional; database work runs inside `ctx.try_with_tx(|tx| …)`, and `TxContext`
derefs to `ReducerContext`, so the whole glue API (`with_world`, `with_world_paced`,
`create_world`) accepts `tx` directly via deref coercion:

```rust
#[spacetimedb::table(accessor = tick_timer, scheduled(tick_proc))]
pub struct TickTimer { #[primary_key] #[auto_inc] scheduled_id: u64, scheduled_at: ScheduleAt, world_key: u64 }

#[spacetimedb::procedure]
fn tick_proc(ctx: &mut ProcedureContext, timer: TickTimer) -> Result<(), String> {
    let wk = timer.world_key; // Copy out: try_with_tx may re-run the closure (see below)
    ctx.try_with_tx(|tx| {
        with_world_paced(tx, wk, rebuild, game).map(|_| ())
    })
}
```

Two rules keep the generation guard as abort-safe as it is under a reducer:

- **The whole guarded tick goes in one `try_with_tx`, and it must be `try_with_tx`, not `with_tx`.**
  `try_with_tx` rolls back on `Err`, so a guard failure discards the eager generation bump and the
  next firing rebuilds — exactly the reducer contract. `with_tx` *always* commits on return, so a
  guard `Err` would persist a bumped generation with no matching step: the guard is defeated.
- **Keep the closure `Fn`-safe.** `try_with_tx` re-invokes the body once if the commit hits a
  conflict, so copy `Copy` values out first and don't move non-`Copy` captures in. A retried
  double-step self-heals anyway (the in-memory generation runs ahead of the rolled-back durable
  stamp → mismatch → rebuild), and a single scheduled-procedure-per-world never conflicts.

Procedures also unlock a scheduler-free **self-pacing loop**: step, release the tx, `ctx.sleep_until`
the next frame, repeat (the tx must never be held across the sleep). Reach for procedure-mode when
the tick needs what reducers forbid — HTTP calls, `sleep_until` — otherwise a scheduled reducer is
simpler and just as fast (the sim cost is identical; procedures only add per-firing transaction
setup). Player actions stay reducers. `examples/procedure-demo` runs the whole pattern.

### Dynamic tick rate

Sleeping already makes cost activity-proportional (asleep bodies are ~free), but an armed 60 Hz
tick still pays a base step and per-firing overhead forever, and slow-moving scenes integrate at
full rate for no visible benefit. An **adaptive `TickPolicy`** clocks the whole thing down —
crate machinery, stored durably with the world definition:

```rust
create_world(ctx, ARENA, &WorldDef {
    tick: TickPolicy::adaptive(
        vec![Tier { dt: 1.0/60.0, substeps: 4 },   // FULL
             Tier { dt: 1.0/60.0, substeps: 2 },   // SLOW: halved solver substeps
             Tier { dt: 1.0/20.0, substeps: 2 }],  // CRAWL: 3× fewer steps/sec on top
        2.0,  // slow_v: below this max speed a scene may demote
        60,   // k_slow: consecutive slow steps before demoting a tier
        30,   // k_quiet: consecutive all-asleep steps before Park
        4),   // max_catchup
    ..WorldDef::default()
})?;
```

Every `with_world_paced` tick then runs the ladder for you: **demote** one tier after `k_slow`
consecutive slow steps, **promote to full instantly** on any fast mover, and once fully asleep
for `k_quiet` steps return **`TickDirective::Park`** — stop your timer, the world costs zero
until something wakes it. Two things stay yours, made one-liner-cheap:

- the scheduled table + tick reducer — generated by `box3d_stdb::tick_schedule!` (the macro also
  wires the Park→delete-timer path and an `ensure` helper);
- calling `ensure` (= `resume_full_rate` + re-arm) at the top of wake-capable reducers, so a
  player action snaps a parked or clocked-down world back to full rate before its impulse lands.
  A merely-*connecting* viewer should arm-only (insert the timer row if missing, no resume) —
  resetting hysteresis on every connection would keep a busy lobby from ever clocking down.

The ceiling is yours too: tier 0 is whatever you declare — a fixed 30 Hz world is just
`TickPolicy::realtime(1.0/30.0, 4, 4)`, and `set_tick_policy` swaps a live world's policy
between rounds (validated, next-firing effective, cache stays valid).

Safety rules the ladder encodes: `dt` changes are tier-stepped and stored durably (never
continuous — fixed-`dt` determinism survives because the tier is transactional state like
everything else), and a bigger-dt tier is tunnel-safe by construction — entry requires max speed
under `slow_v` (2 m/s here), where a 1/20 step moves a body <0.1 m, far below the ~5 m/s ceiling
for this geometry.

Measured (10-ball pile, crate engine): FULL 60.0 steps/s → demote cascade exactly at `k_slow` →
CRAWL 20.0 steps/s sustained → two pile collapses promoted back instantly → parked at zero with
the timer deleted by the generated reducer. Substep decimation alone is **−32% fuel/step** at 16
awake bodies (346k → 236k measured; ~122k/step of broadphase+events is fixed cost); the bottom
tier meters **6× less** work than fixed 60 Hz/4-substep on the same scene.

### Choosing a configuration

The knobs compose — pick by what the world is for:

| Goal | Configuration | What you pay |
| --- | --- | --- |
| **Max sim speed** (lobby matches, cosmetic physics) | `Persistence::Ephemeral` + `with_world_paced` | state resets to spawn on any cache drop; clients need an event-table pipe for positions |
| **Durability** (persistent zones, resumable matches) | `Persistence::Mirrored` (default) | ~1.5–1.8× stepping while bodies are awake (0.3–0.5 µs per moving body per step; sleeping bodies are free — measured 512 asleep bodies stepping at pure call overhead) |
| **Realtime accuracy** | `with_world_paced` on the scheduled tick (both modes) | negligible — one row field + integer math per firing |
| **Tick needs HTTP / self-pacing** | drive the tick from a `#[procedure]` (`try_with_tx`, both modes) | per-firing transaction setup; keep the whole guarded tick in one `try_with_tx` (see *Ticking from a procedure*) |
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
- `examples/procedure-demo` — the same ball, but the 60 Hz tick runs in a `#[procedure]`
  (`try_with_tx` + `with_world_paced`) instead of a scheduled reducer, plus a `sleep_until`
  self-pacing loop. See *Ticking from a procedure*.
- `examples/test-module` + `scripts/test-c-mem.fish` — guard/mirror/poison probes, settle and
  bench reducers, and a 10-assertion C-heap leak suite (every error path frees to exact baseline;
  a poisoned world leaks exactly one world's allocations by design — torn C state is never
  destroyed).
- `examples/sandbox-module` + `examples/sandbox-client` — a multiplayer physics sandbox: spawn
  boxes/balls, shoot them with raycast impulses, score bodies falling into a pit sensor. The
  module shows the gameplay helper tier (`cast_ray_closest`, `apply_impulse`, sensor events) and
  presence-gated ticking; the client is a vanilla three.js page (GitHub Pages-ready via
  `.github/workflows/pages.yml`) subscribed straight to the `public-mirror` table.

## Static memory, and why it's safe here

Live `b3World` objects are C allocations in the module's **wasm linear memory**, held in a fixed
static array (`BOX3D_MAX_WORLDS` slots, default 1024) indexed by world key. Reaching for a static
array like this inside a SpacetimeDB module is normally a latent bug — linear memory sits outside
the transactional datastore and outside every durability guarantee:

- **A reducer abort rolls back your tables but not linear memory.** A panic partway through a
  mutation leaves the C world half-changed while the tables it was meant to match snap back — the
  two silently disagree.
- **Instance memory doesn't survive a republish or restart** — a fresh wasm instance starts with
  the static array zeroed.
- **The host may relocate your module between machines or regions.** On Maincloud an instance
  isn't pinned; a move spins up a fresh instance elsewhere with empty linear memory, and nothing
  carries the old array across. Code that trusted the static array would read state that simply
  isn't there — with no signal that it moved.

So the static array is treated as a **cache that is never authoritative**. The durable record
lives in tables, and every entry into `with_world` re-earns the right to use the cache:

- **Generation stamp (the `b3_world` metadata table).** Each world's durable row carries a
  generation counter alongside its `WorldDef`, tick, and pacing accumulator. A cache slot
  remembers the generation it was built at; on entry the glue compares it to the durable stamp and
  **bumps the stamp eagerly, before stepping**. A cold instance (region move, restart, republish)
  has an empty slot → mismatch → rebuild. An aborted tick rolls the table bump back but *not* the
  in-memory bump, so the next call sees a mismatch → rebuild. Every loss-of-trust path — panic,
  abort, republish, restart, cold start, migration — converges on the same behavior: discard the
  cache, rebuild from tables.
- **Poison discipline.** A world that may have been torn mid-mutation is *abandoned*
  (`mem::forget`), never handed to `b3DestroyWorld` — destroying torn C state through the
  module-global allocator is UB. The leak is bounded and measured (exactly one empty world ≈ 164
  KB per poison event; every non-poison error path frees the C heap back to exact baseline, gated
  by the leak-suite test). The poisoned slot is dropped and the next call rebuilds clean.
- **Rebuild from the mirror.** `b3_body` holds each body's transform, velocity, and sleep flag,
  delta-committed after every step. A rebuild replays your constructive state (shapes/joints, via
  `rebuild_fn`) and overlays these rows, so a world reconstructed on a brand-new instance resumes
  where it left off — a ball republished mid-flight keeps its arc.

The net effect: the static array only ever accelerates the common case. It is validated against a
durable stamp on *every* tick and discarded the instant that stamp disagrees, so the UB-prone
pattern — authoritative static memory in a relocatable cloud module — never actually occurs. The
tables are the truth; the cache is disposable by construction.

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
  `set_transform`/`wake` keep the C world and the mirror consistent in one call), the keyed
  gameplay tier (`cast_ray_closest`/`apply_impulse`/`apply_force`/`overlap_sphere`/velocity
  getters — see *Gameplay queries & forces*), the `b3_world`/`b3_body` tables, and `StepResult`
  events. Also `install_box3d_logging()` — call once from your `init` reducer to route box3d's
  internal warnings (`b3Log`) to the module log.

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
