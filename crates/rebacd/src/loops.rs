//! The daemon's loops: relay, refresh, resource refresh, sweep, heartbeat, Watch and the closure check.

use crate::{config::Mode, project, relay, Ctx};
use futures::{stream, StreamExt};
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::sync::watch;

fn backoff(failures: u32, base: Duration) -> Duration {
    (base * 2u32.saturating_pow(failures.min(6))).min(Duration::from_secs(10))
}

pub async fn relay_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let idle = Duration::from_millis(ctx.config.relay_idle_ms);
    let mut failures = 0u32;
    while !*stop.borrow() {
        let delay = match relay::relay_once(&ctx).await {
            Ok(0) => {
                failures = 0;
                idle
            }
            Ok(_) => {
                failures = 0;
                Duration::ZERO
            }
            Err(e) => {
                failures += 1;
                if failures == 1 {
                    tracing::error!(error = %crate::logs::kind_of(&e), "relay pass failed");
                }
                backoff(failures, idle)
            }
        };
        tokio::select! { () = tokio::time::sleep(delay) => {}, _ = stop.changed() => {} }
    }
    tracing::debug!("relay_loop stopped");
}

pub async fn refresh_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let idle = Duration::from_millis(ctx.config.refresh_idle_ms);
    let n = ctx.config.refresh_concurrency;
    while !*stop.borrow() {
        let worked = match refresh_pass(&ctx, n).await {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %crate::logs::kind_of(&e), "refresh pass failed");
                0
            }
        };
        let delay = if worked > 0 { Duration::ZERO } else { idle };
        tokio::select! { () = tokio::time::sleep(delay) => {}, _ = stop.changed() => {} }
    }
    tracing::debug!("refresh_loop stopped");
}

/// One pass over both refresh queues. Returns how many items it worked.
pub async fn refresh_pass(ctx: &Ctx, concurrency: usize) -> anyhow::Result<usize> {
    // Claim a few rounds of work per pass, so the workers do not wait on the queue between principals.
    let limit = i32::try_from(concurrency.saturating_mul(4)).unwrap_or(i32::MAX);
    let client = ctx.pool.get().await?;
    let principals = client
        .query(
            "select tenant, principal, zedtoken, generation from rebac.claim_refresh($1, $2)",
            &[&limit, &ctx.config.refresh_lease_ms],
        )
        .await?;
    let resources = client
        .query(
            "select tenant, resource, zedtoken, generation from rebac.claim_refresh_resource($1, $2)",
            &[&limit, &ctx.config.refresh_lease_ms],
        )
        .await?;
    drop(client);
    let worked = principals.len() + resources.len();
    refresh_principals(ctx, principals, concurrency).await;
    refresh_resources(ctx, resources).await?;
    Ok(worked)
}

/// Re-project each claimed principal, `concurrency` at a time, and record the outcome on its queue row.
async fn refresh_principals(ctx: &Ctx, claimed: Vec<tokio_postgres::Row>, concurrency: usize) {
    stream::iter(claimed)
        .for_each_concurrent(concurrency, |row| async move {
            let (tenant, principal, token, generation): (String, String, Option<String>, i64) =
                (row.get(0), row.get(1), row.get(2), row.get(3));
            let result = project::refresh_one(
                ctx,
                &tenant,
                &principal,
                token.as_deref(),
                ctx.config.deadline(),
            )
            .await;
            let Ok(db) = ctx.pool.get().await else { return };
            let recorded = match result {
                Ok(_) => {
                    db.execute(
                        "select rebac.complete_refresh($1, $2, $3)",
                        &[&tenant, &principal, &generation],
                    )
                    .await
                }
                Err(e) => {
                    db.execute(
                        "select rebac.fail_refresh($1, $2, $3)",
                        &[&tenant, &principal, &e.to_string()],
                    )
                    .await
                }
            };
            if let Err(e) = recorded {
                tracing::error!(error = %crate::logs::kind(&e), "could not record a refresh outcome");
            }
        })
        .await;
}

