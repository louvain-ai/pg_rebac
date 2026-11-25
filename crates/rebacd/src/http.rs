//! The lazy first read, over HTTP. A read that raises RB404 (never projected) asks here, then retries. The answer is
//! written down, an empty one included; a lookup SpiceDB cannot answer marks the principal unavailable (RB503 on the
//! next read, never an empty set) and queues it for retry.

use crate::{project, Ctx};
use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize)]
pub struct LookupBody {
    pub tenant: String,
    pub principal: String,
}

#[derive(Serialize)]
pub struct LookupReply {
    pub containers: Option<i32>,
    pub error: Option<&'static str>,
}

/// `/lookup`, `/status` and `/metrics` behind the bearer token when one is configured; `/healthz` always open (it says nothing).
pub fn router(ctx: Arc<Ctx>, token: Option<String>) -> Router {
    let guarded = Router::new()
        .route("/lookup", post(lookup))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route_layer(middleware::from_fn_with_state(
            Arc::new(token),
            require_token,
        ));
    Router::new()
        .merge(guarded)
        .route("/healthz", get(|| async { "ok" }))
        .with_state(ctx)
}

async fn require_token(
    State(token): State<Arc<Option<String>>>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(want) = token.as_deref() {
        let got = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !constant_time_eq(got.as_bytes(), want.as_bytes()) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "rebac_unauthorized" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Equal, in time that depends only on the lengths: a token is never compared byte by byte with an early exit.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

async fn lookup(
    State(ctx): State<Arc<Ctx>>,
    Json(b): Json<LookupBody>,
) -> (StatusCode, Json<LookupReply>) {
    // A grant node (`#...`) is projected by the daemon itself, never looked up by name.
    let refused = if b.tenant.is_empty() {
        Some("rebac_no_tenant")
    } else if b.principal.is_empty() || b.principal.starts_with('#') {
        Some("rebac_no_principal")
    } else {
        None
    };
    if let Some(error) = refused {
        return (
            StatusCode::BAD_REQUEST,
            Json(LookupReply {
                containers: None,
                error: Some(error),
            }),
        );
    }
    match project::refresh_one(
        &ctx,
        &b.tenant,
        &b.principal,
        None,
        ctx.config.read_deadline(),
    )
    .await
    {
        Ok(p) => {
            // The grant nodes it reads through that a read would refuse are re-projected too, so this read succeeds.
            if let Err(e) = refresh_stale_nodes(&ctx, &b.tenant, &b.principal).await {
                tracing::warn!(error = %crate::logs::kind(&e), "a grant node could not be re-projected");
            }
            (
                StatusCode::OK,
                Json(LookupReply {
                    containers: Some(p.containers),
                    error: None,
                }),
            )
        }
        Err(e) => {
            tracing::warn!(error = %crate::logs::kind(&e), "lazy lookup failed");
            // Unavailable until the queued refresh succeeds: the next read is RB503, never an empty set. If even this
            // cannot be written, the principal stays unprojected and the next read is RB404 again.
            let recorded = async {
                let db = ctx.pool.get().await?;
                db.execute(
                    "select rebac.mark_unavailable($1, $2, 'lookup_failed')",
                    &[&b.tenant, &b.principal],
                )
                .await?;
                db.execute(
                    "select rebac.enqueue_refresh($1, $2, 0::smallint, null)",
                    &[&b.tenant, &b.principal],
                )
                .await?;
                anyhow::Ok(())
            }
            .await;
            if let Err(e) = recorded {
                tracing::error!(error = %crate::logs::kind_of(&e), "could not record a failed lookup");
            }
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(LookupReply {
                    containers: None,
                    error: Some("rebac_unavailable"),
                }),
            )
        }
    }
}

/// One reading of `rebac.status()`: `GET /status` serves it as JSON, `GET /metrics` as gauges.
#[derive(Serialize)]
struct Status {
    heartbeat_age_ms: Option<i64>,
    stale: Option<bool>,
    pending_outbox: i64,
    quarantined: i64,
    refresh_queue: i64,
    external_writes: i64,
    unavailable_principals: i64,
    relay_lag_ms: Option<i64>,
    watch_age_ms: Option<i64>,
    oldest_lookup_ms: Option<i64>,
    unfresh_principals: i64,
}

impl Status {
    async fn read(ctx: &Ctx) -> anyhow::Result<Self> {
        let row = ctx
            .pool
            .get()
            .await?
            .query_one("select * from rebac.status()", &[])
            .await?;
        Ok(Self {
            heartbeat_age_ms: row.try_get("heartbeat_age_ms")?,
            stale: row.try_get("stale")?,
            pending_outbox: row.try_get("pending_outbox")?,
            quarantined: row.try_get("quarantined")?,
            refresh_queue: row.try_get("refresh_queue")?,
            external_writes: row.try_get("external_writes")?,
            unavailable_principals: row.try_get("unavailable_principals")?,
            relay_lag_ms: row.try_get("relay_lag_ms")?,
            watch_age_ms: row.try_get("watch_age_ms")?,
            oldest_lookup_ms: row.try_get("oldest_lookup_ms")?,
            unfresh_principals: row.try_get("unfresh_principals")?,
        })
    }

    /// Each number as a gauge: its name, its help text and its value (None when `rebac.status()` has none).
    fn gauges(&self) -> [(&'static str, &'static str, Option<i64>); 10] {
        [
            (
                "rebac_heartbeat_age_ms",
                "Age of the newest heartbeat",
                self.heartbeat_age_ms,
            ),
            (
                "rebac_pending_outbox",
                "Relationship writes not yet delivered to SpiceDB",
                Some(self.pending_outbox),
            ),
            (
                "rebac_quarantined",
                "Outbox rows SpiceDB refused on their own",
                Some(self.quarantined),
            ),
            (
                "rebac_refresh_queue",
                "Principals queued for a refresh",
                Some(self.refresh_queue),
            ),
            (
                "rebac_external_writes",
                "Changes seen on Watch that did not come through rebac.write",
                Some(self.external_writes),
            ),
            (
                "rebac_unavailable_principals",
                "Principals whose last lookup failed (RB503)",
                Some(self.unavailable_principals),
            ),
            (
                "rebac_relay_lag_ms",
                "Age of the oldest undelivered outbox row",
                self.relay_lag_ms,
            ),
            (
                "rebac_watch_age_ms",
                "Time since Watch was last current",
                self.watch_age_ms,
            ),
            (
                "rebac_oldest_lookup_ms",
                "Age of the oldest projected lookup",
                self.oldest_lookup_ms,
            ),
            (
                "rebac_unfresh_principals",
                "Principals past the freshness bound or of unknown age",
                Some(self.unfresh_principals),
            ),
        ]
    }
}

/// Prometheus text: `rebac.status()`'s numbers as gauges, plus this daemon's own leader and Watch state. Counts and
/// ages only: no tenant, principal or container id is ever a label.
async fn metrics(State(ctx): State<Arc<Ctx>>) -> Response {
    use std::fmt::Write as _;
    use std::sync::atomic::Ordering;
    let status = Status::read(&ctx).await;
    let mut gauges = vec![
        (
            "rebacd_leader",
            "1 while this daemon holds the leader lock (or leader election is off)",
            Some(i64::from(crate::leader::is_leader(&ctx))),
        ),
        (
            "rebacd_watch_current",
            "1 while this daemon's Watch stream is caught up and applying",
            Some(i64::from(ctx.watch_current.load(Ordering::SeqCst))),
        ),
        (
            "rebac_up",
            "1 when rebac.status() answered",
            Some(i64::from(status.is_ok())),
        ),
    ];
    if let Ok(status) = &status {
        gauges.extend(status.gauges());
    }
    let mut out = String::new();
    for (name, help, value) in gauges {
        if let Some(value) = value {
            // Writing to a String cannot fail.
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
            );
        }
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        out,
    )
        .into_response()
}

async fn status(State(ctx): State<Arc<Ctx>>) -> (StatusCode, Json<serde_json::Value>) {
    match Status::read(&ctx).await.map(serde_json::to_value) {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "database_unavailable" })),
        ),
    }
}

async fn refresh_stale_nodes(
    ctx: &Ctx,
    tenant: &str,
    principal: &str,
) -> Result<(), project::ProjectError> {
    let nodes: Vec<String> = ctx
        .pool
        .get()
        .await?
        .query("select rebac.stale_nodes($1, $2)", &[&tenant, &principal])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    for n in nodes {
        project::refresh_one(ctx, tenant, &n, None, ctx.config.read_deadline()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn tokens_compare_whole() {
        assert!(constant_time_eq(b"s3cret", b"s3cret"));
        assert!(!constant_time_eq(b"s3cret", b"s3creT"));
        assert!(!constant_time_eq(b"s3cret", b"s3cret!"));
        assert!(!constant_time_eq(b"", b"s3cret"));
        assert!(!constant_time_eq(&[0u8; 256], &[0u8; 0]));
    }
}
