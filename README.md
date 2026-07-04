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
box3d-stdb = { git = "https://github.com/<you>/box3d-stdb" }

# Reroute the wrapper's box3d-sys to the wasm-ready build in this repo.
# Cargo only honors [patch] in the workspace root manifest.
[patch.crates-io]
box3d-sys = { git = "https://github.com/<you>/box3d-stdb" }
```

Register the world once, then one guarded call per tick — reconcile the cache, run your game
logic, step, commit:

```rust
use box3d::{BodyDef, Vec3};
use box3d_stdb::{create_world, with_world, WorldDef};

// once, e.g. match setup — stores the definition (gravity -10z, mirrored);
// the C world itself is built lazily by the first with_world call
create_world(ctx, world_key, &WorldDef::default())?;

#[spacetimedb::reducer]
fn tick(ctx: &ReducerContext, timer: TickTimer) -> Result<(), String> {
    let res = with_world(ctx, timer.world_key, 1.0 / 60.0, 4,
        // rebuild: construction only — recreate bodies from YOUR tables after a cache drop;
        // the glue restores each body's transform/velocity from its mirror row
        |w| { w.spawn(BALL, BodyDef::dynamic_at(Vec3::new(0.0, 0.0, 5.0)))?; Ok(()) },
        // game: per-tick logic, before integration
        |w| { /* inputs, impulses, spawns */ Ok(()) },
    )?;
    // res.events: simulation moves + contact/sensor/hit events from this step
    Ok(())
}
```

Body state lives in the private `b3_body` mirror table (transform, velocities, sleep flag) —
expose it to clients through your own `#[view]` (projected/filtered as you like) or flip the
`public-mirror` feature for whole-table subscription. Raw `box3d` API stays available
(`pub use box3d`, plus a `w.world()` escape hatch inside closures).

### Ephemeral worlds

Worlds that don't need durable physics (lobby matches, cosmetic sims) opt out of the mirror
entirely with `persistence: Persistence::Ephemeral` in their `WorldDef` — stepping cost drops
to raw box3d
(measured: 87 ms ephemeral vs 88 ms raw vs 138 ms mirrored, 512 bodies × 300 steps). The
abort-safety guard still applies; a cache drop just respawns the world fresh. Per-tick positions
come back in `StepResult.events.moves` — pipe them to a broadcast event table for clients
(`examples/ephemeral-demo` shows the pattern). Persistence is fixed at world creation.

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
  `set_transform` keep the C world and the mirror consistent in one call), the `b3_world`/`b3_body`
  tables, and `StepResult` events. Also `install_box3d_logging()` — call once from your `init`
  reducer to route box3d's internal warnings (`b3Log`) to the module log.

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
