//! Backfill a tenant: page through its relationships, mirror what narrowing needs (arrow edges, grants to subject
//! sets and wildcards), then project every grant node and every principal they name.

use crate::authzed::api::v1::Relationship;
use crate::{config::Mode, db, project, Ctx};
use futures::{stream, StreamExt};
use serde::Serialize;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Default, Serialize)]
pub struct Backfilled {
    pub relationships: usize,
    pub edges: usize,
    pub principals: usize,
    pub nodes: usize,
    pub projected: usize,
    pub failed: usize,
}

/// The closure's relations, as backfill reads them.
struct Closure {
    types: Vec<String>,
    arrows: BTreeSet<String>,
    vias: BTreeSet<(String, String)>,
}

/// What one relationship contributes.
#[derive(Debug, PartialEq, Eq)]
enum Mirrored {
    /// An arrow tuple of the projected type: `rebac.edge (child, relation, parent)`.
    Edge(String, String, String),
    /// A grant to a subject set or a wildcard: `rebac.set_grant (container, relation, subject_set)`. A grant through a
    /// via relation has its object typed (`workspace:w1`).
    SetGrant(String, String, String),
    /// A subject of the projected subject type, to project.
    Subject(String),
    Nothing,
}

fn mirrored(ctx: &Ctx, closure: &Closure, ty: &str, prefix: &str, rel: &Relationship) -> Mirrored {
    let (Some(res), Some(sub)) = (&rel.resource, &rel.subject) else {
        return Mirrored::Nothing;
    };
    let Some(sobj) = &sub.object else {
        return Mirrored::Nothing;
    };
    let projected = ty == ctx.projection.resource_type;
    let Some(id) = db::strip(prefix, &res.object_id) else {
        return Mirrored::Nothing;
    };
    // A wildcard subject carries no tenant prefix.
    if sobj.object_id == "*" {
        return if projected {
            Mirrored::SetGrant(id, rel.relation.clone(), format!("{}:*", sobj.object_type))
        } else {
            Mirrored::Nothing
        };
    }
    let Some(subject_id) = db::strip(prefix, &sobj.object_id) else {
        return Mirrored::Nothing;
    };
    let set = format!(
        "{}:{subject_id}#{}",
        sobj.object_type, sub.optional_relation
    );
    let via = closure
        .vias
        .contains(&(ty.to_string(), rel.relation.clone()));
    match (projected, sub.optional_relation.is_empty()) {
        (true, true) if closure.arrows.contains(&rel.relation) => Mirrored::Edge(
            id,
            rel.relation.clone(),
            format!("{}:{subject_id}", sobj.object_type),
        ),
        (true, false) => Mirrored::SetGrant(id, rel.relation.clone(), set),
        (false, false) if via => {
            Mirrored::SetGrant(format!("{ty}:{id}"), rel.relation.clone(), set)
        }
        (_, true) if sobj.object_type == ctx.projection.subject_type => {
            Mirrored::Subject(format!("{}:{}", sobj.object_type, sobj.object_id))
        }
        _ => Mirrored::Nothing,
    }
}

async fn closure(ctx: &Ctx, db: &deadpool_postgres::Object) -> anyhow::Result<Closure> {
    Ok(Closure {
        types: db
            .query("select distinct resource_type from rebac.view_relation order by 1", &[])
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect(),
        arrows: db
            .query(
                "select relation from rebac.view_relation where kind = 'arrow' and resource_type = $1",
                &[&ctx.projection.resource_type],
            )
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect(),
        vias: db
            .query("select resource_type, relation from rebac.view_relation where kind = 'via'", &[])
            .await?
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect(),
    })
}

/// Re-projects each of `holders` (principals or grant nodes), `refresh_concurrency` at a time.
async fn project_all(
    ctx: &Ctx,
    tenant: &str,
    holders: Vec<String>,
    ok: &AtomicUsize,
    failed: &AtomicUsize,
) {
    stream::iter(holders)
        .for_each_concurrent(ctx.config.refresh_concurrency, |p| async move {
            match project::refresh_one(ctx, tenant, &p, None, ctx.config.deadline()).await {
                Ok(_) => ok.fetch_add(1, Ordering::Relaxed),
                Err(_) => failed.fetch_add(1, Ordering::Relaxed),
            };
        })
        .await;
}

