//! The relay: Postgres outbox -> SpiceDB, in order, revocations first, no tenant waiting behind another.

use crate::{
    spicedb::{is_invalid, refusal, Refusal, TupleOp},
    Ctx,
};
use std::collections::HashSet;
use tonic::Status;

#[derive(Debug)]
pub struct Row {
    pub seq: i64,
    pub op: TupleOp,
    /// How long the row has been in the outbox.
    pub age_ms: i64,
}

/// How long a row naming a relation SpiceDB's schema lacks is retried (`Refusal::Defer`) before it is quarantined: no
/// deploy takes this long to write its schema, and a row still refused after it would cost every pass for ever.
pub const DEFER_SETTLE_MS: i64 = 10 * 60_000;

/// What became of a delivery: SpiceDB's token, the rows quarantined (with reasons), the rows deferred.
pub type Delivered = (Option<String>, Vec<(i64, String)>, Vec<i64>);

/// Deliver rows; when SpiceDB refuses the batch as invalid, re-send ONE ROW AT A TIME and quarantine only the rows it
/// refuses on their own, so one poison tuple never holds every later permission change. Any other failure is an
/// outage: nothing is marked, and the next pass sends everything again (touch and delete are idempotent).
pub async fn deliver_isolating(ctx: &Ctx, rows: &[Row]) -> Result<Delivered, Status> {
    if rows.is_empty() {
        return Ok((None, vec![], vec![]));
    }
    let ops: Vec<TupleOp> = rows.iter().map(|r| r.op.clone()).collect();
    match ctx.spicedb.write(&ops, ctx.config.deadline()).await {
        Ok(token) => return Ok((token, vec![], vec![])),
        Err(e) if is_invalid(&e) => {}
        Err(e) => return Err(e),
    }
    let mut token = None;
    let mut refused = vec![];
    let mut deferred = vec![];
    for r in rows {
        match ctx
            .spicedb
            .write(std::slice::from_ref(&r.op), ctx.config.deadline())
            .await
        {
            Ok(t) => token = t.or(token),
            Err(e) => match refusal(&e) {
                Refusal::Outage => return Err(e),
                Refusal::Defer if r.age_ms <= DEFER_SETTLE_MS => deferred.push(r.seq),
                Refusal::Defer | Refusal::Quarantine => refused.push((
                    r.seq,
                    format!("{:?}: {}", e.code(), e.message())
                        .chars()
                        .take(500)
                        .collect(),
                )),
            },
        }
    }
    Ok((token, refused, deferred))
}

/// One pass. Returns how many rows it claimed; an outage is returned AFTER the transaction commits, so the narrowing
/// done at claim stands and the row locks are released.
pub async fn relay_once(ctx: &Ctx) -> anyhow::Result<usize> {
    let c = &ctx.config;
    let mut client = ctx.pool.get().await?;
    let tx = client.transaction().await?;
    tx.batch_execute(&format!(
        "set local lock_timeout = '{}ms'; set local statement_timeout = '{}ms'",
        c.relay_lock_timeout_ms, c.relay_statement_timeout_ms
    ))
    .await?;
    let claimed = tx
        .query(
            "select seq, op, resource, relation, subject, age_ms from fence.claim_relay($1, $2, $3)",
            &[&c.relay_batch, &c.relay_per_tenant, &c.relay_window],
        )
        .await?;
    if claimed.is_empty() {
        tx.commit().await?;
        return Ok(0);
    }
    let rows: Vec<Row> = claimed
        .iter()
        .map(|r| Row {
            seq: r.get(0),
            op: TupleOp {
                touch: r.get::<_, String>(1) == "touch",
                resource: r.get(2),
                relation: r.get(3),
                subject: r.get(4),
            },
            age_ms: r.get(5),
        })
        .collect();
    // Revocations narrow again at claim: a no-op when their transaction narrowed, the safety net when it did not.
    let deletes: Vec<i64> = rows.iter().filter(|r| !r.op.touch).map(|r| r.seq).collect();
    let narrowed: i32 = tx
        .query_one("select fence.narrow_seqs($1)", &[&deletes])
        .await?
        .get(0);

    // One WriteRelationships rejects duplicate relationships; the claim never holds two, but split if it ever did.
    let mut token = None;
    let mut refused = vec![];
    let mut deferred: Vec<i64> = vec![];
    let mut chunk: Vec<&Row> = vec![];
    let mut seen = HashSet::new();
    let mut failure = None;
    let mut chunks: Vec<Vec<&Row>> = vec![];
    for r in &rows {
        let key = (&r.op.resource, &r.op.relation, &r.op.subject);
        if !seen.insert(key) {
            chunks.push(std::mem::take(&mut chunk));
            seen.clear();
            seen.insert(key);
        }
        chunk.push(r);
    }
    chunks.push(chunk);
    for ch in chunks {
        let owned: Vec<Row> = ch
            .iter()
            .map(|r| Row {
                seq: r.seq,
                op: r.op.clone(),
                age_ms: r.age_ms,
            })
            .collect();
        match deliver_isolating(ctx, &owned).await {
            Ok((t, q, d)) => {
                token = t.or(token);
                refused.extend(q);
                deferred.extend(d);
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    if let Some(e) = failure {
        tx.commit().await?;
        anyhow::bail!(
            "spicedb unavailable during relay: {:?} {}",
            e.code(),
            e.message()
        );
    }
    let refused_seqs: HashSet<i64> = refused.iter().map(|(s, _)| *s).collect();
    let delivered: Vec<i64> = rows
        .iter()
        .map(|r| r.seq)
        .filter(|s| !refused_seqs.contains(s) && !deferred.contains(s))
        .collect();
    let (rs, reasons): (Vec<i64>, Vec<String>) = refused.into_iter().unzip();
    tx.query_one(
        "select fence.record_delivery($1, $2, $3, $4)",
        &[&delivered, &token, &rs, &reasons],
    )
    .await?;
    tx.commit().await?;
    if !rs.is_empty() {
        tracing::error!(
            quarantined = rs.len(),
            "quarantined tuples SpiceDB refuses as invalid, so delivery can continue"
        );
    }
    if !deferred.is_empty() {
        tracing::warn!(
            deferred = deferred.len(),
            "rows naming a relation SpiceDB does not define yet, left undelivered and retried"
        );
    }
    tracing::debug!(claimed = rows.len(), narrowed, "relay pass");
    // A deferred row is not counted: a pass that settled nothing idles rather than spinning on it.
    Ok(rows.len() - deferred.len())
}
