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
        tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = stop.changed() => {} }
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
        tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = stop.changed() => {} }
    }
    tracing::debug!("refresh_loop stopped");
}

/// One pass over both refresh queues. Returns how many items it worked.
pub async fn refresh_pass(ctx: &Ctx, concurrency: usize) -> anyhow::Result<usize> {
    let client = ctx.pool.get().await?;
    let claimed = client
        .query(
            "select tenant, principal, zedtoken, generation from rebac.claim_refresh($1, $2)",
            &[&((concurrency * 4) as i32), &ctx.config.refresh_lease_ms],
        )
        .await?;
    let resources = client
        .query(
            "select tenant, resource, zedtoken, generation from rebac.claim_refresh_resource($1, $2)",
            &[&((concurrency * 4) as i32), &ctx.config.refresh_lease_ms],
        )
        .await?;
    drop(client);
    let worked = claimed.len() + resources.len();
    stream::iter(claimed)
        .for_each_concurrent(concurrency, |row| async move {
            let (t, p, tok, gen): (String, String, Option<String>, i64) =
                (row.get(0), row.get(1), row.get(2), row.get(3));
            let result =
                project::refresh_one(ctx, &t, &p, tok.as_deref(), ctx.config.deadline()).await;
            let Ok(c) = ctx.pool.get().await else { return };
            let r = match result {
                Ok(_) => {
                    c.execute("select rebac.complete_refresh($1, $2, $3)", &[&t, &p, &gen])
                        .await
                }
                Err(e) => {
                    c.execute(
                        "select rebac.fail_refresh($1, $2, $3)",
                        &[&t, &p, &e.to_string()],
                    )
                    .await
                }
            };
            if let Err(e) = r {
                tracing::error!(error = %crate::logs::kind(&e), "could not record a refresh outcome");
            }
        })
        .await;
    // With grant nodes on, a resource's readers include the subject sets that view it: their nodes are re-projected
    // too (an arrow tuple moving a file under a space a group reads, say).
    let sets: Vec<(String, String)> = if resources.is_empty() {
        vec![]
    } else {
        ctx.pool
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
            .collect()
    };
    for row in resources {
        let (t, res, tok, gen): (String, String, Option<String>, i64) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        let subjects = viewers(ctx, &res, &sets, tok.as_deref()).await;
        let c = ctx.pool.get().await?;
        match subjects {
            Ok(s) => {
                c.execute(
                    "select rebac.complete_refresh_resource($1, $2, $3, $4, $5)",
                    &[&t, &res, &gen, &s, &tok],
                )
                .await?;
            }
            Err(_) => {
                c.execute("select rebac.fail_refresh_resource($1, $2)", &[&t, &res])
                    .await?;
            }
        }
    }
    Ok(worked)
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
    let check = Duration::from_millis((ctx.config.sweep_interval_ms as u64).min(60_000));
    let requeue = ctx.config.mode == Mode::Live;
    loop {
        tokio::select! { _ = tokio::time::sleep(check) => {}, _ = stop.changed() => {} }
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

/// THE HEARTBEAT the staleness bound reads. Beaten only while SpiceDB answers and Postgres takes the write, so a
/// daemon that is down or cut off from SpiceDB stops beating and reads fail closed once the bound passes. It does NOT
/// require the Watch stream: a dropped stream is judged per principal in SQL (rebac.watch_ok), so only the principals
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
        tokio::select! { _ = tokio::time::sleep(every) => {}, _ = stop.changed() => {} }
    }
    tracing::debug!("heartbeat_loop stopped");
}

