//! `rebacd verify`: the projection diffed against SpiceDB. EXTRA rows (the projection grants what SpiceDB does not)
//! are a fence violation and fail the command; MISSING rows (SpiceDB grants what the projection lacks) are a
//! freshness gap, reported and bounded. They are never summed. Only counts are printed: an id is tenant content.

use crate::{project, Ctx};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Default, Serialize)]
pub struct Diff {
    pub principals: usize,
    pub principals_with_extras: usize,
    pub principals_with_missing: usize,
    pub extra_rows: usize,
    pub missing_rows: usize,
    pub unavailable: usize,
}

impl std::ops::AddAssign for Diff {
    fn add_assign(&mut self, d: Diff) {
        self.principals += d.principals;
        self.principals_with_extras += d.principals_with_extras;
        self.principals_with_missing += d.principals_with_missing;
        self.extra_rows += d.extra_rows;
        self.missing_rows += d.missing_rows;
        self.unavailable += d.unavailable;
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Against {
    /// SpiceDB, minus pending revocations.
    SpiceDb,
    /// `rebac.readable_shadow` (what a shadow daemon projected), with no SpiceDB call.
    Shadow,
}

pub async fn diff(
    ctx: &Ctx,
    tenant: &str,
    sample: Option<i64>,
    against: Against,
) -> anyhow::Result<Diff> {
    let db = ctx.pool.get().await?;
    let principals: Vec<String> = db
        .query(
            "select principal from rebac.principal_state where tenant = $1 and state = 'projected'
               and not rebac.is_node(principal)
             order by random() limit $2",
            &[&tenant, &sample.unwrap_or(i64::MAX)],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut d = Diff::default();
    for p in principals {
        // What the principal reads, through its grant nodes too. The projection first, then the authority: a container revoked in between reads as missing, never extra.
        let have: BTreeSet<String> = db
            .query("select rebac.expanded($1, $2)", &[&tenant, &p])
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let want: BTreeSet<String> = match against {
            Against::SpiceDb => {
                let pending: BTreeSet<String> = db
                    .query("select rebac.pending_revoked($1, $2)", &[&tenant, &p])
                    .await?
                    .iter()
                    .map(|r| r.get(0))
                    .collect();
                let Ok((found, _)) =
                    project::lookup(ctx, &db, tenant, &p, None, ctx.config.deadline()).await
                else {
                    d.unavailable += 1;
                    continue;
                };
                found.into_iter().filter(|c| !pending.contains(c)).collect()
            }
            Against::Shadow => db
                .query("select container from rebac.readable_shadow where tenant = $1 and principal = $2", &[&tenant, &p])
                .await?
                .iter()
                .map(|r| r.get(0))
                .collect(),
        };
        d.principals += 1;
        let extra = have.difference(&want).count();
        let missing = want.difference(&have).count();
        d.extra_rows += extra;
        d.missing_rows += missing;
        d.principals_with_extras += usize::from(extra > 0);
        d.principals_with_missing += usize::from(missing > 0);
    }
    Ok(d)
}

/// Every tenant with a projected principal, summed. Extra and missing stay separate totals.
pub async fn all_tenants(
    ctx: &Ctx,
    sample: Option<i64>,
    against: Against,
) -> anyhow::Result<(usize, Diff)> {
    let db = ctx.pool.get().await?;
    let tenants: Vec<String> = db
        .query(
            "select distinct tenant from rebac.principal_state where state = 'projected'",
            &[],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    drop(db);
    let mut total = Diff::default();
    for t in &tenants {
        total += diff(ctx, t, sample, against).await?;
    }
    Ok((tenants.len(), total))
}

/// The cross-tenant check: every principal id that appears in more than one tenant, diffed in each.
pub async fn isolation(ctx: &Ctx) -> anyhow::Result<Diff> {
    let db = ctx.pool.get().await?;
    let tenants: Vec<String> = db
        .query(
            "select distinct tenant from rebac.principal_state where principal in
               (select principal from rebac.principal_state group by principal having count(distinct tenant) > 1)",
            &[],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut total = Diff::default();
    for t in tenants {
        total += diff(ctx, &t, None, Against::SpiceDb).await?;
    }
    Ok(total)
}