/// Ask SpiceDB who views each claimed resource now, and queue those readers (`rebac.complete_refresh_resource`).
async fn refresh_resources(ctx: &Ctx, claimed: Vec<tokio_postgres::Row>) -> anyhow::Result<()> {
    if claimed.is_empty() {
        return Ok(());
    }
    // With grant nodes on, a resource's readers include the subject sets that view it: their nodes are re-projected
    // too (an arrow tuple moving a file under a space a group reads, say).
    let sets: Vec<(String, String)> = ctx
        .pool
        .get()
        .await?
        .query(
            "select resource_type, relation from rebac.view_relation where kind = 'set' and rebac.nodes_on()
             order by 1, 2",
            &[],
        )
        .await?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    for row in claimed {
        let (tenant, resource, token, generation): (String, String, Option<String>, i64) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        let found = viewers(ctx, &resource, &sets, token.as_deref()).await;
        let db = ctx.pool.get().await?;
        match found {
            Ok(subjects) => {
                db.execute(
                    "select rebac.complete_refresh_resource($1, $2, $3, $4, $5)",
                    &[&tenant, &resource, &generation, &subjects, &token],
                )
                .await?;
            }
            Err(e) => {
                tracing::warn!(error = %crate::logs::kind(&e), "a resource's readers could not be looked up; retried");
                db.execute(
                    "select rebac.fail_refresh_resource($1, $2)",
                    &[&tenant, &resource],
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Who views a resource now, per SpiceDB: the subjects of the projected subject type, and the subject sets of each
/// `(type, relation)` in `sets` (`group:acme/eng#member`).
async fn viewers(
    ctx: &Ctx,
    resource: &str,
    sets: &[(String, String)],
    token: Option<&str>,
) -> Result<Vec<String>, tonic::Status> {
    let perm = &ctx.projection.permission;
    let deadline = ctx.config.deadline();
    let mut out = ctx
        .spicedb
        .lookup_subjects(
            resource,
            perm,
            &ctx.projection.subject_type,
            None,
            token,
            deadline,
        )
        .await?;
    for (ty, rel) in sets {
        out.extend(
            ctx.spicedb
                .lookup_subjects(resource, perm, ty, Some(rel), token, deadline)
                .await?,
        );
    }
    Ok(out)
}

pub async fn sweep_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    // How often a replica looks whether the sweep is due (rebac.sweep decides, from sweep_interval_ms): at most a minute.
    let check = Duration::from_millis(
        u64::try_from(ctx.config.sweep_interval_ms).map_or(60_000, |ms| ms.min(60_000)),
    );
    let requeue = ctx.config.mode == Mode::Live;
    loop {
        tokio::select! { () = tokio::time::sleep(check) => {}, _ = stop.changed() => {} }
        if *stop.borrow() {
            return;
        }
        if !crate::leader::is_leader(&ctx) {
            continue;
        }
        let r = async {
            let c = ctx.pool.get().await?;
            let row = c
                .query_one(
                    "select requeued, principals from rebac.sweep($1, $2, $3)",
                    &[
                        &ctx.config.sweep_interval_ms,
                        &ctx.config.sweep_settle_ms,
                        &requeue,
                    ],
                )
                .await?;
            anyhow::Ok((row.get::<_, i64>(0), row.get::<_, i64>(1)))
        }
        .await;
        match r {
            Ok((0, 0)) => {}
            Ok((requeued, principals)) => tracing::info!(requeued, principals, "permission sweep"),
            Err(e) => tracing::error!(error = %crate::logs::kind_of(&e), "permission sweep failed"),
        }
    }
}

/// The heartbeat the staleness bound reads. Beaten only while SpiceDB answers and Postgres takes the write, so a
/// daemon that is down or cut off from SpiceDB stops beating and reads fail closed once the bound passes. It does NOT
/// require the Watch stream: a dropped stream is judged per principal in SQL (`rebac.watch_ok`), so only the principals
/// not looked up since the stream was last current fail closed, after `watch_grace_ms`.
pub async fn heartbeat_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let every = Duration::from_millis(ctx.config.heartbeat_ms);
    let mut was_ok = true;
    while !*stop.borrow() {
        let ok = match ctx
            .spicedb
            .read_schema(Duration::from_millis(ctx.config.heartbeat_ms.max(500) * 2))
            .await
        {
            Ok(_) => match ctx.pool.get().await {
                Ok(c) => c.execute("select rebac.heartbeat('*')", &[]).await.is_ok(),
                Err(_) => false,
            },
            Err(_) => false,
        };
        if ok != was_ok {
            if ok {
                tracing::info!("heartbeat resumed");
            } else {
                tracing::warn!("heartbeat stopped: SpiceDB or Postgres unavailable; reads fail closed past the bound");
            }
            was_ok = ok;
        }
        tokio::select! { () = tokio::time::sleep(every) => {}, _ = stop.changed() => {} }
    }
    tracing::debug!("heartbeat_loop stopped");
}

/// Watch, applied. Follows SpiceDB's changes to the closure's types and applies the ones that did not come through
/// `rebac.write` (another service, the zed CLI) as if they had (`rebac.apply_watch`): a revocation is narrowed in the
/// same transaction that records the stream's position, under the commit's lock order; a grant is a refresh.
///
/// While the stream is down, or a message cannot be applied, `Ctx::watch_current` is false and
/// `watch_state.current_at` stops moving, so `rebac.watch_ok` fails closed every principal not looked up since, once
/// `watch_grace_ms` passes. A reconnect resumes from the applied position, which replays
/// what was missed. A position that cannot be resumed (none stored, refused by SpiceDB, or older than
/// `watch_resume_max_age_ms`, past which SpiceDB may have garbage-collected changes without saying so) restarts from
/// SpiceDB's head and records a gap: every set looked up before it fails closed until the sweep it forces re-projects
/// it. In shadow mode changes are only counted.
pub async fn watch_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let mut failures = 0u32;
    while !*stop.borrow() {
        if !crate::leader::is_leader(&ctx) {
            ctx.watch_current.store(false, Ordering::SeqCst);
            tokio::select! { () = tokio::time::sleep(Duration::from_millis(200)) => {}, _ = stop.changed() => {} }
            continue;
        }
        let r = watch_once(&ctx, &mut stop).await;
        ctx.watch_current.store(false, Ordering::SeqCst);
        if *stop.borrow() {
            return;
        }
        failures += 1;
        match r {
            Ok(Resume::Reset) => {
                failures = 0;
                continue;
            }
            Ok(Resume::Same) => {}
            Err(e) => {
                if failures == 1 {
                    tracing::warn!(error = %crate::logs::kind_of(&e), "watch stream ended; reconnecting (reads fail closed meanwhile)");
                }
            }
        }
        tokio::select! { () = tokio::time::sleep(backoff(failures, Duration::from_millis(500))) => {}, _ = stop.changed() => {} }
    }
    tracing::debug!("watch_loop stopped");
}

/// How the next stream opens.
enum Resume {
    /// From the applied position, as stored.
    Same,
    /// The position was dropped: from SpiceDB's head, with a gap, at once.
    Reset,
}

/// An error that says the position itself cannot be resumed (as opposed to SpiceDB being unreachable): restart from
/// head. SpiceDB refuses a stale or foreign revision with one of these codes; an outage is anything else.
#[must_use]
pub fn position_refused(s: &tonic::Status) -> bool {
    use tonic::Code;
    matches!(
        s.code(),
        Code::InvalidArgument | Code::OutOfRange | Code::FailedPrecondition | Code::NotFound
    )
}

/// Where this daemon keeps its Watch position: a shadow daemon never moves the live one's.
fn scope(ctx: &Ctx) -> &'static str {
    match ctx.config.mode {
        Mode::Live => "watch_applied",
        Mode::Shadow => "watch_shadow",
    }
}

/// The position to resume from, or None when it must restart from head.
async fn resume_position(ctx: &Ctx) -> anyhow::Result<Option<String>> {
    // Clamped to about 30,000 years, so the interval it makes in SQL cannot overflow.
    let max_age_ms = i64::try_from(ctx.config.watch_resume_max_age_ms)
        .unwrap_or(i64::MAX)
        .min(1_000_000_000_000_000);
    let row = ctx
        .pool
        .get()
        .await?
        .query_opt(
            "select zedtoken, coalesce(now() - watched_at <= $2::bigint * interval '1 millisecond', false)
             from rebac.watch_state where scope = $1",
            &[&scope(ctx), &max_age_ms],
        )
        .await?;
    let Some(row) = row else { return Ok(None) };
    let (token, recent): (Option<String>, bool) = (row.get(0), row.get(1));
    Ok(match token.filter(|t| !t.is_empty()) {
        Some(t) if recent => Some(t),
        Some(_) => {
            tracing::warn!(
                "the Watch position is older than watch_resume_max_age_ms: restarting from head (sets looked up before fail closed until re-projected)"
            );
            None
        }
        None => None,
    })
}

/// Every type is watched (an empty filter). `rebac.apply_watch` skips relations outside the closure, and watching all of
/// them means every revision past a stored position carries something the stream delivers, which is what lets a
/// resumed stream compare its position with the head exactly (an idle store's head is its last delivered revision).
fn types_watched() -> Vec<String> {
    Vec::new()
}

/// Make sure a Watch position is stored, restarting from head (with a gap) when none is. The backfill calls this
/// before it projects anything, so a daemon started after it resumes from there and never needs the gap.
pub async fn ensure_watch_position(ctx: &Ctx) -> anyhow::Result<()> {
    if !ctx.config.watch || ctx.config.mode != Mode::Live || resume_position(ctx).await?.is_some() {
        return Ok(());
    }
    // From SpiceDB's head revision, read directly: an idle store may send a new stream nothing to take it from.
    let token = head_token(ctx).await?;
    ctx.pool
        .get()
        .await?
        .execute("select rebac.watch_restarted($1)", &[&token])
        .await?;
    Ok(())
}

/// SpiceDB's head revision as a ZedToken, read-only (a fully consistent CheckPermission of a probe object; its answer
/// is ignored).
async fn head_token(ctx: &Ctx) -> anyhow::Result<String> {
    let p = &ctx.projection;
    let probe = |t: &str| format!("{t}:__rebac_head_probe");
    Ok(ctx
        .spicedb
        .head_revision(
            &probe(&p.resource_type),
            &p.permission,
            &probe(&p.subject_type),
            ctx.config.deadline(),
        )
        .await?)
}

/// The stream is caught up: current as of when the head was read (`opened`), not now, since what was committed after
/// it may still be in flight.
async fn mark_caught_up(ctx: &Ctx, opened: std::time::Instant) -> anyhow::Result<()> {
    ctx.watch_current.store(true, Ordering::SeqCst);
    let since_ms = opened.elapsed().as_secs_f64() * 1000.0;
    crate::leader::fenced_execute(
        ctx,
        "select rebac.watch_alive(now() - $1 * interval '1 millisecond')",
        &[&since_ms],
    )
    .await?;
    Ok(())
}

/// Where a reopened stream counts as caught up: past a head revision, or at the marker's own revision.
enum CatchUp {
    Past(crate::revision::Revision),
    Marker(String),
}

impl CatchUp {
    fn reached(&self, token: Option<&str>) -> bool {
        let Some(token) = token else { return false };
        match self {
            CatchUp::Marker(m) => token == m,
            CatchUp::Past(head) => crate::revision::of_token(token)
                .and_then(|r| crate::revision::covers(&r, head))
                .unwrap_or(false),
        }
    }
}

async fn catch_up_target(ctx: &Ctx) -> anyhow::Result<CatchUp> {
    if ctx.config.watch_marker {
        return Ok(CatchUp::Marker(write_marker(ctx).await?));
    }
    let head = head_token(ctx).await?;
    match crate::revision::of_token(&head) {
        Some(r) => Ok(CatchUp::Past(r)),
        None => anyhow::bail!(
            "cannot read a revision from SpiceDB's ZedToken, so a resumed Watch stream cannot tell when its replay is done; \
             set watch_marker = true (reads stay closed meanwhile)"
        ),
    }
}

/// The object id of the marker relationship the Watch loop touches (`watch_marker = true`) to learn where a replay ends. It carries no tenant
/// prefix, so it maps to nobody (G5); the loop also drops it before `rebac.apply_watch` sees it.
pub const WATCH_MARKER: &str = "__rebac_watch_marker";

/// Touch `<resource_type>:__rebac_watch_marker#<a direct relation>@<subject_type>:__rebac_watch_marker` and return the
/// token SpiceDB wrote it at. A TOUCH of an unchanged relationship still writes a revision, and Watch reports it.
async fn write_marker(ctx: &Ctx) -> anyhow::Result<String> {
    let p = &ctx.projection;
    let rel: Option<String> = ctx
        .pool
        .get()
        .await?
        .query_opt(
            "select relation from rebac.view_relation where resource_type = $1 and kind = 'direct' order by relation limit 1",
            &[&p.resource_type],
        )
        .await?
        .map(|r| r.get(0));
    let rel = rel.ok_or_else(|| {
        anyhow::anyhow!(
            "the closure names no direct relation on {} for the Watch marker",
            p.resource_type
        )
    })?;
    let op = crate::spicedb::TupleOp {
        touch: true,
        resource: format!("{}:{WATCH_MARKER}", p.resource_type),
        relation: rel.clone(),
        subject: format!("{}:{WATCH_MARKER}", p.subject_type),
    };
    ctx.spicedb
        .write(&[op], ctx.config.deadline())
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "SpiceDB refused the Watch marker {}:{WATCH_MARKER}#{rel}@{}:{WATCH_MARKER} ({:?}); the relation must accept {} directly",
                p.resource_type, p.subject_type, e.code(), p.subject_type
            )
        })?
        .ok_or_else(|| anyhow::anyhow!("SpiceDB wrote the Watch marker at no token"))
}