/// WATCH, APPLIED. Follows SpiceDB's changes to the closure's types and applies the ones that did not come through
/// `rebac.write` (another service, the zed CLI) as if they had (`rebac.apply_watch`): a revocation is narrowed in the
/// same transaction that records the stream's position, under the commit's lock order; a grant is a refresh.
///
/// While the stream is down, or a message cannot be applied, `Ctx::watch_current` is false and the heartbeat stops,
/// so reads fail closed once `max_staleness_ms` passes. A reconnect resumes from the applied position, which replays
/// what was missed. A position that cannot be resumed (none stored, refused by SpiceDB, or older than
/// `watch_resume_max_age_ms`, past which SpiceDB may have garbage-collected changes without saying so) restarts from
/// SpiceDB's head and records a gap: every set looked up before it fails closed until the sweep it forces re-projects
/// it. In shadow mode changes are only counted.
pub async fn watch_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let mut failures = 0u32;
    while !*stop.borrow() {
        if !crate::leader::is_leader(&ctx) {
            ctx.watch_current.store(false, Ordering::SeqCst);
            tokio::select! { _ = tokio::time::sleep(Duration::from_millis(200)) => {}, _ = stop.changed() => {} }
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
        tokio::select! { _ = tokio::time::sleep(backoff(failures, Duration::from_millis(500))) => {}, _ = stop.changed() => {} }
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
    let c = ctx.pool.get().await?;
    let sc = scope(ctx);
    let row = c
        .query_opt(
            "select zedtoken, coalesce(extract(epoch from (now() - watched_at)) * 1000, 'infinity')::float8
             from rebac.watch_state where scope = $1",
            &[&sc],
        )
        .await?;
    Ok(match row {
        Some(r) => {
            let (tok, age): (Option<String>, f64) = (r.get(0), r.get(1));
            match tok {
                Some(t) if !t.is_empty() && age <= ctx.config.watch_resume_max_age_ms as f64 => {
                    Some(t)
                }
                Some(_) => {
                    tracing::warn!(
                        "the Watch position is older than watch_resume_max_age_ms: restarting from head (sets looked up before fail closed until re-projected)"
                    );
                    None
                }
                None => None,
            }
        }
        None => None,
    })
}

/// EVERY type is watched (an empty filter). `rebac.apply_watch` skips relations outside the closure, and watching all of
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

/// One Watch message as `rebac.apply_watch`'s arrays. A TOUCH carrying a caveat or an expiry is applied as a removal
/// (fail closed, then refreshed): the closure refuses both, so only a schema the daemon has not re-checked yet can
/// produce one.
fn watch_rows(
    msg: &crate::authzed::api::v1::WatchResponse,
) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    use crate::authzed::api::v1::relationship_update::Operation;
    let (mut ops, mut resources, mut relations, mut subjects) = (vec![], vec![], vec![], vec![]);
    for u in &msg.updates {
        let Some(rel) = &u.relationship else { continue };
        let (Some(res), Some(sub)) = (&rel.resource, &rel.subject) else {
            continue;
        };
        let Some(sobj) = &sub.object else { continue };
        if res.object_id == WATCH_MARKER {
            continue;
        }
        let mut subject = format!("{}:{}", sobj.object_type, sobj.object_id);
        if !sub.optional_relation.is_empty() {
            subject = format!("{subject}#{}", sub.optional_relation);
        }
        let narrows = u.operation == Operation::Delete as i32
            || rel.optional_caveat.is_some()
            || rel.optional_expires_at.is_some();
        ops.push(if narrows { "delete" } else { "touch" }.to_string());
        resources.push(format!("{}:{}", res.object_type, res.object_id));
        relations.push(rel.relation.clone());
        subjects.push(subject);
    }
    (ops, resources, relations, subjects)
}

