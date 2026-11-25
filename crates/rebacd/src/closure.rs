//! The closure check: derive `rebac.view_relation` from the live SpiceDB schema and compare it with the table.

use crate::{config::Mode, Ctx};
use rebac_schema::{closure, parse, ViewRelation};

pub async fn derive(ctx: &Ctx) -> anyhow::Result<Vec<ViewRelation>> {
    match derive_checked(ctx).await? {
        Derived::Supported(d) => Ok(d),
        Derived::Unsupported(why) => anyhow::bail!("{why}"),
    }
}

/// What the live schema's closure is: a list the daemon can narrow soundly, or a refusal (its reason). An `Err` is
/// SpiceDB not answering, which says nothing about the schema.
pub enum Derived {
    Supported(Vec<ViewRelation>),
    Unsupported(String),
}

pub async fn derive_checked(ctx: &Ctx) -> anyhow::Result<Derived> {
    let text = ctx.spicedb.read_schema(ctx.config.deadline()).await?;
    Ok(derive_text(ctx, &text))
}

pub fn derive_text(ctx: &Ctx, text: &str) -> Derived {
    let schema = match parse(text) {
        Ok(s) => s,
        Err(e) => return Derived::Unsupported(format!("the schema does not parse: {e}")),
    };
    match closure(
        &schema,
        &ctx.projection.resource_type,
        &ctx.projection.permission,
    ) {
        Ok(mut d) => {
            d.sort();
            Derived::Supported(d)
        }
        Err(e) => Derived::Unsupported(e.to_string()),
    }
}

pub async fn stored(
    db: &impl deadpool_postgres::GenericClient,
) -> anyhow::Result<Vec<ViewRelation>> {
    let rows = db
        .query(
            "select resource_type, relation, kind, target_type, through from rebac.view_relation",
            &[],
        )
        .await?;
    let mut out: Vec<ViewRelation> = rows
        .iter()
        .map(|r| ViewRelation {
            resource_type: r.get(0),
            relation: r.get(1),
            kind: match r.get::<_, &str>(2) {
                "direct" => rebac_schema::Kind::Direct,
                "arrow" => rebac_schema::Kind::Arrow,
                "set" => rebac_schema::Kind::Set,
                _ => rebac_schema::Kind::Via,
            },
            target_type: r.get(3),
            through: r.get(4),
        })
        .collect();
    out.sort();
    Ok(out)
}

