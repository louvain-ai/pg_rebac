//! One full sweep, timed against a real SpiceDB, with grant nodes off (0.2's per-principal expansion) and on.
//!
//! The tenant is `examples/projection-scale/model.sql`'s: teams of 20, departments of 500, an all-company group, a
//! broad group, 20 direct shares each, 100,000 containers. It is written to SpiceDB, backfilled, and then every
//! principal (and every node) is queued at sweep priority and worked by the daemon's own refresh pass.
//!
//! ```text
//! docker run -d -p 127.0.0.1:50262:50051 authzed/spicedb:v1.56.2 serve \
//!   --grpc-preshared-key rebac-test-key --datastore-engine memory
//! cargo run --release -p rebacd --example sweep_bench -- <principals> [concurrency]
//! ```
//!
//! `REBAC_BENCH_PG` (default `postgres://postgres:rebac@127.0.0.1:55644`) and `REBAC_BENCH_SPICEDB` (default
//! `http://127.0.0.1:50262`) point elsewhere. The SpiceDB store is cleared first: use one that holds nothing else.

use rebacd::{
    backfill, closure,
    config::{Config, Consistency, Mode},
    loops,
    spicedb::{SpiceDb, TupleOp},
    Ctx,
};
use std::time::{Duration, Instant};

const SCHEMA: &str = r"
definition user {}
definition group {
  relation member: user
}
definition container {
  relation parent: container
  relation member: user | group#member
  relation editor: user
  permission view = member + editor + parent->view
}
";
const TENANT: &str = "tenant-0001";
const KEY: &str = "rebac-test-key";
const CONTAINERS: u64 = 100_000;

type Error = Box<dyn std::error::Error>;

