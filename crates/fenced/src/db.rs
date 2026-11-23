//! Postgres access. Every step the daemon takes is a `fence.*` SQL function; this module only calls them.

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::NoTls;

pub fn pool(url: &str, size: usize) -> anyhow::Result<Pool> {
    let pg: tokio_postgres::Config = url.parse()?;
    let mgr = Manager::from_config(
        pg,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Ok(Pool::builder(mgr).max_size(size).build()?)
}

/// What is projected, read from `fence.config`.
#[derive(Debug, Clone)]
pub struct Projection {
    pub resource_type: String,
    pub permission: String,
    pub subject_type: String,
}

impl Projection {
    pub async fn load(db: &impl deadpool_postgres::GenericClient) -> anyhow::Result<Self> {
        let get = |k: &'static str| async move {
            let row = db.query_one("select fence.setting($1)", &[&k]).await?;
            row.get::<_, Option<String>>(0)
                .ok_or_else(|| anyhow::anyhow!("fence.config has no `{k}`"))
        };
        Ok(Self {
            resource_type: get("resource_type").await?,
            permission: get("permission").await?,
            subject_type: get("subject_type").await?,
        })
    }
}

/// An object id with this tenant's prefix removed, or None when it carries another prefix: never a tenant to trust.
pub fn strip(prefix: &str, id: &str) -> Option<String> {
    if prefix.is_empty() {
        Some(id.to_string())
    } else {
        id.strip_prefix(prefix).map(str::to_string)
    }
}
