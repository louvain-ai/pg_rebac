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

/// What became of a delivery.
#[derive(Debug, Default)]
pub struct Delivered {
    /// The token SpiceDB wrote the last accepted write at.
    pub token: Option<String>,
    /// Rows SpiceDB refuses on their own, with the reason.
    pub quarantined: Vec<(i64, String)>,
    /// Rows naming a relation SpiceDB lacks, young enough to try again (`Refusal::Defer`).
    pub deferred: Vec<i64>,
}

impl Delivered {
    fn absorb(&mut self, later: Delivered) {
        self.token = later.token.or(self.token.take());
        self.quarantined.extend(later.quarantined);
        self.deferred.extend(later.deferred);
    }
}

/// Deliver rows; when SpiceDB refuses the batch as invalid, re-send ONE ROW AT A TIME and quarantine only the rows it
/// refuses on their own, so one poison tuple never holds every later permission change. Any other failure is an
/// outage: nothing is marked, and the next pass sends everything again (touch and delete are idempotent).
pub async fn deliver_isolating(ctx: &Ctx, rows: &[Row]) -> Result<Delivered, Status> {
    let mut out = Delivered::default();
    if rows.is_empty() {
        return Ok(out);
    }
    match ctx
        .spicedb
        .write(rows.iter().map(|r| &r.op), ctx.config.deadline())
        .await
    {
        Ok(token) => {
            out.token = token;
            return Ok(out);
        }
        Err(e) if is_invalid(&e) => {}
        Err(e) => return Err(e),
    }
    for r in rows {
        match ctx.spicedb.write([&r.op], ctx.config.deadline()).await {
            Ok(token) => out.token = token.or(out.token.take()),
            Err(e) => match refusal(&e) {
                Refusal::Outage => return Err(e),
                Refusal::Defer if r.age_ms <= DEFER_SETTLE_MS => out.deferred.push(r.seq),
                Refusal::Defer | Refusal::Quarantine => out.quarantined.push((
                    r.seq,
                    format!("{:?}: {}", e.code(), e.message())
                        .chars()
                        .take(500)
                        .collect(),
                )),
            },
        }
    }
    Ok(out)
}

/// One WriteRelationships refuses a relationship named twice. A claim never holds two, but if it ever did, it is split
/// wherever one repeats, in order.
fn without_duplicates(mut rows: Vec<Row>) -> Vec<Vec<Row>> {
    let mut starts = vec![];
    let mut seen = HashSet::new();
    for (i, r) in rows.iter().enumerate() {
        let key = (&r.op.resource, &r.op.relation, &r.op.subject);
        if !seen.insert(key) {
            starts.push(i);
            seen.clear();
            seen.insert(key);
        }
    }
    let mut chunks = Vec::with_capacity(starts.len() + 1);
    for at in starts.into_iter().rev() {
        chunks.push(rows.split_off(at));
    }
    chunks.push(rows);
    chunks.reverse();
    chunks
}

/// An outage during a pass, with the `Status` kept in the chain so its log line names `grpc:<code>`.
fn outage(e: Status) -> anyhow::Error {
    anyhow::Error::new(e).context("spicedb unavailable during relay")
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
            "select seq, op, resource, relation, subject, age_ms from rebac.claim_relay($1, $2, $3)",
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
        .query_one("select rebac.narrow_seqs($1, false)", &[&deletes])
        .await?
        .get(0);

    let claimed = rows.len();
    let seqs: Vec<i64> = rows.iter().map(|r| r.seq).collect();
    let mut out = Delivered::default();
    for chunk in without_duplicates(rows) {
        match deliver_isolating(ctx, &chunk).await {
            Ok(d) => out.absorb(d),
            Err(e) => {
                tx.commit().await?;
                return Err(outage(e));
            }
        }
    }
    let Delivered {
        token,
        quarantined,
        deferred,
    } = out;
    let (rs, reasons): (Vec<i64>, Vec<String>) = quarantined.into_iter().unzip();
    let settled: HashSet<i64> = rs.iter().chain(&deferred).copied().collect();
    let delivered: Vec<i64> = seqs.into_iter().filter(|s| !settled.contains(s)).collect();
    tx.query_one(
        "select rebac.record_delivery($1, $2, $3, $4)",
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
    tracing::debug!(claimed, narrowed, "relay pass");
    // A deferred row is not counted: a pass that settled nothing idles rather than spinning on it.
    Ok(claimed - deferred.len())
}

#[cfg(test)]
mod tests {
    use super::{outage, without_duplicates, Row};
    use crate::spicedb::TupleOp;

    fn row(seq: i64, touch: bool, subject: &str) -> Row {
        Row {
            seq,
            op: TupleOp {
                touch,
                resource: "container:acme/a".into(),
                relation: "member".into(),
                subject: subject.into(),
            },
            age_ms: 0,
        }
    }

    #[test]
    fn a_repeated_relationship_starts_a_new_chunk_in_order() {
        let rows = vec![
            row(1, true, "user:acme/a"),
            row(2, true, "user:acme/b"),
            row(3, false, "user:acme/a"),
            row(4, true, "user:acme/c"),
            row(5, true, "user:acme/c"),
        ];
        let seqs: Vec<Vec<i64>> = without_duplicates(rows)
            .iter()
            .map(|c| c.iter().map(|r| r.seq).collect())
            .collect();
        assert_eq!(seqs, [vec![1, 2], vec![3, 4], vec![5]]);
        assert_eq!(
            without_duplicates(vec![row(1, true, "user:acme/a")]).len(),
            1
        );
    }

    #[test]
    fn an_outage_logs_its_grpc_code() {
        let e = outage(tonic::Status::unavailable("connection refused"));
        assert_eq!(crate::logs::kind_of(&e), "grpc:Unavailable");
    }
}