pub async fn write(
    db: &mut deadpool_postgres::Object,
    rels: &[ViewRelation],
) -> anyhow::Result<()> {
    let tx = db.transaction().await?;
    tx.execute("delete from rebac.view_relation", &[]).await?;
    for r in rels {
        tx.execute(
            "insert into rebac.view_relation (resource_type, relation, kind, target_type, through) values ($1, $2, $3, $4, $5)",
            &[&r.resource_type, &r.relation, &r.kind.as_str(), &r.target_type, &r.through],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn closure_invalid(db: &impl deadpool_postgres::GenericClient) -> anyhow::Result<bool> {
    Ok(db
        .query_opt("select state = 'invalid' from rebac.closure_state", &[])
        .await?
        .map(|r| r.get::<_, bool>(0))
        .unwrap_or(false))
}

async fn invalidate(ctx: &Ctx, why: &str) -> anyhow::Result<()> {
    if ctx.config.mode == Mode::Shadow {
        // A shadow daemon does not own the live projection, so it never gates its reads.
        tracing::error!("the closure no longer matches the schema (shadow mode: live reads are not gated by this daemon)");
        return Ok(());
    }
    let why: String = why.chars().take(200).collect();
    ctx.pool
        .get()
        .await?
        .execute("select rebac.invalidate_closure($1)", &[&why])
        .await?;
    Ok(())
}

/// One rebuild at a time across every replica: a session advisory lock on a connection held for the rebuild.
const REBUILD_LOCK: i64 = 0x7262_6163_636c_6f73; // "rbacclos"

/// THE REBUILD after the closure changed. In one transaction the new list is stored and the closure marked invalid,
/// so every read fails closed (RB503) from the moment the stored list stops matching what it was built from. Then,
/// per tenant, the mirrors are pruned of relations that no longer grant view and re-read from SpiceDB, and every
/// principal is re-projected (`backfill`). Once every tenant's relationships have been re-read the closure is valid
/// again. A principal whose re-projection failed stays closed on its own (`rebac.checked`: looked up before the
/// change) until the sweep, which this forces, re-projects it. If a tenant's relationships cannot be read, the
/// closure stays invalid and the caller retries.
///
/// In SHADOW mode the list is stored and the shadow rebuilt, but the live closure state is never touched and the
/// mirrors are not pruned: the live projection is whoever maintains it.
pub async fn rebuild(ctx: &Ctx, rels: &[ViewRelation], why: &str) -> anyhow::Result<()> {
    let lock = ctx.pool.get().await?;
    lock.execute("select pg_advisory_lock($1)", &[&REBUILD_LOCK])
        .await?;
    let r = rebuild_locked(ctx, rels, why).await;
    let _ = lock
        .execute("select pg_advisory_unlock($1)", &[&REBUILD_LOCK])
        .await;
    r
}

async fn rebuild_locked(ctx: &Ctx, rels: &[ViewRelation], why: &str) -> anyhow::Result<()> {
    let live = ctx.config.mode == Mode::Live;
    {
        let mut db = ctx.pool.get().await?;
        // Another replica may have finished this very rebuild while this one waited for the lock.
        if stored(&db).await? == rels && !(live && closure_invalid(&db).await?) {
            return Ok(());
        }
        let tx = db.transaction().await?;
        tx.execute("delete from rebac.view_relation", &[]).await?;
        for r in rels {
            tx.execute(
                "insert into rebac.view_relation (resource_type, relation, kind, target_type, through) values ($1, $2, $3, $4, $5)",
                &[&r.resource_type, &r.relation, &r.kind.as_str(), &r.target_type, &r.through],
            )
            .await?;
        }
        if live {
            let why: String = why.chars().take(200).collect();
            tx.execute("select rebac.invalidate_closure($1)", &[&why])
                .await?;
            // Mirrored tuples of relations that no longer grant view narrow nothing now; drop them.
            tx.batch_execute(
                "delete from rebac.edge e where not exists (select 1 from rebac.view_relation v
                   where v.kind = 'arrow' and v.relation = e.relation and v.resource_type = rebac.setting('resource_type'));
                 delete from rebac.set_grant g where not exists (select 1 from rebac.view_relation v
                   where v.relation = g.relation
                     and ((v.kind = 'direct' and v.resource_type = rebac.setting('resource_type') and strpos(g.container, ':') = 0)
                          or (v.kind = 'via' and v.resource_type = split_part(g.container, ':', 1))));",
            )
            .await?;
        }
        tx.commit().await?;
    }
    tracing::warn!(
        relations = rels.len(),
        "the closure changed: rebuilding every tenant (live reads fail closed meanwhile)"
    );
    let tenants: Vec<String> = ctx
        .pool
        .get()
        .await?
        .query(
            "select tenant from rebac.principal_state union select tenant from rebac.subject
             union select tenant from rebac.edge union select tenant from rebac.set_grant order by 1",
            &[],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let (mut unread, mut unprojected) = (0usize, 0usize);
    for t in &tenants {
        match crate::backfill::backfill(ctx, t).await {
            Ok(b) => unprojected += b.failed,
            Err(e) => {
                tracing::error!(error = %crate::logs::kind_of(&e), "a tenant's relationships could not be re-read");
                unread += 1;
            }
        }
    }
    if unread > 0 {
        anyhow::bail!("{unread} tenants could not be re-read; the closure stays invalid");
    }
    let db = ctx.pool.get().await?;
    if live {
        db.execute("select rebac.accept_closure()", &[]).await?;
    }
    // The sweep re-projects everyone again, the principals that failed above among them.
    db.execute(
        "update rebac.sweep_claim set last_at = now() - interval '1 year'",
        &[],
    )
    .await?;
    tracing::info!(
        tenants = tenants.len(),
        unprojected,
        "the closure is rebuilt; principals not yet re-projected stay closed until the sweep reaches them"
    );
    Ok(())
}

/// What a re-check of the live schema found.
#[derive(Debug, PartialEq, Eq)]
pub enum Recheck {
    Unchanged,
    Rebuilt,
    Unsupported,
}

/// Re-derive the closure from the live schema (on a schema change, and periodically). Unsupported: reads fail closed
/// until it is supported again. Different from the stored list, or invalid from before: rebuild.
pub async fn recheck(ctx: &Ctx) -> anyhow::Result<Recheck> {
    match derive_checked(ctx).await? {
        Derived::Unsupported(why) => {
            tracing::error!("the SpiceDB schema's closure is unsupported: reads fail closed");
            invalidate(ctx, &format!("unsupported: {why}")).await?;
            Ok(Recheck::Unsupported)
        }
        Derived::Supported(derived) => {
            let db = ctx.pool.get().await?;
            let have = stored(&db).await?;
            let invalid = ctx.config.mode == Mode::Live && closure_invalid(&db).await?;
            drop(db);
            if have == derived && !invalid {
                return Ok(Recheck::Unchanged);
            }
            rebuild(ctx, &derived, "schema_changed").await?;
            Ok(Recheck::Rebuilt)
        }
    }
}

/// At start: an empty table is filled; a table that differs from the schema refuses the start (and the closure is
/// marked invalid, so reads fail closed), because a relation missing from it is a grant that never refreshes or a
/// revocation that never narrows. `accept` rebuilds instead; so does a closure left invalid by an earlier run.
pub async fn check_at_start(ctx: &Ctx, accept: bool) -> anyhow::Result<()> {
    let derived = match derive_checked(ctx).await? {
        Derived::Supported(d) => d,
        Derived::Unsupported(why) => {
            invalidate(ctx, &format!("unsupported: {why}")).await?;
            anyhow::bail!("the SpiceDB schema cannot be projected soundly: {why}");
        }
    };
    let mut db = ctx.pool.get().await?;
    let have = stored(&db).await?;
    let invalid = ctx.config.mode == Mode::Live && closure_invalid(&db).await?;
    if have.is_empty() && !invalid {
        write(&mut db, &derived).await?;
        return Ok(());
    }
    if have != derived && !accept {
        drop(db);
        invalidate(ctx, "schema_changed_unaccepted").await?;
        anyhow::bail!(
            "rebac.view_relation does not match the SpiceDB schema ({} rows stored, {} derived); \
             run `rebacd closure --write` or start with --accept-schema",
            have.len(),
            derived.len()
        );
    }
    if have != derived || invalid {
        drop(db);
        rebuild(ctx, &derived, "schema_changed").await?;
    }
    Ok(())
}