async fn watch_once(ctx: &Ctx, stop: &mut watch::Receiver<bool>) -> anyhow::Result<Resume> {
    let mut from = resume_position(ctx).await?;
    let types = types_watched();
    let apply = ctx.config.mode == Mode::Live;
    // NO POSITION TO RESUME: restart from SpiceDB's head revision, read directly, and record the gap before anything
    // is applied, so the stream below is an ordinary resumed one (an idle store may send a stream opened at "now"
    // nothing at all, not even the reply that completes the open).
    if from.is_none() && apply {
        let t = head_token(ctx).await?;
        crate::leader::fenced_execute(ctx, "select rebac.watch_restarted($1)", &[&t]).await?;
        tracing::warn!("Watch restarted from SpiceDB's head: sets looked up before now fail closed until the forced sweep re-projects them");
        from = Some(t);
    }
    let restart = from.is_none();
    // CAUGHT UP. A resumed stream first replays what happened while it was down, and nothing in a Watch message says
    // where that replay ends (SpiceDB sends a checkpoint after every revision it delivers, replayed or not). So the
    // daemon reads SpiceDB's head revision first (a fully consistent CheckPermission, read-only) and keeps the stream
    // "not current" until a message's position is at or past it (crate::revision): Watch delivers in revision order,
    // so by then every change committed before the stream opened has been delivered and (in order) applied. Until
    // then the heartbeat stays off and reads stay closed through the whole replay. `watch_marker = true` instead
    // touches a marker relationship once the stream is open and waits for its revision, for a datastore whose
    // revisions crate::revision cannot compare.
    //
    // AN IDLE STORE sends a resumed stream nothing at all, not even the reply that completes the open (SpiceDB's
    // in-memory datastore sends no idle checkpoint). Its head is then the stored position itself, so the comparison
    // below makes the stream current at once, before the open completes. Every type is watched (changes to relations
    // outside the closure are skipped by rebac.apply_watch), so any revision past the stored position carries a change
    // the stream delivers: a head past the position always means a replay to wait for, never a quiet store.
    let mut caught_up = false;
    let opened = std::time::Instant::now();
    let mut alive = tokio::time::interval(Duration::from_millis(ctx.config.heartbeat_ms.max(100)));
    let mut target = if apply && !ctx.config.watch_marker {
        Some(catch_up_target(ctx).await?)
    } else {
        None
    };
    // AN IDLE STORE whose position is already at or past the head: nothing to replay, current at once.
    if let (Some(t), Some(f)) = (&target, &from) {
        if t.reached(Some(f)) {
            caught_up = true;
            mark_caught_up(ctx, opened).await?;
            tracing::debug!("Watch caught up at open: nothing to replay");
        }
    }
    let open = ctx.spicedb.watch(types, from.clone());
    tokio::pin!(open);
    let opened_stream = loop {
        tokio::select! {
            s = &mut open => break s,
            _ = alive.tick(), if caught_up && apply => {
                if !crate::leader::is_leader(ctx) {
                    anyhow::bail!("no longer the leader: stopping the Watch stream");
                }
                crate::leader::fenced_execute(ctx, "select rebac.watch_alive()", &[]).await?;
            }
            _ = stop.changed() => return Ok(Resume::Same),
        }
    };
    let mut stream = match opened_stream {
        Ok(s) => s,
        Err(e) if !restart && position_refused(&e) => return drop_position(ctx, &e).await,
        Err(e) => return Err(e.into()),
    };
    let mut first = true;
    if apply && target.is_none() {
        target = Some(catch_up_target(ctx).await?);
    }
    let catch_up_by = tokio::time::Instant::now() + 6 * ctx.config.deadline();
    loop {
        let msg = tokio::select! {
            m = stream.message() => m,
            _ = alive.tick(), if caught_up && apply => {
                // Connected, and everything delivered is applied: the position is current as of now.
                if !crate::leader::is_leader(ctx) {
                    anyhow::bail!("no longer the leader: stopping the Watch stream");
                }
                crate::leader::fenced_execute(ctx, "select rebac.watch_alive()", &[]).await?;
                continue;
            }
            _ = tokio::time::sleep_until(catch_up_by), if apply && !caught_up => {
                anyhow::bail!("Watch did not reach the revision read when it opened within {:?}; reopening (reads stay closed)", 6 * ctx.config.deadline());
            }
            _ = stop.changed() => return Ok(Resume::Same),
        };
        let msg = match msg {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(Resume::Same),
            // A stale position may only be refused on the stream's first message.
            Err(e) if first && !restart && position_refused(&e) => {
                return drop_position(ctx, &e).await
            }
            Err(e) => return Err(e.into()),
        };
        let token = msg.changes_through.as_ref().map(|t| t.token.clone());
        if first && restart && apply {
            let Some(t) = &token else {
                anyhow::bail!("Watch's first message has no position");
            };
            crate::leader::fenced_execute(ctx, "select rebac.watch_restarted($1)", &[t]).await?;
            tracing::warn!("Watch restarted from SpiceDB's head: sets looked up before now fail closed until the forced sweep re-projects them");
        }
        let (ops, resources, relations, subjects) = watch_rows(&msg);
        let mut c = ctx.pool.get().await?;
        let tx = c.transaction().await?;
        crate::leader::assert_leader(ctx, &tx).await?;
        let row = tx
            .query_one(
                "select external, narrowed from rebac.apply_watch($1, $2, $3, $4, $5, $6)",
                &[&ops, &resources, &relations, &subjects, &token, &apply],
            )
            .await?;
        tx.commit().await?;
        let (external, narrowed): (i32, i32) = (row.get(0), row.get(1));
        first = false;
        if !caught_up && target.as_ref().is_some_and(|t| t.reached(token.as_deref())) {
            caught_up = true;
            mark_caught_up(ctx, opened).await?;
            tracing::debug!("Watch caught up: the stream is past the revision read when it opened");
        }
        if external > 0 {
            tracing::info!(
                external,
                narrowed,
                "applied relationship changes not made through rebac.write"
            );
        }
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

/// THE CLOSURE, KEPT. Re-reads the live schema every `closure_check_ms`, and at once when Watch reports a schema
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
            _ = tokio::time::sleep(every) => {},
            _ = ctx.closure_kick.notified() => {},
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
            let invalid: bool = ctx.config.mode == crate::config::Mode::Live
                && ctx
                    .pool
                    .get()
                    .await?
                    .query_opt("select state = 'invalid' from rebac.closure_state", &[])
                    .await?
                    .map(|r| r.get(0))
                    .unwrap_or(false);
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
        let (ops, res, rels, subs) = watch_rows(&msg);
        assert_eq!(ops, ["touch", "delete", "touch", "delete"]);
        assert_eq!(res[0], "container:acme/a");
        assert_eq!(rels[0], "member");
        assert_eq!(subs[0], "group:acme/eng#member");
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
        let (ops, res, _, _) = watch_rows(&msg);
        assert_eq!(ops, ["delete"]);
        assert_eq!(res, ["container:acme/a"]);
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