/// One Watch message as `rebac.apply_watch`'s parallel arrays.
#[derive(Debug, Default)]
struct WatchRows {
    ops: Vec<&'static str>,
    resources: Vec<String>,
    relations: Vec<String>,
    subjects: Vec<String>,
}

/// One Watch message as `rebac.apply_watch`'s arrays. A TOUCH carrying a caveat or an expiry is applied as a removal
/// (fail closed, then refreshed): the closure refuses both, so only a schema the daemon has not re-checked yet can
/// produce one.
fn watch_rows(msg: &crate::authzed::api::v1::WatchResponse) -> WatchRows {
    use crate::authzed::api::v1::relationship_update::Operation;
    let mut rows = WatchRows::default();
    for update in &msg.updates {
        let Some(rel) = &update.relationship else {
            continue;
        };
        let (Some(resource), Some(subject)) = (&rel.resource, &rel.subject) else {
            continue;
        };
        let Some(object) = &subject.object else {
            continue;
        };
        if resource.object_id == WATCH_MARKER {
            continue;
        }
        let narrows = update.operation == Operation::Delete as i32
            || rel.optional_caveat.is_some()
            || rel.optional_expires_at.is_some();
        rows.ops.push(if narrows { "delete" } else { "touch" });
        rows.resources
            .push(format!("{}:{}", resource.object_type, resource.object_id));
        rows.relations.push(rel.relation.clone());
        rows.subjects.push(if subject.optional_relation.is_empty() {
            format!("{}:{}", object.object_type, object.object_id)
        } else {
            format!(
                "{}:{}#{}",
                object.object_type, object.object_id, subject.optional_relation
            )
        });
    }
    rows
}

