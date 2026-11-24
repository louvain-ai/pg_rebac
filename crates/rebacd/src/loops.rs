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
    for row in resources {
        let (t, res, tok, gen): (String, String, Option<String>, i64) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        let subjects = ctx
            .spicedb
            .lookup_subjects(
                &res,
                &ctx.projection.permission,
                &ctx.projection.subject_type,
                tok.as_deref(),
                ctx.config.deadline(),
            )
            .await;
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

pub async fn sweep_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let check = Duration::from_millis((ctx.config.sweep_interval_ms as u64).min(60_000));
    let requeue = ctx.config.mode == Mode::Live;
    loop {
        tokio::select! { _ = tokio::time::sleep(check) => {}, _ = stop.changed() => {} }
        if *stop.borrow() {
            return;
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

/// THE HEARTBEAT the staleness bound reads. Beaten only while SpiceDB answers, Postgres takes the write and (with
/// Watch on) the Watch stream is connected and applying, so a daemon that is down, cut off from SpiceDB or behind on
/// Watch stops beating and reads fail closed once the bound passes.
pub async fn heartbeat_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let every = Duration::from_millis(ctx.config.heartbeat_ms);
    let mut was_ok = true;
    while !*stop.borrow() {
        // With Watch on (live), a beat also needs the stream connected and applying: a revocation made outside
        // rebac.write is then in force within the stream's delivery, or reads fail closed.
        let watch_ok = !(ctx.config.watch && ctx.config.mode == Mode::Live)
            || ctx.watch_current.load(Ordering::SeqCst);
        let ok = watch_ok
            && match ctx
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
                tracing::warn!("heartbeat stopped: SpiceDB, Postgres or the Watch stream unavailable; reads fail closed past the bound");
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

async fn types_watched(ctx: &Ctx) -> anyhow::Result<Vec<String>> {
    Ok(ctx
        .pool
        .get()
        .await?
        .query(
            "select distinct resource_type from rebac.view_relation",
            &[],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// Make sure a Watch position is stored, restarting from head (with a gap) when none is. The backfill calls this
/// before it projects anything, so a daemon started after it resumes from there and never needs the gap.
pub async fn ensure_watch_position(ctx: &Ctx) -> anyhow::Result<()> {
    if !ctx.config.watch || ctx.config.mode != Mode::Live || resume_position(ctx).await?.is_some() {
        return Ok(());
    }
    let mut stream = ctx.spicedb.watch(types_watched(ctx).await?, None).await?;
    let first = tokio::time::timeout(ctx.config.deadline(), stream.message())
        .await
        .map_err(|_| anyhow::anyhow!("Watch sent nothing within the deadline"))??
        .ok_or_else(|| anyhow::anyhow!("Watch ended before its first message"))?;
    let token = first
        .changes_through
        .map(|t| t.token)
        .ok_or_else(|| anyhow::anyhow!("Watch's first message has no position"))?;
    ctx.pool
        .get()
        .await?
        .execute("select rebac.watch_restarted($1)", &[&token])
        .await?;
    Ok(())
}

/// The object id of the marker relationship the Watch loop touches to learn where a replay ends. It carries no tenant
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
    let from = resume_position(ctx).await?;
    let restart = from.is_none();
    let types = types_watched(ctx).await?;
    let apply = ctx.config.mode == Mode::Live;
    // Opening the stream waits for SpiceDB's first message, so it races the stop signal too.
    let opened = tokio::select! {
        s = ctx.spicedb.watch(types, from.clone()) => s,
        _ = stop.changed() => return Ok(Resume::Same),
    };
    let mut stream = match opened {
        Ok(s) => s,
        Err(e) if !restart && position_refused(&e) => return drop_position(ctx, &e).await,
        Err(e) => return Err(e.into()),
    };
    let mut first = true;
    // CAUGHT UP. A resumed stream first replays what happened while it was down, and nothing in a Watch message says
    // where that replay ends (SpiceDB sends a checkpoint after every revision it delivers, replayed or not). So, once
    // the stream is open, the daemon TOUCHES a marker relationship and keeps the stream "not current" until Watch
    // delivers the marker's own revision: Watch delivers in revision order, so by then every change committed before
    // the stream opened has been delivered and (in order) applied. Until then the heartbeat stays off and reads stay
    // closed through the whole replay, not just its first message.
    let mut caught_up = false;
    let marker = if apply {
        Some(write_marker(ctx).await?)
    } else {
        None
    };
    let catch_up_by = tokio::time::Instant::now() + 6 * ctx.config.deadline();
    let mut alive = tokio::time::interval(Duration::from_millis(ctx.config.heartbeat_ms.max(100)));
    loop {
        let msg = tokio::select! {
            m = stream.message() => m,
            _ = alive.tick(), if caught_up && apply => {
                // Connected, and everything delivered is applied: the position is current as of now.
                ctx.pool.get().await?.execute("select rebac.watch_alive()", &[]).await?;
                continue;
            }
            _ = tokio::time::sleep_until(catch_up_by), if apply && !caught_up => {
                anyhow::bail!("Watch did not deliver its marker within {:?}; reopening (reads stay closed)", 6 * ctx.config.deadline());
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
            ctx.pool
                .get()
                .await?
                .execute("select rebac.watch_restarted($1)", &[t])
                .await?;
            tracing::warn!("Watch restarted from SpiceDB's head: sets looked up before now fail closed until the forced sweep re-projects them");
        }
        let (ops, resources, relations, subjects) = watch_rows(&msg);
        let mut c = ctx.pool.get().await?;
        let tx = c.transaction().await?;
        let row = tx
            .query_one(
                "select external, narrowed from rebac.apply_watch($1, $2, $3, $4, $5, $6)",
                &[&ops, &resources, &relations, &subjects, &token, &apply],
            )
            .await?;
        tx.commit().await?;
        let (external, narrowed): (i32, i32) = (row.get(0), row.get(1));
        first = false;
        if !caught_up && marker.is_some() && token.as_deref() == marker.as_deref() {
            caught_up = true;
            ctx.watch_current.store(true, Ordering::SeqCst);
            ctx.pool
                .get()
                .await?
                .execute("select rebac.watch_alive()", &[])
                .await?;
            tracing::debug!("Watch caught up: its marker was delivered");
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
    ctx.pool
        .get()
        .await?
        .execute(
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
