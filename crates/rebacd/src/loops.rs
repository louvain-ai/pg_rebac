//! The daemon's loops: relay, refresh, resource refresh, sweep, heartbeat and Watch (observe-only in slice 1).

use crate::{config::Mode, project, relay, Ctx};
use futures::{stream, StreamExt};
use std::{sync::Arc, time::Duration};
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

/// THE HEARTBEAT the staleness bound reads. Beaten only while SpiceDB answers and Postgres takes the write, so a
/// daemon that is down, or cut off from SpiceDB, stops beating and reads fail closed once the bound passes.
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
                tracing::warn!("heartbeat stopped: SpiceDB or Postgres unreachable; reads fail closed past the bound");
            }
            was_ok = ok;
        }
        tokio::select! { _ = tokio::time::sleep(every) => {}, _ = stop.changed() => {} }
    }
    tracing::debug!("heartbeat_loop stopped");
}

/// WATCH, OBSERVE-ONLY. Follows SpiceDB's changes to the projected types and counts the ones that did not come through
/// `rebac.write` (another service, the zed CLI): they are narrowed only by the sweep in slice 1, and `rebac.status()`
/// says how many there were. Its position is kept in `rebac.watch_state`.
pub async fn watch_loop(ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let mut failures = 0u32;
    while !*stop.borrow() {
        let r = watch_once(&ctx, &mut stop).await;
        if *stop.borrow() {
            return;
        }
        failures += 1;
        if let Err(e) = r {
            if failures == 1 {
                tracing::warn!(error = %crate::logs::kind_of(&e), "watch stream ended; reconnecting");
            }
        }
        tokio::select! { _ = tokio::time::sleep(backoff(failures, Duration::from_millis(500))) => {}, _ = stop.changed() => {} }
    }
    tracing::debug!("watch_loop stopped");
}

async fn watch_once(ctx: &Ctx, stop: &mut watch::Receiver<bool>) -> anyhow::Result<()> {
    let c = ctx.pool.get().await?;
    let from: Option<String> = c
        .query_opt(
            "select zedtoken from rebac.watch_state where scope = 'watch'",
            &[],
        )
        .await?
        .and_then(|r| r.get(0));
    let types: Vec<String> = c
        .query(
            "select distinct resource_type from rebac.view_relation",
            &[],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    // Opening the stream waits for SpiceDB's first message, so it races the stop signal too.
    let mut stream = tokio::select! {
        s = ctx.spicedb.watch(types, from) => s?,
        _ = stop.changed() => return Ok(()),
    };
    loop {
        let msg = tokio::select! {
            m = stream.message() => m?,
            _ = stop.changed() => return Ok(()),
        };
        let Some(msg) = msg else { return Ok(()) };
        let mut external: i64 = 0;
        for u in &msg.updates {
            let Some(rel) = &u.relationship else { continue };
            let (Some(res), Some(sub)) = (&rel.resource, &rel.subject) else {
                continue;
            };
            let Some(sobj) = &sub.object else { continue };
            let resource = format!("{}:{}", res.object_type, res.object_id);
            let mut subject = format!("{}:{}", sobj.object_type, sobj.object_id);
            if !sub.optional_relation.is_empty() {
                subject = format!("{subject}#{}", sub.optional_relation);
            }
            let op = if u.operation
                == crate::authzed::api::v1::relationship_update::Operation::Delete as i32
            {
                "delete"
            } else {
                "touch"
            };
            let ours: bool = c
                .query_one(
                    "select exists (select 1 from rebac.outbox where resource = $1 and relation = $2 and subject = $3
                       and op = $4 and (synced_at is null or synced_at > now() - interval '1 hour'))",
                    &[&resource, &rel.relation, &subject, &op],
                )
                .await?
                .get(0);
            if !ours {
                external += 1;
            }
        }
        let schema_changed = msg.schema_updated;
        let token = msg.changes_through.map(|t| t.token);
        c.execute(
            "insert into rebac.watch_state (scope, zedtoken, external_writes, watched_at) values ('watch', $1, $2, now())
             on conflict (scope) do update set zedtoken = coalesce(excluded.zedtoken, rebac.watch_state.zedtoken),
               external_writes = rebac.watch_state.external_writes + excluded.external_writes, watched_at = now()",
            &[&token, &external],
        )
        .await?;
        if external > 0 {
            tracing::warn!(
                external,
                "relationship changes not made through rebac.write (narrowed by the sweep only)"
            );
        }
        if schema_changed {
            // The closure loop re-derives it; this stream ends so it reopens on the types the new closure names.
            tracing::warn!("the SpiceDB schema changed: re-checking the closure");
            ctx.closure_kick.notify_one();
            return Ok(());
        }
    }
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
