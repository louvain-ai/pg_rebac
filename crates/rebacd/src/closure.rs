//! The closure check: derive `rebac.view_relation` from the live SpiceDB schema and compare it with the table.

use crate::Ctx;
use rebac_schema::{closure, parse, ViewRelation};

pub async fn derive(ctx: &Ctx) -> anyhow::Result<Vec<ViewRelation>> {
    let text = ctx.spicedb.read_schema(ctx.config.deadline()).await?;
    let schema = parse(&text)?;
    Ok(closure(
        &schema,
        &ctx.projection.resource_type,
        &ctx.projection.permission,
    )?)
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

/// At start: an empty table is filled; a table that differs from the schema refuses the start, because a relation
/// missing from it is a grant that never refreshes or a revocation that never narrows.
pub async fn check_at_start(ctx: &Ctx, accept: bool) -> anyhow::Result<()> {
    let derived = derive(ctx).await?;
    let mut db = ctx.pool.get().await?;
    let have = stored(&db).await?;
    if have.is_empty() || (accept && have != derived) {
        write(&mut db, &derived).await?;
        if !have.is_empty() {
            db.execute(
                "update rebac.sweep_claim set last_at = now() - interval '1 year'",
                &[],
            )
            .await?;
            tracing::warn!("rebac.view_relation replaced from the schema; every principal will be re-projected");
        }
        return Ok(());
    }
    if have != derived {
        anyhow::bail!(
            "rebac.view_relation does not match the SpiceDB schema ({} rows stored, {} derived); \
             run `rebacd closure --write` or start with --accept-schema",
            have.len(),
            derived.len()
        );
    }
    Ok(())
}
