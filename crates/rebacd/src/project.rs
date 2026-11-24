//! Projecting one principal: lock it, read its pending revocations, ask SpiceDB, write what is left.

use crate::config::Mode;
use crate::{db, spicedb::is_rejected_subject, Ctx};
use std::{collections::BTreeSet, time::Duration};
use tonic::Status;

#[derive(Debug)]
pub enum ProjectError {
    /// SpiceDB could not answer: the projection is unknown, never empty.
    Unavailable(Status),
    Db(anyhow::Error),
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectError::Unavailable(s) => {
                write!(f, "spicedb unavailable: {:?} {}", s.code(), s.message())
            }
            ProjectError::Db(e) => write!(f, "database: {e}"),
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for ProjectError {
    fn from(e: E) -> Self {
        ProjectError::Db(e.into())
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Projected {
    pub containers: i32,
    /// Subjects SpiceDB refused to parse: each contributes nothing (fail closed for that identity).
    pub rejected: usize,
    /// Ids SpiceDB returned under another tenant's prefix: dropped.
    pub foreign: usize,
}

/// The containers a principal's subjects hold, per SpiceDB, stripped of the tenant prefix.
pub async fn lookup(
    ctx: &Ctx,
    db: &impl deadpool_postgres::GenericClient,
    tenant: &str,
    principal: &str,
    token: Option<&str>,
    deadline: Duration,
) -> Result<(BTreeSet<String>, Projected), ProjectError> {
    let subjects: Vec<String> = db
        .query("select rebac.subjects_of($1, $2)", &[&tenant, &principal])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let prefix: String = db
        .query_one("select rebac.prefix($1)", &[&tenant])
        .await?
        .get(0);
    let mut out = BTreeSet::new();
    let mut stats = Projected::default();
    for s in &subjects {
        match ctx
            .spicedb
            .lookup_resources(
                &ctx.projection.resource_type,
                &ctx.projection.permission,
                s,
                token,
                deadline,
            )
            .await
        {
            Ok(ids) => {
                for id in ids {
                    match db::strip(&prefix, &id) {
                        Some(c) => {
                            out.insert(c);
                        }
                        None => stats.foreign += 1,
                    }
                }
            }
            Err(e) if is_rejected_subject(&e) => stats.rejected += 1,
            Err(e) => return Err(ProjectError::Unavailable(e)),
        }
    }
    Ok((out, stats))
}

/// Re-project one principal. The principal's lock is taken BEFORE SpiceDB is asked, so a lookup that began before a
/// revocation cannot finish after it and write the grant back; pending revocations are read before the lookup too.
pub async fn refresh_one(
    ctx: &Ctx,
    tenant: &str,
    principal: &str,
    token: Option<&str>,
    deadline: Duration,
) -> Result<Projected, ProjectError> {
    let mut client = ctx.pool.get().await.map_err(anyhow::Error::from)?;
    let tx = client.transaction().await?;
    tx.execute(
        "select rebac.lock_principal($1, $2)",
        &[&tenant, &principal],
    )
    .await?;
    let pending: BTreeSet<String> = tx
        .query(
            "select rebac.pending_revoked($1, $2)",
            &[&tenant, &principal],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let (found, mut stats) = lookup(ctx, &tx, tenant, principal, token, deadline).await?;
    let containers: Vec<String> = found.into_iter().filter(|c| !pending.contains(c)).collect();
    let target = match ctx.config.mode {
        Mode::Live => "live",
        Mode::Shadow => "shadow",
    };
    stats.containers = tx
        .query_one(
            "select rebac.project($1, $2, $3, $4, $5)",
            &[&tenant, &principal, &containers, &token, &target],
        )
        .await?
        .get(0);
    tx.commit().await?;
    if stats.rejected > 0 || stats.foreign > 0 {
        tracing::warn!(
            rejected = stats.rejected,
            foreign = stats.foreign,
            "subjects or ids skipped during a projection"
        );
    }
    Ok(stats)
}
