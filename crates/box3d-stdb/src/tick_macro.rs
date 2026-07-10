/// Generate the tick reducer and arm-helper for a consumer's scheduled-timer table.
///
/// # What it generates
///
/// Given:
/// ```ignore
/// // Consumer declares the table (outside the macro — see Boundary note below):
/// #[spacetimedb::table(accessor = tick_timer, scheduled(tick))]
/// pub struct TickTimer {
///     #[primary_key] #[auto_inc] pub scheduled_id: u64,
///     pub scheduled_at: spacetimedb::ScheduleAt,
///     pub world_key: u64,
/// }
///
/// // Then invokes the macro:
/// box3d_stdb::tick_schedule! {
///     timer:           tick_timer / TickTimer,
///     reducer:         tick,
///     ensure:          ensure_ticking,
///     interval_micros: 16_667,
///     rebuild:         spawn_scene,
///     game:            game_logic,
///     post:            post_tick,
/// }
/// ```
///
/// 1. **Tick reducer** — `fn tick(ctx, timer: TickTimer)` that calls `with_world_paced` and
///    deletes this world's timer rows on `TickDirective::Park`.
///
/// 2. **Arm helper** — `fn ensure_ticking(ctx, world_key)` that calls `resume_full_rate` then
///    inserts a timer row if none exists for that world.
///
/// # Boundary note 
///
/// `#[spacetimedb::table]` cannot be applied inside a `macro_rules!` expansion on stable Rust
/// with spacetimedb 2.6.1: the proc-macro's `query_builder_helper_structs` applies
/// `quote_spanned!(table_ident.span()=>...)` to the fn signature but uses `Span::call_site()`
/// for the body initializers, causing the `_table_name` parameter and its use to resolve in
/// different hygiene universes — "cannot find value `_table_name`". The consumer writes the
/// two-line struct themselves; the macro generates the rest (reducer + ensure).
///
/// # Callback contracts
///
/// - `rebuild: fn(&spacetimedb::ReducerContext, &mut box3d_stdb::WorldCtx<'_>) -> Result<(), String>`
///   — construction only; replayed from your tables after any cache drop; must be deterministic
///   and stable in spawn order.
/// - `game: fn(&spacetimedb::ReducerContext, &mut box3d_stdb::WorldCtx<'_>) -> Result<(), String>`
///   — per-tick logic run before integration (inputs, impulses, mutations).
/// - `post: fn(&spacetimedb::ReducerContext, &box3d_stdb::PacedResult<()>) -> Result<(), String>`
///   — runs after `with_world_paced` returns, receiving the full `PacedResult` so you can do
///   sensor scoring, kill checks, and energy metering (`res.substeps`, `res.steps_run`,
///   `res.events.*`, `res.tier`).
///
/// # Park / re-arm lifecycle
///
/// When physics settles (`TickDirective::Park`), the generated tick reducer deletes all timer rows
/// for that `world_key` — stopping the scheduler. Any reducer that mutates the world (spawns,
/// impulses, player actions) should call `ensure_ticking(ctx, world_key)`, which calls
/// `resume_full_rate` to snap the adaptive tier back to full rate, then re-inserts the timer row
/// if it was deleted.
///
/// # Assumption
///
/// `spacetimedb` is a direct dependency of the consuming module crate. All generated code refers to
/// `spacetimedb::*` unqualified. `box3d_stdb` items are accessed via `$crate::` from within this
/// macro.
///
/// # Full example (the demo-module conversion)
///
/// ```ignore
/// fn spawn_scene(ctx: &spacetimedb::ReducerContext, w: &mut box3d_stdb::WorldCtx<'_>) -> Result<(), String> {
///     // ... spawn ground + ball ...
///     Ok(())
/// }
/// fn game_logic(_ctx: &spacetimedb::ReducerContext, _w: &mut box3d_stdb::WorldCtx<'_>) -> Result<(), String> { Ok(()) }
/// fn post_tick(_ctx: &spacetimedb::ReducerContext, _res: &box3d_stdb::PacedResult<()>) -> Result<(), String> { Ok(()) }
///
/// #[spacetimedb::table(accessor = tick_timer, scheduled(tick))]
/// pub struct TickTimer {
///     #[primary_key] #[auto_inc] pub scheduled_id: u64,
///     pub scheduled_at: spacetimedb::ScheduleAt,
///     pub world_key: u64,
/// }
///
/// box3d_stdb::tick_schedule! {
///     timer:           tick_timer / TickTimer,
///     reducer:         tick,
///     ensure:          ensure_ticking,
///     interval_micros: 16_667,
///     rebuild:         spawn_scene,
///     game:            game_logic,
///     post:            post_tick,
/// }
/// ```
#[macro_export]
macro_rules! tick_schedule {
    (
        timer:           $accessor:ident / $Row:ident,
        reducer:         $reducer:ident,
        ensure:          $ensure:ident,
        interval_micros: $micros:expr,
        rebuild:         $rebuild:expr,
        game:            $game:expr,
        post:            $post:expr $(,)?
    ) => {
        // 1. Tick reducer — scheduler-only; parks by deleting this world's timer rows.
        #[spacetimedb::reducer]
        pub fn $reducer(
            ctx: &spacetimedb::ReducerContext,
            timer: $Row,
        ) -> Result<(), String> {
            if ctx.sender() != ctx.database_identity() {
                return Err("tick may only be called by the scheduler".into());
            }
            let world_key = timer.world_key;
            let res = $crate::with_world_paced(
                ctx,
                world_key,
                |w| $rebuild(ctx, w),
                |w| $game(ctx, w),
            )?;
            $post(ctx, &res)?;
            if res.directive == $crate::TickDirective::Park {
                // Collect-then-delete: mutating the table mid-iteration is undefined territory.
                let ids: ::std::vec::Vec<u64> = ctx
                    .db
                    .$accessor()
                    .iter()
                    .filter(|t| t.world_key == world_key)
                    .map(|t| t.scheduled_id)
                    .collect();
                for id in ids {
                    ctx.db.$accessor().scheduled_id().delete(id);
                }
            }
            Ok(())
        }

        // 2. Arm helper — resume full rate + re-arm.
        pub fn $ensure(
            ctx: &spacetimedb::ReducerContext,
            world_key: u64,
        ) -> Result<(), String> {
            let was_parked = $crate::resume_full_rate(ctx, world_key)?;
            if was_parked {
                // The parked flag is the transactional truth, not row presence: a row seen here
                // may be an orphan the Park delete is racing to remove (or one the scheduler
                // already dropped). Replace wholesale so a resume always yields exactly one
                // freshly registered interval row.
                let stale: ::std::vec::Vec<u64> = ctx
                    .db
                    .$accessor()
                    .iter()
                    .filter(|t| t.world_key == world_key)
                    .map(|t| t.scheduled_id)
                    .collect();
                for id in stale {
                    ctx.db.$accessor().scheduled_id().delete(id);
                }
            }
            if was_parked || !ctx.db.$accessor().iter().any(|t| t.world_key == world_key) {
                ctx.db.$accessor().insert($Row {
                    scheduled_id: 0,
                    scheduled_at: spacetimedb::ScheduleAt::Interval(
                        spacetimedb::TimeDuration::from_micros($micros),
                    ),
                    world_key,
                });
            }
            Ok(())
        }
    };
}