/// The position the next stream opens from. A live daemon with none to resume restarts from SpiceDB's head revision,
/// read directly, and records the gap before anything is applied, so the stream it opens is an ordinary resumed one
/// (an idle store may send a stream opened at "now" nothing at all, not even the reply that completes the open). A
/// shadow daemon with none opens at "now".
async fn start_position(ctx: &Ctx, apply: bool) -> anyhow::Result<Option<String>> {
    let from = resume_position(ctx).await?;
    if from.is_some() || !apply {
        return Ok(from);
    }
    let head = head_token(ctx).await?;
    crate::leader::fenced_execute(ctx, "select rebac.watch_restarted($1)", &[&head]).await?;
    tracing::warn!("Watch restarted from SpiceDB's head: sets looked up before now fail closed until the forced sweep re-projects them");
    Ok(Some(head))
}

/// When a stream is current. A resumed stream first replays what happened while it was down, and nothing in a Watch
/// message says where that replay ends (SpiceDB sends a checkpoint after every revision it delivers, replayed or not).
/// So the daemon reads SpiceDB's head revision first (a fully consistent CheckPermission, read-only) and counts the
/// stream current only once a message's position is at or past it (`crate::revision`): Watch delivers in revision
/// order, so by then every change committed before the stream opened has been delivered and, in order, applied.
/// Until then `watch_state.current_at` does not move, so the replay is never taken for currency. `watch_marker =
/// true` instead touches a marker relationship once the stream is open and waits for its revision, for a datastore
/// whose revisions `crate::revision` cannot compare.
///
/// An idle store sends a resumed stream nothing at all, not even the reply that completes the open (SpiceDB's
/// in-memory datastore sends no idle checkpoint). Its head is then the stored position itself, so the stream is
/// current at once, before the open completes. Every type is watched (`rebac.apply_watch` skips relations outside the
/// closure), so any revision past the stored position carries a change the stream delivers: a head past the position
/// always means a replay to wait for, never a quiet store.
struct Currency {
    apply: bool,
    caught_up: bool,
    /// When the head was read: a caught-up stream is current as of then.
    opened: std::time::Instant,
    target: Option<CatchUp>,
}