/// A stable 36-character id, as long as a UUID: the lengths matter to the database, not the bytes.
fn id(s: &str) -> String {
    let mut h: u128 = 0xcbf2_9ce4_8422_2325_0000_0000_0000_0001;
    for b in s.bytes() {
        h ^= u128::from(b);
        h = h.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    let x = format!("{h:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &x[0..8],
        &x[8..12],
        &x[12..16],
        &x[16..20],
        &x[20..32]
    )
}

/// splitmix64 over (principal, share): where a principal's direct shares land.
fn spread(principal: u64, share: u64) -> u64 {
    let mut x = (principal * 31 + share) ^ 0x9E37_79B9_7F4A_7C15;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (x ^ (x >> 31)) % CONTAINERS
}

fn relationships(principals: u64) -> Vec<TupleOp> {
    let container = |j: u64| format!("container:{TENANT}/{}", id(&format!("c{j}")));
    let user = |i: u64| format!("user:{TENANT}/{}", id(&format!("p{i}")));
    let group = |name: &str| format!("group:{TENANT}/{}", id(name));
    let grant = |resource: String, subject: String| TupleOp {
        touch: true,
        resource,
        relation: "member".into(),
        subject,
    };
    let mut out = vec![];
    for team in 0..=(principals - 1) / 20 {
        let set = group(&format!("team{team}")) + "#member";
        out.extend((0..5).map(|k| grant(container((team * 5 + k) % 30_000), set.clone())));
    }
    for dept in 0..=(principals - 1) / 500 {
        let set = group(&format!("dept{dept}")) + "#member";
        out.extend(
            (0..100).map(|k| grant(container(30_000 + (dept * 100 + k) % 20_000), set.clone())),
        );
    }
    out.extend((0..1_000).map(|k| grant(container(50_000 + k), group("all") + "#member")));
    out.extend((0..20_000).map(|k| grant(container(51_000 + k), group("broad") + "#member")));
    for i in 0..principals {
        out.push(grant(group(&format!("team{}", i / 20)), user(i)));
        out.push(grant(group(&format!("dept{}", i / 500)), user(i)));
        out.push(grant(group("all"), user(i)));
        if i % 10_000 == 7 {
            out.push(grant(group("broad"), user(i)));
        }
        out.extend((0..20).map(|k| grant(container(spread(i, k)), user(i))));
    }
    out.sort_by(|a, b| (&a.resource, &a.subject).cmp(&(&b.resource, &b.subject)));
    out.dedup_by(|a, b| (&a.resource, &a.subject) == (&b.resource, &b.subject));
    out
}

async fn fresh_db(pg: &str, name: &str) -> Result<String, Error> {
    let (admin, conn) =
        tokio_postgres::connect(&format!("{pg}/postgres"), tokio_postgres::NoTls).await?;
    tokio::spawn(conn);
    admin
        .batch_execute(&format!("drop database if exists {name} with (force)"))
        .await?;
    admin
        .batch_execute(&format!("create database {name}"))
        .await?;
    let url = format!("{pg}/{name}");
    let (db, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../sql/rebac.sql"))?;
    db.batch_execute(&format!("set client_min_messages = warning; {sql}"))
        .await?;
    Ok(url)
}

/// Backfill, then one sweep: (backfill seconds, lookups worked, sweep seconds). A lookup that fails (SpiceDB ends a
/// connection at its maximum age) is retried by the refresh queue, as in production, and counted in the sweep.
async fn sweep(ctx: &Ctx, concurrency: usize) -> Result<(f64, usize, f64), Error> {
    let started = Instant::now();
    backfill::backfill(ctx, TENANT).await?;
    let backfill_s = started.elapsed().as_secs_f64();
    ctx.pool
        .get()
        .await?
        .batch_execute("delete from rebac.sweep_claim; select rebac.sweep(1, 0, false)")
        .await?;
    let started = Instant::now();
    let mut worked = 0;
    loop {
        let pass = loops::refresh_pass(ctx, concurrency).await?;
        worked += pass;
        if pass == 0 {
            // A failed refresh backs off; make it due rather than wait out its ladder.
            let left: i64 = ctx
                .pool
                .get()
                .await?
                .query_one(
                    "with due as (update rebac.refresh set due_at = now(), leased_until = null returning 1)
                     select count(*) from due",
                    &[],
                )
                .await?
                .get(0);
            if left == 0 {
                break;
            }
        }
    }
    Ok((backfill_s, worked, started.elapsed().as_secs_f64()))
}

async fn daemon(endpoint: &str, concurrency: usize, pg: &str, nodes: &str) -> Result<Ctx, Error> {
    std::env::set_var(
        "REBAC_BENCH_DB",
        fresh_db(pg, &format!("rebac_sweep_{nodes}")).await?,
    );
    let ctx = Ctx::connect(Config {
        database_url_env: "REBAC_BENCH_DB".into(),
        spicedb_endpoint: endpoint.into(),
        spicedb_token_env: "REBAC_BENCH_TOKEN".into(),
        mode: Mode::Live,
        deadline_ms: 120_000,
        read_deadline_ms: 120_000,
        refresh_concurrency: concurrency,
        consistency: Consistency::AtLeastAsFresh,
        ..Config::default()
    })
    .await?;
    ctx.pool
        .get()
        .await?
        .execute(
            "update rebac.config set value = $1 where key = 'node_grants'",
            &[&nodes],
        )
        .await?;
    closure::check_at_start(&ctx, true).await?;
    Ok(ctx)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    let principals: u64 = args.get(1).map_or(Ok(10_000), |x| x.parse())?;
    let concurrency: usize = args.get(2).map_or(Ok(4), |x| x.parse())?;
    let pg = std::env::var("REBAC_BENCH_PG")
        .unwrap_or_else(|_| "postgres://postgres:rebac@127.0.0.1:55644".into());
    let endpoint =
        std::env::var("REBAC_BENCH_SPICEDB").unwrap_or_else(|_| "http://127.0.0.1:50262".into());
    std::env::set_var("REBAC_BENCH_TOKEN", KEY);

    let spicedb = SpiceDb::connect(&endpoint, KEY, Consistency::FullyConsistent).await?;
    for ty in ["container", "group"] {
        // A store with no schema yet has nothing to delete.
        let _ = spicedb.delete_all(ty, Duration::from_secs(60)).await;
    }
    spicedb.write_schema(SCHEMA, Duration::from_secs(5)).await?;
    let all = relationships(principals);
    let started = Instant::now();
    for chunk in all.chunks(1_000) {
        spicedb.write(chunk, Duration::from_secs(30)).await?;
    }
    println!(
        "{} relationships written in {:.1} s; {principals} principals, refresh_concurrency {concurrency}",
        all.len(),
        started.elapsed().as_secs_f64()
    );

    for nodes in ["off", "on"] {
        let ctx = daemon(&endpoint, concurrency, &pg, nodes).await?;
        let (backfill_s, worked, sweep_s) = sweep(&ctx, concurrency).await?;
        let row = ctx
            .pool
            .get()
            .await?
            .query_one(
                "select count(*), pg_total_relation_size('rebac.readable') / 1000000,
                        (select count(*) from rebac.node_member) from rebac.readable",
                &[],
            )
            .await?;
        let (rows, mb, members): (i64, i64, i64) = (row.get(0), row.get(1), row.get(2));
        let per_s = f64::from(u32::try_from(worked)?) / sweep_s;
        println!(
            "node_grants={nodes:3}: backfill {backfill_s:.1} s; sweep of {worked} lookups {sweep_s:.1} s \
             ({per_s:.0} per second); rebac.readable {rows} rows, {mb} MB; memberships {members}"
        );
    }
    Ok(())
}
