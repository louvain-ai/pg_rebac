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
///
/// # Errors
///
/// [`ProjectError::Unavailable`] when SpiceDB cannot answer for a subject it accepts; [`ProjectError::Db`] on a
/// database error.
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
    let row = db
        .query_one(
            "select rebac.prefix($1), coalesce(rebac.setting('wildcard_probe'), '__rebac_anyone__')",
            &[&tenant],
        )
        .await?;
    let (prefix, probe): (String, String) = (row.get(0), row.get(1));
    let mut out = BTreeSet::new();
    let mut stats = Projected::default();
    for s in &subjects {
        // A wildcard node ('#user:*') reads as `user:*`, which SpiceDB cannot look up: it is looked up as a stand-in
        // that holds no grant of its own, and rebac.project keeps only what the mirror grants to the wildcard.
        let subject = match s.strip_suffix(":*") {
            Some(ty) if !ty.contains('#') => format!("{ty}:{prefix}{probe}"),
            _ => s.clone(),
        };
        match ctx
            .spicedb
            .lookup_resources(
                &ctx.projection.resource_type,
                &ctx.projection.permission,
                &subject,
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

/// The grant nodes a principal stands for, per SpiceDB: for each subject-set relation the closure names
/// (`group#member`), the sets each of its subjects is a member of, as `#group:eng#member`. `None` when grant nodes are
/// off, for a node itself, and in shadow mode (which projects every grant per principal).
///
/// # Errors
///
/// [`ProjectError::Unavailable`] when SpiceDB cannot answer: never fewer memberships. [`ProjectError::Db`] on a
/// database error.
pub async fn memberships(
    ctx: &Ctx,
    db: &impl deadpool_postgres::GenericClient,
    tenant: &str,
    principal: &str,
    token: Option<&str>,
    deadline: Duration,
) -> Result<Option<Vec<String>>, ProjectError> {
    let on: bool = db.query_one("select rebac.nodes_on()", &[]).await?.get(0);
    if !on || principal.starts_with('#') || ctx.config.mode != Mode::Live {
        return Ok(None);
    }
    let sets: Vec<(String, String)> = db
        .query(
            "select resource_type, relation from rebac.view_relation where kind = 'set' order by 1, 2",
            &[],
        )
        .await?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let mut nodes = BTreeSet::new();
    if sets.is_empty() {
        return Ok(Some(vec![]));
    }
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
    for s in subjects.iter().filter(|s| !s.contains('#')) {
        for (ty, rel) in &sets {
            match ctx
                .spicedb
                .lookup_resources(ty, rel, s, token, deadline)
                .await
            {
                Ok(ids) => {
                    for id in ids {
                        if let Some(set) = db::strip(&prefix, &id) {
                            nodes.insert(format!("#{ty}:{set}#{rel}"));
                        }
                    }
                }
                Err(e) if is_rejected_subject(&e) => {}
                Err(e) => return Err(ProjectError::Unavailable(e)),
            }
        }
    }
    Ok(Some(nodes.into_iter().collect()))
}

/// Re-project one principal. The principal's lock is taken BEFORE SpiceDB is asked, so a lookup that began before a
/// revocation cannot finish after it and write the grant back; pending revocations are read before the lookup too.
///
/// # Errors
///
/// As [`lookup`] and [`memberships`]; nothing is written then.
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
    // A change seen on Watch since this refresh was claimed re-queues it FULLY CONSISTENT (zedtoken ''); it committed
    // before this lock was granted, so read it now and never look up at the older token it was claimed with.
    let requeued_full: bool = tx
        .query_one(
            "select exists (select 1 from rebac.refresh where tenant = $1 and principal = $2 and zedtoken = '')",
            &[&tenant, &principal],
        )
        .await?
        .get(0);
    let token = if requeued_full { None } else { token };
    // Memberships a pending revocation ends, read before SpiceDB is asked for the same reason as `pending`.
    let left: BTreeSet<String> = tx
        .query(
            "select rebac.pending_left_nodes($1, $2)",
            &[&tenant, &principal],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    // FRESHNESS is stamped from this database's clock, just before SpiceDB is asked: the set reflects every change
    // SpiceDB held at this instant (or, at_least_as_fresh, within SpiceDB's revision quantization of it).
    let looked_up_at: std::time::SystemTime =
        tx.query_one("select clock_timestamp()", &[]).await?.get(0);
    let (found, mut stats) = lookup(ctx, &tx, tenant, principal, token, deadline).await?;
    let nodes: Option<Vec<String>> = memberships(ctx, &tx, tenant, principal, token, deadline)
        .await?
        .map(|n| n.into_iter().filter(|x| !left.contains(x)).collect());
    let containers: Vec<String> = found.into_iter().filter(|c| !pending.contains(c)).collect();
    let target = match ctx.config.mode {
        Mode::Live => "live",
        Mode::Shadow => "shadow",
    };
    stats.containers = tx
        .query_one(
            "select rebac.project($1, $2, $3, $4, $5, $6, $7)",
            &[
                &tenant,
                &principal,
                &containers,
                &token,
                &target,
                &looked_up_at,
                &nodes,
            ],
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