impl Currency {
    async fn open(ctx: &Ctx, apply: bool, from: Option<&str>) -> anyhow::Result<Self> {
        let mut currency = Self {
            apply,
            caught_up: false,
            opened: std::time::Instant::now(),
            target: None,
        };
        if apply && !ctx.config.watch_marker {
            currency.target = Some(catch_up_target(ctx).await?);
        }
        currency.observe(ctx, from).await?;
        Ok(currency)
    }

    /// With `watch_marker`, the marker is touched once the stream is open, so its revision is one the stream delivers.
    async fn stream_opened(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        if self.apply && self.target.is_none() {
            self.target = Some(catch_up_target(ctx).await?);
        }
        Ok(())
    }

    /// Caught up and applying: `rebac.watch_alive` is told on every tick.
    fn beating(&self) -> bool {
        self.apply && self.caught_up
    }

    /// Still replaying: the catch-up deadline applies.
    fn replaying(&self) -> bool {
        self.apply && !self.caught_up
    }

    /// The stream has applied everything through `token`.
    async fn observe(&mut self, ctx: &Ctx, token: Option<&str>) -> anyhow::Result<()> {
        if !self.caught_up && self.target.as_ref().is_some_and(|t| t.reached(token)) {
            self.caught_up = true;
            mark_caught_up(ctx, self.opened).await?;
            tracing::debug!(
                "Watch caught up: the stream is at or past the revision read when it opened"
            );
        }
        Ok(())
    }
}

