//! Backfill a tenant: page through its relationships, mirror its arrow edges, and project every principal they name.

use crate::{project, Ctx};
use futures::{stream, StreamExt};
use serde::Serialize;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Default, Serialize)]
pub struct Backfilled {
    pub relationships: usize,
    pub edges: usize,
    pub principals: usize,
    pub projected: usize,
    pub failed: usize,
}

pub async fn backfill(ctx: &Ctx, tenant: &str) -> anyhow::Result<Backfilled> {
    let db = ctx.pool.get().await?;
    let prefix: String = db
        .query_one("select rebac.prefix($1)", &[&tenant])
        .await?
        .get(0);
    let types: Vec<String> = db
        .query(
            "select distinct resource_type from rebac.view_relation",
            &[],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let arrows: BTreeSet<String> = db
        .query(
            "select relation from rebac.view_relation where kind = 'arrow' and resource_type = $1",
            &[&ctx.projection.resource_type],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut out = Backfilled::default();
    let mut subjects = BTreeSet::new();
    for ty in &types {
        for rel in ctx
            .spicedb
            .read_relationships(ty, &prefix, ctx.config.deadline())
            .await?
        {
            out.relationships += 1;
            let (Some(res), Some(sub)) = (&rel.resource, &rel.subject) else {
                continue;
            };
            let Some(sobj) = &sub.object else { continue };
            if *ty == ctx.projection.resource_type
                && arrows.contains(&rel.relation)
                && sub.optional_relation.is_empty()
            {
                let (Some(child), Some(parent)) = (
                    crate::db::strip(&prefix, &res.object_id),
                    crate::db::strip(&prefix, &sobj.object_id),
                ) else {
                    continue;
                };
                out.edges += db
                    .execute(
                        "insert into rebac.edge (tenant, child, relation, parent) values ($1, $2, $3, $4) on conflict do nothing",
                        &[&tenant, &child, &rel.relation, &format!("{}:{parent}", sobj.object_type)],
                    )
                    .await? as usize;
            } else if *ty == ctx.projection.resource_type && !sub.optional_relation.is_empty() {
                // A grant to a subject set: mirrored, so a membership's removal narrows without SpiceDB.
                let (Some(container), Some(set_id)) = (
                    crate::db::strip(&prefix, &res.object_id),
                    crate::db::strip(&prefix, &sobj.object_id),
                ) else {
                    continue;
                };
                db.execute(
                    "insert into rebac.set_grant (tenant, container, relation, subject_set) values ($1, $2, $3, $4)
                     on conflict do nothing",
                    &[&tenant, &container, &rel.relation, &format!("{}:{set_id}#{}", sobj.object_type, sub.optional_relation)],
                )
                .await?;
            } else if sobj.object_type == ctx.projection.subject_type
                && sub.optional_relation.is_empty()
            {
                subjects.insert(format!("{}:{}", sobj.object_type, sobj.object_id));
            }
        }
    }
    let subjects: Vec<String> = subjects.into_iter().collect();
    let principals: Vec<String> = db
        .query(
            "select distinct p from (
               select rebac.principals_of($1, s) as p from unnest($2::text[]) s
               union select principal from rebac.principal_state where tenant = $1 and state <> 'retired'
               union select principal from rebac.subject where tenant = $1) x where p is not null",
            &[&tenant, &subjects],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    drop(db);
    out.principals = principals.len();
    let ok = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    stream::iter(principals)
        .for_each_concurrent(ctx.config.refresh_concurrency, |p| {
            let (ok, failed) = (&ok, &failed);
            async move {
                match project::refresh_one(ctx, tenant, &p, None, ctx.config.deadline()).await {
                    Ok(_) => ok.fetch_add(1, Ordering::Relaxed),
                    Err(_) => failed.fetch_add(1, Ordering::Relaxed),
                };
            }
        })
        .await;
    out.projected = ok.into_inner();
    out.failed = failed.into_inner();
    Ok(out)
}
