//! rebacd: the pg_rebac sync daemon.

#[allow(clippy::all, missing_docs)]
pub mod authzed {
    pub mod api {
        pub mod v1 {
            tonic::include_proto!("authzed.api.v1");
        }
    }
}

#[allow(clippy::all, missing_docs)]
pub mod google {
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
}

pub mod backfill;
pub mod closure;
pub mod config;
pub mod db;
pub mod http;
pub mod logs;
pub mod loops;
pub mod project;
pub mod relay;
pub mod spicedb;
pub mod tls;
pub mod verify;

/// Everything a loop needs.
pub struct Ctx {
    pub config: config::Config,
    pub pool: deadpool_postgres::Pool,
    pub spicedb: spicedb::SpiceDb,
    pub projection: db::Projection,
    /// Watch kicks this when SpiceDB reports a schema change, so the closure is re-checked at once.
    pub closure_kick: tokio::sync::Notify,
}

impl Ctx {
    pub async fn connect(config: config::Config) -> anyhow::Result<Self> {
        let pool = db::pool(&config.database_url()?, config.refresh_concurrency + 6)?;
        let spicedb = spicedb::SpiceDb::connect(
            &config.spicedb_endpoint,
            &config.spicedb_token()?,
            config.consistency,
        )
        .await?;
        let projection = db::Projection::load(&pool.get().await?).await?;
        Ok(Self {
            config,
            pool,
            spicedb,
            projection,
            closure_kick: tokio::sync::Notify::new(),
        })
    }
}
