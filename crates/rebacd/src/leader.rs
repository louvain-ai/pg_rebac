//! ONE LEADER among the daemons of a deployment. Several `rebacd run` processes may share a database (for
//! availability, or during a rolling deploy): the relay, the refresh workers and `POST /lookup` are safe to run in
//! all of them (rows are claimed with `skip locked`, refreshes under leases), but the Watch stream, the closure
//! rebuild and the sweep are not. Two Watch appliers would move one position back and forth and could each record a
//! gap; two closure rebuilds would race. So those run only in the daemon that holds a session-level advisory lock
//! (`pg_try_advisory_lock(1919050596, 1)`; a shadow daemon's is `(1919050596, 2)`) on a connection of its own.
//!
//! A lock is not a fence on its own: a daemon whose lock connection has just died still believes it leads until its
//! next check. So every Watch write runs in a transaction that first calls `rebac.assert_leader(pid)` with the lock
//! connection's backend pid: it raises unless that backend still holds the lock (`pg_locks`), and it locks the
//! `watch_applied` row, so a deposed leader's last write either commits before the new leader's first or is refused.
//!
//! The lock connection must be a real session: through a pooler in transaction mode, a session-level advisory lock is
//! held by whichever server connection ran it and is released, or kept, at the pooler's whim. Point `DATABASE_URL`
//! at Postgres itself, or at a pooler in session mode.

use crate::Ctx;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;

/// The advisory lock's first key. 1919050595 is the tenant locks' space; this one is its own. The second key is the
/// mode's: 1 for live, 2 for shadow, so a shadow daemon never holds the live leader's lock.
pub const LOCK_SPACE: i32 = 1_919_050_596;

#[must_use]
pub fn lock_key(ctx: &Ctx) -> i32 {
    match ctx.config.mode {
        crate::config::Mode::Live => 1,
        crate::config::Mode::Shadow => 2,
    }
}

/// Hold the leader lock while this daemon runs: try every `leader_retry_ms`, check the held connection every
/// `heartbeat_ms`, and give leadership up (pid 0) the moment the connection fails.
pub async fn leader_loop(ctx: std::sync::Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let check = Duration::from_millis(ctx.config.heartbeat_ms.max(100));
    let retry = Duration::from_millis(ctx.config.leader_retry_ms.max(100));
    let mut held: Option<tokio_postgres::Client> = None;
    while !*stop.borrow() {
        match &held {
            None => match try_acquire(&ctx).await {
                Ok(Some((client, pid))) => {
                    ctx.leader_pid.store(pid, Ordering::SeqCst);
                    tracing::info!(
                        pid,
                        "leader: this daemon runs Watch, the closure check and the sweep"
                    );
                    held = Some(client);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %crate::logs::kind_of(&e), "leader: could not try the lock");
                }
            },
            Some(c) => {
                let ok = tokio::time::timeout(check * 3, c.simple_query("select 1"))
                    .await
                    .map(|r| r.is_ok())
                    .unwrap_or(false);
                if !ok {
                    ctx.leader_pid.store(0, Ordering::SeqCst);
                    ctx.watch_current.store(false, Ordering::SeqCst);
                    tracing::warn!(
                        "leader: lost the lock connection; another daemon may take over"
                    );
                    held = None;
                }
            }
        }
        let wait = if held.is_some() { check } else { retry };
        tokio::select! { () = tokio::time::sleep(wait) => {}, _ = stop.changed() => {} }
    }
    // Stopping: the lock goes with the connection, so the next daemon takes over at once.
    ctx.leader_pid.store(0, Ordering::SeqCst);
    drop(held);
}

async fn try_acquire(ctx: &Ctx) -> anyhow::Result<Option<(tokio_postgres::Client, i32)>> {
    let (pg, tls) = crate::tls::configure(&ctx.config.database_url()?)?;
    let (client, conn) = pg.connect(tls).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    // A dead host must not hold the lock for hours: the server gives up on the session after ~25 s of silence.
    client
        .batch_execute(
            "set tcp_keepalives_idle = 10; set tcp_keepalives_interval = 5; set tcp_keepalives_count = 3",
        )
        .await?;
    let row = client
        .query_one(
            "select pg_try_advisory_lock($1, $2), pg_backend_pid()",
            &[&LOCK_SPACE, &lock_key(ctx)],
        )
        .await?;
    if row.get::<_, bool>(0) {
        Ok(Some((client, row.get(1))))
    } else {
        Ok(None)
    }
}

/// True when this daemon may run the leader's work: it holds the lock, or leader election is off.
#[must_use]
pub fn is_leader(ctx: &Ctx) -> bool {
    !ctx.config.leader_election || ctx.leader_pid.load(Ordering::SeqCst) != 0
}

/// The pid `rebac.assert_leader` checks: the lock connection's, or 0 (no check) with leader election off.
#[must_use]
pub fn fence_pid(ctx: &Ctx) -> i32 {
    fence_pid_for(
        ctx.config.leader_election,
        ctx.leader_pid.load(Ordering::SeqCst),
    )
}

/// 0 asks `rebac.assert_leader` for no check, so it is sent only with leader election off. A daemon that does not
/// lead (`leader_pid` 0) sends -1, which no backend holds, so its fenced write is refused.
fn fence_pid_for(leader_election: bool, leader_pid: i32) -> i32 {
    match (leader_election, leader_pid) {
        (false, _) => 0,
        (true, 0) => -1,
        (true, pid) => pid,
    }
}

/// The fence, as the first statement of a transaction that writes Watch state.
pub async fn assert_leader(
    ctx: &Ctx,
    tx: &deadpool_postgres::Transaction<'_>,
) -> anyhow::Result<()> {
    tx.execute(
        "select rebac.assert_leader($1, $2)",
        &[&fence_pid(ctx), &lock_key(ctx)],
    )
    .await?;
    Ok(())
}

/// Run one statement in a transaction fenced by `rebac.assert_leader`.
pub async fn fenced_execute(
    ctx: &Ctx,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> anyhow::Result<u64> {
    let mut c = ctx.pool.get().await?;
    let tx = c.transaction().await?;
    assert_leader(ctx, &tx).await?;
    let n = tx.execute(sql, params).await?;
    tx.commit().await?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::fence_pid_for;

    #[test]
    fn a_daemon_that_does_not_lead_is_fenced_out() {
        assert_eq!(fence_pid_for(true, 0), -1, "0 would skip the check");
        assert_eq!(fence_pid_for(true, 4242), 4242);
        assert_eq!(fence_pid_for(false, 0), 0);
        assert_eq!(fence_pid_for(false, 4242), 0);
    }
}