/// Connected, and everything delivered is applied: the position is current as of now.
async fn beat(ctx: &Ctx) -> anyhow::Result<()> {
    if !crate::leader::is_leader(ctx) {
        anyhow::bail!("no longer the leader: stopping the Watch stream");
    }
    crate::leader::fenced_execute(ctx, "select rebac.watch_alive()", &[]).await?;
    Ok(())
}

/// Apply one message in one transaction with its position, fenced (`rebac.apply_watch`). Returns the position.
async fn apply_message(
    ctx: &Ctx,
    msg: &crate::authzed::api::v1::WatchResponse,
    apply: bool,
) -> anyhow::Result<Option<String>> {
    let token = msg.changes_through.as_ref().map(|t| t.token.clone());
    let rows = watch_rows(msg);
    let mut db = ctx.pool.get().await?;
    let tx = db.transaction().await?;
    crate::leader::assert_leader(ctx, &tx).await?;
    let row = tx
        .query_one(
            "select external, narrowed from rebac.apply_watch($1, $2, $3, $4, $5, $6)",
            &[
                &rows.ops,
                &rows.resources,
                &rows.relations,
                &rows.subjects,
                &token,
                &apply,
            ],
        )
        .await?;
    tx.commit().await?;
    let (external, narrowed): (i32, i32) = (row.get(0), row.get(1));
    if external > 0 {
        tracing::info!(
            external,
            narrowed,
            "applied relationship changes not made through rebac.write"
        );
    }
    Ok(token)
}

