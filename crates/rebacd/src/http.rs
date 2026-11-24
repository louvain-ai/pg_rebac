//! The lazy first read, over HTTP. A read that raises RB404 (never projected) asks here, then retries. The answer is
//! written down, an empty one included; a lookup SpiceDB cannot answer marks the principal unavailable (RB503 on the
//! next read, never an empty set) and queues it for retry.

use crate::{project, Ctx};
use axum::{
    extract::State,
    http::StatusCode,
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

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/lookup", post(lookup))
        .route("/healthz", get(|| async { "ok" }))
        .route("/status", get(status))
        .with_state(ctx)
}

async fn lookup(
    State(ctx): State<Arc<Ctx>>,
    Json(b): Json<LookupBody>,
) -> (StatusCode, Json<LookupReply>) {
    if b.tenant.is_empty() || b.principal.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(LookupReply {
                containers: None,
                error: Some("rebac_no_tenant"),
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
        Ok(p) => (
            StatusCode::OK,
            Json(LookupReply {
                containers: Some(p.containers),
                error: None,
            }),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "lazy lookup failed");
            if let Ok(c) = ctx.pool.get().await {
                let _ = c
                    .execute(
                        "select rebac.mark_unavailable($1, $2, 'lookup_failed')",
                        &[&b.tenant, &b.principal],
                    )
                    .await;
                let _ = c
                    .execute(
                        "select rebac.enqueue_refresh($1, $2, 0::smallint, null)",
                        &[&b.tenant, &b.principal],
                    )
                    .await;
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

async fn status(State(ctx): State<Arc<Ctx>>) -> (StatusCode, Json<serde_json::Value>) {
    let r = async {
        let c = ctx.pool.get().await?;
        let row = c
            .query_one(
                "select heartbeat_age_ms, stale, pending_outbox, quarantined, refresh_queue, external_writes,
                        unavailable_principals from rebac.status()",
                &[],
            )
            .await?;
        anyhow::Ok(serde_json::json!({
            "heartbeat_age_ms": row.get::<_, Option<i64>>(0),
            "stale": row.get::<_, Option<bool>>(1),
            "pending_outbox": row.get::<_, i64>(2),
            "quarantined": row.get::<_, i64>(3),
            "refresh_queue": row.get::<_, i64>(4),
            "external_writes": row.get::<_, i64>(5),
            "unavailable_principals": row.get::<_, i64>(6),
        }))
    }
    .await;
    match r {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "database_unavailable" })),
        ),
    }
}
