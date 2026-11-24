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

/// `/lookup` and `/status` behind the bearer token when one is configured; `/healthz` always open (it says nothing).
pub fn router(ctx: Arc<Ctx>, token: Option<String>) -> Router {
    let guarded = Router::new()
        .route("/lookup", post(lookup))
        .route("/status", get(status))
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
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8 | u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
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
            let kind = match &e {
                project::ProjectError::Unavailable(st) => format!("grpc:{:?}", st.code()),
                project::ProjectError::Db(err) => crate::logs::kind_of(err),
            };
            tracing::warn!(error = %kind, "lazy lookup failed");
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
                        unavailable_principals, relay_lag_ms, watch_age_ms, oldest_lookup_ms, unfresh_principals
                 from rebac.status()",
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
            "relay_lag_ms": row.get::<_, Option<i64>>(7),
            "watch_age_ms": row.get::<_, Option<i64>>(8),
            "oldest_lookup_ms": row.get::<_, Option<i64>>(9),
            "unfresh_principals": row.get::<_, i64>(10),
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