async fn watch_once(ctx: &Ctx, stop: &mut watch::Receiver<bool>) -> anyhow::Result<Resume> {
    let apply = ctx.config.mode == Mode::Live;
    let from = start_position(ctx, apply).await?;
    // Only a stream resumed from a stored position can have that position refused.
    let resumed = from.is_some();
    let mut currency = Currency::open(ctx, apply, from.as_deref()).await?;
    let mut alive = tokio::time::interval(Duration::from_millis(ctx.config.heartbeat_ms.max(100)));
    let open = ctx.spicedb.watch(types_watched(), from);
    tokio::pin!(open);
    let opened = loop {
        tokio::select! {
            s = &mut open => break s,
            _ = alive.tick(), if currency.beating() => beat(ctx).await?,
            _ = stop.changed() => return Ok(Resume::Same),
        }
    };
    let mut stream = match opened {
        Ok(s) => s,
        Err(e) if resumed && position_refused(&e) => return drop_position(ctx, &e).await,
        Err(e) => return Err(e.into()),
    };
    currency.stream_opened(ctx).await?;
    let catch_up_within = 6 * ctx.config.deadline();
    let catch_up_by = tokio::time::Instant::now() + catch_up_within;
    let mut first = true;
    loop {
        let msg = tokio::select! {
            m = stream.message() => m,
            _ = alive.tick(), if currency.beating() => {
                beat(ctx).await?;
                continue;
            }
            () = tokio::time::sleep_until(catch_up_by), if currency.replaying() => {
                anyhow::bail!("Watch did not reach the revision read when it opened within {catch_up_within:?}; reopening (reads stay closed)");
            }
            _ = stop.changed() => return Ok(Resume::Same),
        };
        let msg = match msg {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(Resume::Same),
            // A stale position may only be refused on the stream's first message.
            Err(e) if first && resumed && position_refused(&e) => {
                return drop_position(ctx, &e).await
            }
            Err(e) => return Err(e.into()),
        };
        first = false;
        let token = apply_message(ctx, &msg, apply).await?;
        currency.observe(ctx, token.as_deref()).await?;
        if msg.schema_updated {
            // The closure loop re-derives it; this stream ends so it reopens on the types the new closure names.
            tracing::warn!("the SpiceDB schema changed: re-checking the closure");
            ctx.closure_kick.notify_one();
            return Ok(Resume::Same);
        }
    }
}

/// SpiceDB refused the stored position: forget it, so the next stream starts from head and records the gap.
async fn drop_position(ctx: &Ctx, e: &tonic::Status) -> anyhow::Result<Resume> {
    tracing::warn!(code = ?e.code(), "SpiceDB refused the Watch position: restarting from head");
    crate::leader::fenced_execute(
        ctx,
        "update rebac.watch_state set zedtoken = null where scope = $1",
        &[&scope(ctx)],
    )
    .await?;
    Ok(Resume::Reset)
}