/// Reads every relationship of the tenant's closure types and mirrors what narrowing needs. Returns the counts so far
/// and the subjects of the projected subject type that the relationships name.
async fn mirror(ctx: &Ctx, tenant: &str) -> anyhow::Result<(Backfilled, BTreeSet<String>)> {
    let db = ctx.pool.get().await?;
    let prefix: String = db
        .query_one("select rebac.prefix($1)", &[&tenant])
        .await?
        .get(0);
    let closure = closure(ctx, &db).await?;
    let mut out = Backfilled::default();
    let mut subjects = BTreeSet::new();
    for ty in &closure.types {
        for rel in ctx
            .spicedb
            .read_relationships(ty, &prefix, ctx.config.deadline())
            .await?
        {
            out.relationships += 1;
            match mirrored(ctx, &closure, ty, &prefix, &rel) {
                Mirrored::Edge(child, relation, parent) => {
                    let n = db
                        .execute(
                            "insert into rebac.edge (tenant, child, relation, parent) values ($1, $2, $3, $4)
                             on conflict do nothing",
                            &[&tenant, &child, &relation, &parent],
                        )
                        .await?;
                    out.edges += usize::from(n > 0);
                }
                Mirrored::SetGrant(container, relation, set) => {
                    db.execute(
                        "insert into rebac.set_grant (tenant, container, relation, subject_set) values ($1, $2, $3, $4)
                         on conflict do nothing",
                        &[&tenant, &container, &relation, &set],
                    )
                    .await?;
                }
                Mirrored::Subject(s) => {
                    subjects.insert(s);
                }
                Mirrored::Nothing => {}
            }
        }
    }
    Ok((out, subjects))
}

/// UPGRADING FROM 0.2: its mirrors lack grants to sets through a via relation and wildcard grants, which 0.2 did not
/// record. The install marks `remirror_pending`; this re-reads every tenant's relationships once to mirror them, then
/// forces the sweep, which projects the grant nodes they name. Reads stay open meanwhile on 0.2's rows.
///
/// # Errors
///
/// When SpiceDB cannot be read or the database refuses a write; the mark stays, and the next start retries.
pub async fn remirror_if_pending(ctx: &Ctx) -> anyhow::Result<()> {
    let db = ctx.pool.get().await?;
    let pending = db
        .query_opt(
            "select 1 from rebac.config where key = 'remirror_pending'",
            &[],
        )
        .await?
        .is_some();
    if !pending || ctx.config.mode != Mode::Live {
        return Ok(());
    }
    let tenants: Vec<String> = db
        .query(
            "select tenant from rebac.principal_state union select tenant from rebac.subject
             union select tenant from rebac.edge union select tenant from rebac.set_grant order by 1",
            &[],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    drop(db);
    for t in &tenants {
        mirror(ctx, t).await?;
    }
    ctx.pool
        .get()
        .await?
        .batch_execute(
            "delete from rebac.config where key = 'remirror_pending';
             update rebac.sweep_claim set last_at = now() - interval '1 year'",
        )
        .await?;
    tracing::info!(
        tenants = tenants.len(),
        "mirrors re-read after the upgrade; the sweep projects the grant nodes"
    );
    Ok(())
}

/// # Errors
///
/// When SpiceDB cannot be read or the database refuses a write. A principal that cannot be projected is counted in
/// `failed`, not an error.
pub async fn backfill(ctx: &Ctx, tenant: &str) -> anyhow::Result<Backfilled> {
    // A Watch position from before anything is projected, so the daemon resumes from it with no gap.
    crate::loops::ensure_watch_position(ctx).await?;
    let (mut out, subjects) = mirror(ctx, tenant).await?;
    let db = ctx.pool.get().await?;
    // Grant nodes first, so each principal after them stores only what its nodes do not hold. Shadow mode projects
    // every grant per principal and has no nodes.
    let nodes: Vec<String> = if ctx.config.mode == Mode::Live {
        db.query(
            "select distinct '#' || subject_set from rebac.set_grant where tenant = $1 and rebac.nodes_on() order by 1",
            &[&tenant],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect()
    } else {
        vec![]
    };
    let subjects: Vec<String> = subjects.into_iter().collect();
    let principals: Vec<String> = db
        .query(
            "select distinct p from (
               select rebac.principals_of($1, s) as p from unnest($2::text[]) s
               union select principal from rebac.principal_state where tenant = $1 and state <> 'retired'
               union select principal from rebac.subject where tenant = $1) x
             where p is not null and not rebac.is_node(p)",
            &[&tenant, &subjects],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    drop(db);
    out.principals = principals.len();
    out.nodes = nodes.len();
    let (ok, failed) = (AtomicUsize::new(0), AtomicUsize::new(0));
    project_all(ctx, tenant, nodes, &ok, &failed).await;
    project_all(ctx, tenant, principals, &ok, &failed).await;
    out.projected = ok.into_inner();
    out.failed = failed.into_inner();
    Ok(out)
}
