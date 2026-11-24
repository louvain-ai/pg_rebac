use clap::{Parser, Subcommand};
use rebacd::{
    backfill, closure,
    config::{Config, Mode},
    http, loops, verify, Ctx,
};
use std::{path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(name = "rebacd", version, about = "The pg_rebac sync daemon")]
struct Cli {
    /// rebac.toml; without one, the defaults (DATABASE_URL, SPICEDB_TOKEN, SpiceDB on 127.0.0.1:50051).
    #[arg(long, global = true, env = "REBACD_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the loops: relay, refresh, sweep, heartbeat, Watch (applies changes made outside rebac.write), the closure
    /// check, and the lazy-lookup endpoint.
    Run {
        /// Replace rebac.view_relation when the schema's closure differs from it (and re-project everyone).
        #[arg(long)]
        accept_schema: bool,
        /// Write only rebac.readable_shadow and relay nothing (overrides the config's mode).
        #[arg(long)]
        shadow: bool,
    },
    /// Project every principal a tenant's relationships name, and mirror its arrow edges.
    Backfill {
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        shadow: bool,
    },
    /// Diff the projection: against SpiceDB (default), across tenants (--isolation), the closure (--closure), or a
    /// shadow projection (--shadow). Exits 2 on any EXTRA row or a closure mismatch. Prints counts only.
    Verify {
        #[arg(long)]
        tenant: Option<String>,
        #[arg(long)]
        sample: Option<i64>,
        #[arg(long)]
        isolation: bool,
        #[arg(long)]
        closure: bool,
        #[arg(long)]
        shadow: bool,
        /// Every tenant with a projected principal, printed as ONE line of totals (for a scheduled job; no ids).
        #[arg(long)]
        all_tenants: bool,
    },
    /// Print the closure derived from the live schema; --write stores it in rebac.view_relation and rebuilds every
    /// tenant (reads fail closed meanwhile).
    Closure {
        #[arg(long)]
        write: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // REBACD_LOG_FORMAT=json: one JSON object a line (for a log pipeline that reads fields); otherwise text.
    let filter =
        || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if std::env::var("REBACD_LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_ansi(false)
            .with_env_filter(filter())
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter()).init();
    }
    let cli = Cli::parse();
    let mut config = Config::load(cli.config.as_deref())?;
    match cli.cmd {
        Cmd::Run {
            accept_schema,
            shadow,
        } => {
            if shadow {
                config.mode = Mode::Shadow;
            }
            let ctx = Arc::new(Ctx::connect(config).await?);
            closure::check_at_start(&ctx, accept_schema).await?;
            run(ctx).await
        }
        Cmd::Backfill { tenant, shadow } => {
            if shadow {
                config.mode = Mode::Shadow;
            }
            let ctx = Ctx::connect(config).await?;
            closure::check_at_start(&ctx, false).await?;
            let b = backfill::backfill(&ctx, &tenant).await?;
            println!("{}", serde_json::to_string(&b)?);
            if b.failed > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Verify {
            tenant,
            sample,
            isolation,
            closure: check_closure,
            shadow,
            all_tenants,
        } => {
            let ctx = Ctx::connect(config).await?;
            let mut bad = false;
            if check_closure {
                let derived = closure::derive(&ctx).await?;
                let stored = closure::stored(&ctx.pool.get().await?).await?;
                let ok = derived == stored;
                bad |= !ok;
                println!(
                    "{}",
                    serde_json::json!({ "closure_matches": ok, "derived": derived.len(), "stored": stored.len() })
                );
            }
            if isolation {
                let d = verify::isolation(&ctx).await?;
                bad |= d.extra_rows > 0;
                println!("{}", serde_json::json!({ "isolation": d }));
            }
            if all_tenants {
                let against = if shadow {
                    verify::Against::Shadow
                } else {
                    verify::Against::SpiceDb
                };
                let (tenants, d) = verify::all_tenants(&ctx, sample, against).await?;
                bad |= d.extra_rows > 0;
                println!(
                    "{}",
                    serde_json::json!({ "verify": if shadow { "shadow" } else { "spicedb" }, "tenants": tenants, "totals": d })
                );
            }
            if let Some(t) = tenant {
                let against = if shadow {
                    verify::Against::Shadow
                } else {
                    verify::Against::SpiceDb
                };
                let d = verify::diff(&ctx, &t, sample, against, "readable").await?;
                bad |= d.extra_rows > 0;
                println!(
                    "{}",
                    serde_json::json!({ if shadow { "shadow" } else { "spicedb" }: d })
                );
            }
            if bad {
                std::process::exit(2);
            }
            Ok(())
        }
        Cmd::Closure { write } => {
            let ctx = Ctx::connect(config).await?;
            let derived = closure::derive(&ctx).await?;
            for r in &derived {
                println!(
                    "{}#{} {} target={} through={}",
                    r.resource_type,
                    r.relation,
                    r.kind.as_str(),
                    r.target_type.as_deref().unwrap_or("-"),
                    r.through.as_deref().unwrap_or("-")
                );
            }
            if write {
                // Through the rebuild, so reads fail closed until every tenant is re-read under the new list.
                closure::rebuild(&ctx, &derived, "closure_written").await?;
            }
            Ok(())
        }
    }
}

async fn run(ctx: Arc<Ctx>) -> anyhow::Result<()> {
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let mut tasks = vec![
        (
            "refresh",
            tokio::spawn(loops::refresh_loop(ctx.clone(), stop.clone())),
        ),
        (
            "sweep",
            tokio::spawn(loops::sweep_loop(ctx.clone(), stop.clone())),
        ),
        (
            "heartbeat",
            tokio::spawn(loops::heartbeat_loop(ctx.clone(), stop.clone())),
        ),
        (
            "closure",
            tokio::spawn(loops::closure_loop(ctx.clone(), stop.clone())),
        ),
    ];
    if ctx.config.mode == Mode::Live {
        tasks.push((
            "relay",
            tokio::spawn(loops::relay_loop(ctx.clone(), stop.clone())),
        ));
    }
    if ctx.config.watch {
        tasks.push((
            "watch",
            tokio::spawn(loops::watch_loop(ctx.clone(), stop.clone())),
        ));
    }
    let listener = tokio::net::TcpListener::bind(&ctx.config.listen).await?;
    tracing::info!(listen = %ctx.config.listen, mode = ?ctx.config.mode, "rebacd running");
    let mut srv_stop = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, http::router(ctx))
            .with_graceful_shutdown(async move {
                let _ = srv_stop.changed().await;
            })
            .await
    });
    shutdown_signal().await;
    tracing::info!("stopping: waiting for the passes in flight");
    stop_tx.send(true)?;
    // Each pass is bounded by its own deadlines; a loop that outlives them is abandoned with a warning.
    for (name, t) in tasks {
        if tokio::time::timeout(std::time::Duration::from_secs(70), t)
            .await
            .is_err()
        {
            tracing::warn!(task = name, "a loop did not stop in time");
        }
    }
    if tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .is_err()
    {
        tracing::warn!(task = "http", "the HTTP server did not stop in time");
    }
    tracing::info!("stopped");
    Ok(())
}

/// SIGINT or SIGTERM (what a container runtime sends). Either stops the loops after the passes in flight.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install a SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