/// The closure, kept. Re-reads the live schema every `closure_check_ms`, and at once when Watch reports a schema
/// change; when its text differs from the last one checked (or the closure is invalid from before), it re-derives
/// the closure (`closure::recheck`): unsupported, reads fail closed; changed, every tenant is rebuilt while reads fail
/// closed. Not tied to Watch, so a deployment with `watch = false`, or a dropped stream, still notices.
pub async fn closure_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let every = Duration::from_millis(ctx.config.closure_check_ms.max(500));
    // None: the first pass re-derives once (cheap when nothing changed), so a change between the start's check and
    // this loop is not missed.
    let mut last: Option<String> = None;
    let mut unsupported = false;
    let mut failures = 0u32;
    while !*stop.borrow() {
        tokio::select! {
            () = tokio::time::sleep(every) => {},
            () = ctx.closure_kick.notified() => {},
            _ = stop.changed() => {},
        }
        if *stop.borrow() {
            break;
        }
        if !crate::leader::is_leader(&ctx) {
            continue;
        }
        let r = async {
            let text = ctx.spicedb.read_schema(ctx.config.deadline()).await?;
            let invalid = ctx.config.mode == Mode::Live
                && crate::closure::closure_invalid(&ctx.pool.get().await?).await?;
            // Unchanged text: nothing to do, unless an earlier rebuild left the closure invalid (then retry it). An
            // unsupported schema stays invalid until its text changes.
            if last.as_deref() == Some(text.as_str()) && (!invalid || unsupported) {
                return anyhow::Ok(());
            }
            let outcome = crate::closure::recheck(&ctx).await?;
            tracing::info!(?outcome, "closure re-checked");
            unsupported = outcome == crate::closure::Recheck::Unsupported;
            last = Some(text);
            anyhow::Ok(())
        }
        .await;
        match r {
            Ok(()) => failures = 0,
            Err(e) => {
                failures += 1;
                if failures == 1 {
                    tracing::error!(error = %crate::logs::kind_of(&e), "closure re-check failed; retrying");
                }
            }
        }
    }
    tracing::debug!("closure_loop stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authzed::api::v1 as pb;

    fn update(op: pb::relationship_update::Operation, caveat: bool) -> pb::RelationshipUpdate {
        pb::RelationshipUpdate {
            operation: op as i32,
            relationship: Some(pb::Relationship {
                resource: Some(pb::ObjectReference {
                    object_type: "container".into(),
                    object_id: "acme/a".into(),
                }),
                relation: "member".into(),
                subject: Some(pb::SubjectReference {
                    object: Some(pb::ObjectReference {
                        object_type: "group".into(),
                        object_id: "acme/eng".into(),
                    }),
                    optional_relation: "member".into(),
                }),
                optional_caveat: caveat.then(|| pb::ContextualizedCaveat {
                    caveat_name: "ip".into(),
                    context: None,
                }),
                optional_expires_at: None,
            }),
        }
    }

    #[test]
    fn watch_rows_map_operations_and_fail_closed_on_conditions() {
        use pb::relationship_update::Operation;
        let msg = pb::WatchResponse {
            updates: vec![
                update(Operation::Touch, false),
                update(Operation::Delete, false),
                update(Operation::Create, false),
                update(Operation::Touch, true),
            ],
            ..Default::default()
        };
        let rows = watch_rows(&msg);
        assert_eq!(rows.ops, ["touch", "delete", "touch", "delete"]);
        assert_eq!(rows.resources[0], "container:acme/a");
        assert_eq!(rows.relations[0], "member");
        assert_eq!(rows.subjects[0], "group:acme/eng#member");
    }

    #[test]
    fn the_watch_marker_never_reaches_apply_watch() {
        use pb::relationship_update::Operation;
        let mut marker = update(Operation::Touch, false);
        if let Some(r) = marker.relationship.as_mut() {
            r.resource.as_mut().unwrap().object_id = WATCH_MARKER.into();
        }
        let msg = pb::WatchResponse {
            updates: vec![marker, update(Operation::Delete, false)],
            ..Default::default()
        };
        let rows = watch_rows(&msg);
        assert_eq!(rows.ops, ["delete"]);
        assert_eq!(rows.resources, ["container:acme/a"]);
    }

    #[test]
    fn a_refused_position_is_told_from_an_outage() {
        use tonic::{Code, Status};
        for c in [
            Code::InvalidArgument,
            Code::OutOfRange,
            Code::FailedPrecondition,
            Code::NotFound,
        ] {
            assert!(position_refused(&Status::new(c, "x")), "{c:?}");
        }
        for c in [
            Code::Unavailable,
            Code::DeadlineExceeded,
            Code::Internal,
            Code::Unknown,
            Code::Cancelled,
        ] {
            assert!(!position_refused(&Status::new(c, "x")), "{c:?}");
        }
    }
}
