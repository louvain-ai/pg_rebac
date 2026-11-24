//! The daemon against a real SpiceDB and a real Postgres (`docker compose up -d --wait` at the repo root).
//! Runs only with REBAC_IT=1, and says SKIPPED loudly otherwise: a fence test that silently passes is worse than none.

use rebacd::{
    backfill, closure,
    config::{Config, Mode},
    loops, project, relay,
    spicedb::{SpiceDb, TupleOp},
    verify, Ctx,
};
use std::time::Duration;

const SCHEMA: &str = r#"
definition user {}
definition workspace {
  relation member: user
  relation admin: user
}
definition group {
  relation member: user
}
definition container {
  relation workspace: workspace
  relation parent: container
  relation member: user | group#member | user:*
  relation editor: user
  permission view = member + editor + workspace->admin + parent->view
}
"#;

fn enabled() -> bool {
    if std::env::var("REBAC_IT").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("!!!!!!!! SKIPPED: the rebacd integration test needs REBAC_IT=1 and `docker compose up -d --wait`");
    false
}

/// The test Postgres (`docker compose`'s by default; `REBAC_IT_PG` points elsewhere).
fn pg() -> String {
    std::env::var("REBAC_IT_PG")
        .unwrap_or_else(|_| "postgres://postgres:rebac@127.0.0.1:55433".into())
}
const KEY: &str = "rebac-test-key";

async fn fresh_db(name: &str) -> String {
    let (admin, conn) =
        tokio_postgres::connect(&format!("{}/postgres", pg()), tokio_postgres::NoTls)
            .await
            .unwrap();
    tokio::spawn(conn);
    admin
        .batch_execute(&format!("drop database if exists {name} with (force)"))
        .await
        .unwrap();
    admin
        .batch_execute(&format!("create database {name}"))
        .await
        .unwrap();
    let url = format!("{}/{name}", pg());
    let (db, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let sql = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../sql/rebac.sql"))
        .unwrap();
    db.batch_execute(&format!("set client_min_messages = warning; {sql}"))
        .await
        .unwrap();
    url
}

fn config(url_env: &str, endpoint: &str, mode: Mode) -> Config {
    Config {
        database_url_env: url_env.into(),
        spicedb_endpoint: endpoint.into(),
        spicedb_token_env: "REBAC_IT_TOKEN".into(),
        mode,
        deadline_ms: 2_000,
        read_deadline_ms: 2_000,
        // The loops below are driven one by one; the leader test turns it on.
        leader_election: false,
        ..Config::default()
    }
}

fn op(touch: bool, resource: &str, relation: &str, subject: &str) -> TupleOp {
    TupleOp {
        touch,
        resource: resource.into(),
        relation: relation.into(),
        subject: subject.into(),
    }
}

async fn set_of(ctx: &Ctx, tenant: &str, p: &str) -> Vec<String> {
    let c = ctx.pool.get().await.unwrap();
    c.query(
        "select container from rebac.readable where tenant = $1 and principal = $2 order by 1",
        &[&tenant, &p],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| r.get(0))
    .collect()
}

/// Refused by rebac.assert_leader (its RB503 message), or by a connection that is gone.
fn fenced(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    m.contains("no longer holds the leader lock") || m.contains("closed")
}

async fn sql(ctx: &Ctx, q: &str) {
    ctx.pool
        .get()
        .await
        .unwrap()
        .batch_execute(q)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_end_to_end() {
    if !enabled() {
        return;
    }
    std::env::set_var("REBAC_IT_TOKEN", KEY);
    let url = fresh_db("rebacd_it").await;
    std::env::set_var("REBAC_IT_DB", &url);
    let endpoint =
        std::env::var("REBAC_IT_SPICEDB").unwrap_or_else(|_| "http://127.0.0.1:50061".into());

    // A fresh SpiceDB: the schema, and relationships written by "someone else" before pg_rebac existed.
    let s = SpiceDb::connect(&endpoint, KEY, rebacd::config::Consistency::FullyConsistent)
        .await
        .unwrap();
    // A store left by an earlier run may hold tuples the schema below drops: clear them first (best effort).
    for ty in ["container", "workspace", "group"] {
        let _ = s.delete_all(ty, Duration::from_secs(5)).await;
    }
    s.write_schema(SCHEMA, Duration::from_secs(5))
        .await
        .unwrap();
    for ty in ["container", "workspace", "group"] {
        s.delete_all(ty, Duration::from_secs(5)).await.unwrap();
    }
    s.write(
        &[
            op(true, "container:acme/space", "member", "user:acme/ana"),
            op(
                true,
                "container:acme/file",
                "parent",
                "container:acme/space",
            ),
            op(
                true,
                "container:acme/w1doc",
                "workspace",
                "workspace:acme/w1",
            ),
            op(true, "workspace:acme/w1", "admin", "user:acme/adm"),
            op(true, "container:acme/other", "editor", "user:acme/bo"),
            // The same principal id in another tenant reads that tenant's containers only.
            op(true, "container:globex/g1", "member", "user:globex/ana"),
            // A grant to a group, and one member of it.
            op(
                true,
                "container:acme/team",
                "member",
                "group:acme/eng#member",
            ),
            op(true, "group:acme/eng", "member", "user:acme/dee"),
        ],
        Duration::from_secs(5),
    )
    .await
    .unwrap();

    let ctx = Ctx::connect(config("REBAC_IT_DB", &endpoint, Mode::Live))
        .await
        .unwrap();

    // CLOSURE: derived from the live schema at start, and a stored list that disagrees refuses the next start.
    closure::check_at_start(&ctx, false).await.unwrap();
    let n: i64 = ctx
        .pool
        .get()
        .await
        .unwrap()
        .query_one("select count(*) from rebac.view_relation", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        n, 6,
        "member, editor, parent, workspace, workspace#admin, group#member"
    );
    sql(
        &ctx,
        "delete from rebac.view_relation where relation = 'editor'",
    )
    .await;
    assert!(
        closure::check_at_start(&ctx, false).await.is_err(),
        "a closure missing a view relation refuses the start"
    );
    closure::check_at_start(&ctx, true).await.unwrap();

    // BACKFILL: edges mirrored, every named principal projected through arrows and the via relation.
    let b = backfill::backfill(&ctx, "acme").await.unwrap();
    assert_eq!(b.failed, 0);
    assert!(b.edges >= 2, "parent and workspace edges mirrored: {b:?}");
    assert_eq!(set_of(&ctx, "acme", "ana").await, vec!["file", "space"]);
    assert_eq!(set_of(&ctx, "acme", "adm").await, vec!["w1doc"]);
    assert_eq!(set_of(&ctx, "acme", "bo").await, vec!["other"]);
    assert_eq!(
        set_of(&ctx, "acme", "dee").await,
        vec!["team"],
        "read through a group"
    );
    let set_grants: i64 = ctx
        .pool
        .get()
        .await
        .unwrap()
        .query_one(
            "select count(*) from rebac.set_grant where tenant = 'acme'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        set_grants, 1,
        "the grant to the group is mirrored by backfill"
    );
    backfill::backfill(&ctx, "globex").await.unwrap();
    assert_eq!(
        set_of(&ctx, "globex", "ana").await,
        vec!["g1"],
        "tenants stay apart"
    );
    let d = verify::diff(&ctx, "acme", None, verify::Against::SpiceDb, "readable")
        .await
        .unwrap();
    assert_eq!(
        (d.extra_rows, d.missing_rows, d.unavailable),
        (0, 0, 0),
        "{d:?}"
    );
    let iso = verify::isolation(&ctx).await.unwrap();
    assert_eq!(iso.extra_rows, 0);

    // A GRANT through rebac.write: relayed, then refreshed at least as fresh as its own write.
    sql(&ctx, "set rebac.tenant = 'acme'; select rebac.write('touch', 'container:acme/new', 'member', 'user:acme/ana')").await;
    assert_eq!(relay::relay_once(&ctx).await.unwrap(), 1);
    let tok: Option<String> = ctx
        .pool
        .get()
        .await
        .unwrap()
        .query_one(
            "select zedtoken from rebac.refresh where tenant = 'acme' and principal = 'ana'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(tok.is_some(), "the refresh carries the delivery's zedtoken");
    loops::refresh_pass(&ctx, 2).await.unwrap();
    assert_eq!(
        set_of(&ctx, "acme", "ana").await,
        vec!["file", "new", "space"]
    );

    // A REVOCATION is in force at COMMIT, before the relay has run, and SpiceDB follows when it does.
    sql(&ctx, "begin; set local rebac.tenant = 'acme'; select rebac.write('delete', 'container:acme/space', 'member', 'user:acme/ana'); commit").await;
    assert_eq!(
        set_of(&ctx, "acme", "ana").await,
        vec!["new"],
        "the space and its file are gone at commit"
    );
    // A refresh before delivery cannot restore it: the pending delete is subtracted.
    project::refresh_one(&ctx, "acme", "ana", None, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(
        set_of(&ctx, "acme", "ana").await,
        vec!["new"],
        "no regrowth from a pending revocation"
    );
    relay::relay_once(&ctx).await.unwrap();
    loops::refresh_pass(&ctx, 2).await.unwrap();
    assert_eq!(set_of(&ctx, "acme", "ana").await, vec!["new"]);
    let d = verify::diff(&ctx, "acme", None, verify::Against::SpiceDb, "readable")
        .await
        .unwrap();
    assert_eq!(d.extra_rows, 0, "{d:?}");

    // An arrow touch names no reader: LookupSubjects finds who can now see the file.
    sql(&ctx, "set rebac.tenant = 'acme'; select rebac.write('touch', 'container:acme/file2', 'parent', 'container:acme/other')").await;
    relay::relay_once(&ctx).await.unwrap();
    loops::refresh_pass(&ctx, 2).await.unwrap(); // the resource queue: LookupSubjects, enqueue bo
    loops::refresh_pass(&ctx, 2).await.unwrap(); // bo refreshed
    assert_eq!(set_of(&ctx, "acme", "bo").await, vec!["file2", "other"]);

    // SUBJECT SETS: a new member reads what the group was granted after delivery; removed, it is gone at commit.
    sql(&ctx, "set rebac.tenant = 'acme'; select rebac.write('touch', 'group:acme/eng', 'member', 'user:acme/eve')").await;
    relay::relay_once(&ctx).await.unwrap();
    loops::refresh_pass(&ctx, 2).await.unwrap();
    assert_eq!(set_of(&ctx, "acme", "eve").await, vec!["team"]);
    sql(&ctx, "set rebac.tenant = 'acme'; select rebac.write('delete', 'group:acme/eng', 'member', 'user:acme/dee')").await;
    assert!(
        set_of(&ctx, "acme", "dee").await.is_empty(),
        "a membership removed narrows at commit"
    );
    project::refresh_one(&ctx, "acme", "dee", None, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        set_of(&ctx, "acme", "dee").await.is_empty(),
        "and no refresh regrows it while pending"
    );
    relay::relay_once(&ctx).await.unwrap();
    loops::refresh_pass(&ctx, 2).await.unwrap();
    assert!(set_of(&ctx, "acme", "dee").await.is_empty());
    assert_eq!(set_of(&ctx, "acme", "eve").await, vec!["team"]);

    // The workspace admin revoked: narrowed through the edge at commit.
    sql(&ctx, "set rebac.tenant = 'acme'; select rebac.write('delete', 'workspace:acme/w1', 'admin', 'user:acme/adm')").await;
    assert!(set_of(&ctx, "acme", "adm").await.is_empty());
    relay::relay_once(&ctx).await.unwrap();

    // SPICEDB DOWN: a revocation is still in force at commit; the relay fails and marks nothing delivered.
    let down = Ctx::connect(config("REBAC_IT_DB", "http://127.0.0.1:1", Mode::Live))
        .await
        .unwrap();
    sql(&down, "set rebac.tenant = 'acme'; select rebac.write('delete', 'container:acme/other', 'editor', 'user:acme/bo')").await;
    assert!(
        set_of(&down, "acme", "bo").await.is_empty(),
        "narrowed with SpiceDB unreachable"
    );
    assert!(relay::relay_once(&down).await.is_err());
    let unsynced: i64 = down
        .pool
        .get()
        .await
        .unwrap()
        .query_one(
            "select count(*) from rebac.outbox where synced_at is null",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(unsynced, 1, "an outage marks nothing delivered");
    // A first read with SpiceDB down is unavailable, never empty.
    assert!(
        project::refresh_one(&down, "acme", "newcomer", None, Duration::from_millis(300))
            .await
            .is_err()
    );
    drop(down);
    relay::relay_once(&ctx).await.unwrap();

    // A QUARANTINED row: SpiceDB refuses it on its own; the rest of the batch is delivered.
    sql(
        &ctx,
        "set rebac.tenant = 'acme';
               select rebac.write('touch', 'container:acme/fine', 'member', 'user:acme/cy');
               select rebac.write('touch', 'container:acme/bad', 'nosuchrelation', 'user:acme/cy')",
    )
    .await;
    // An unknown relation is SpiceDB's FAILED_PRECONDITION: possibly a schema that lags a deploy, so while young the
    // row is DEFERRED (undelivered, no error, the rest delivered, the pass not counting it), and quarantined once settled.
    let counts = |c: deadpool_postgres::Object| async move {
        let r = c
            .query_one("select count(*) filter (where error is not null), count(*) filter (where synced_at is null) from rebac.outbox", &[])
            .await.unwrap();
        (r.get::<_, i64>(0), r.get::<_, i64>(1))
    };
    assert_eq!(
        relay::relay_once(&ctx).await.unwrap(),
        1,
        "only the delivered row counts"
    );
    assert_eq!(
        counts(ctx.pool.get().await.unwrap()).await,
        (0, 1),
        "deferred: undelivered, not quarantined"
    );
    assert_eq!(
        relay::relay_once(&ctx).await.unwrap(),
        0,
        "a pass holding only a deferred row idles"
    );
    sql(&ctx, "update rebac.outbox set created_at = now() - interval '11 minutes' where synced_at is null").await;
    relay::relay_once(&ctx).await.unwrap();
    assert_eq!(
        counts(ctx.pool.get().await.unwrap()).await,
        (1, 0),
        "quarantined once past the settle window"
    );

    // SHADOW: a shadow daemon writes only readable_shadow, and the compare is clean.
    let shadow = Ctx::connect(config("REBAC_IT_DB", &endpoint, Mode::Shadow))
        .await
        .unwrap();
    loops::refresh_pass(&ctx, 2).await.unwrap();
    let before = set_of(&ctx, "acme", "cy").await;
    for p in ["ana", "adm", "bo", "cy", "dee", "eve"] {
        project::refresh_one(&shadow, "acme", p, None, Duration::from_secs(2))
            .await
            .unwrap();
    }
    assert_eq!(
        set_of(&ctx, "acme", "cy").await,
        before,
        "shadow leaves the live projection alone"
    );
    project::refresh_one(&shadow, "globex", "ana", None, Duration::from_secs(2))
        .await
        .unwrap();
    let (tenants, all) = verify::all_tenants(&shadow, None, verify::Against::Shadow)
        .await
        .unwrap();
    assert_eq!(
        (tenants, all.extra_rows, all.missing_rows),
        (2, 0, 0),
        "{all:?}"
    );
    let d = verify::diff(&shadow, "acme", None, verify::Against::Shadow, "readable")
        .await
        .unwrap();
    assert_eq!((d.extra_rows, d.missing_rows), (0, 0), "{d:?}");

    // HEARTBEAT: beaten while SpiceDB answers; reads are fresh, and back-dated past 5 s they fail closed. With Watch
    // on, a beat also needs the stream: a daemon whose Watch is not running does not beat.
    {
        let (stop_tx, stop) = tokio::sync::watch::channel(false);
        sql(&ctx, "delete from rebac.watch_state where scope = '*'").await;
        let hb = tokio::spawn(loops::heartbeat_loop(
            std::sync::Arc::new(
                Ctx::connect(config("REBAC_IT_DB", &endpoint, Mode::Live))
                    .await
                    .unwrap(),
            ),
            stop,
        ));
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        stop_tx.send(true).unwrap();
        hb.await.unwrap();
        let beats: i64 = ctx
            .pool
            .get()
            .await
            .unwrap()
            .query_one(
                "select count(*) from rebac.watch_state where scope = '*'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(beats, 0, "no beat while Watch is not applying");
    }
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let hb = tokio::spawn(loops::heartbeat_loop(
        std::sync::Arc::new(
            Ctx::connect(Config {
                watch: false,
                ..config("REBAC_IT_DB", &endpoint, Mode::Live)
            })
            .await
            .unwrap(),
        ),
        stop.clone(),
    ));
    let mut fresh = false;
    for _ in 0..50 {
        let n: i64 = ctx.pool.get().await.unwrap()
            .query_one("select count(*) from rebac.watch_state where scope = '*' and heartbeat_at > now() - interval '3 seconds'", &[])
            .await.unwrap().get(0);
        if n == 1 {
            fresh = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(fresh, "the daemon beats the heartbeat");
    stop_tx.send(true).unwrap();
    hb.await.unwrap();
    {
        let c = ctx.pool.get().await.unwrap();
        c.batch_execute("set rebac.tenant = 'acme'").await.unwrap();
        assert!(c
            .query("select * from rebac.readable_set('ana')", &[])
            .await
            .is_ok());
        c.batch_execute("update rebac.watch_state set heartbeat_at = now() - interval '6 seconds'")
            .await
            .unwrap();
        let err = c
            .query("select * from rebac.readable_set('ana')", &[])
            .await
            .unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("RB503"));
    }

    // WATCH, APPLIED: a revocation written straight to SpiceDB (not through rebac.write) is in force within seconds,
    // with nothing but the Watch loop running: no relay, no refresh, no sweep.
    sql(&ctx, "select rebac.heartbeat('*')").await;
    s.write(
        &[op(
            true,
            "container:acme/ext-doc",
            "member",
            "user:acme/ext",
        )],
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    project::refresh_one(&ctx, "acme", "ext", None, ctx.config.deadline())
        .await
        .unwrap();
    assert_eq!(set_of(&ctx, "acme", "ext").await, vec!["ext-doc"]);
    let wctx = std::sync::Arc::new(
        Ctx::connect(config("REBAC_IT_DB", &endpoint, Mode::Live))
            .await
            .unwrap(),
    );
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let w = tokio::spawn(loops::watch_loop(wctx.clone(), stop));
    // Up to 15 s: the Postgres datastore reports its head through an idle checkpoint about every 5 s.
    for _ in 0..300 {
        if wctx.watch_current.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        wctx.watch_current.load(std::sync::atomic::Ordering::SeqCst),
        "the Watch stream is applying"
    );
    let revoked_at = std::time::Instant::now();
    s.write(
        &[op(
            false,
            "container:acme/ext-doc",
            "member",
            "user:acme/ext",
        )],
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    let mut gone = None;
    for _ in 0..100 {
        if set_of(&ctx, "acme", "ext").await.is_empty() {
            gone = Some(revoked_at.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let gone = gone.expect("an external revocation is narrowed from Watch within 2 s");
    eprintln!("external revocation narrowed after {gone:?}");
    {
        let c = ctx.pool.get().await.unwrap();
        c.batch_execute("set rebac.tenant = 'acme'").await.unwrap();
        assert!(
            c.query("select * from rebac.readable_set('ext')", &[])
                .await
                .unwrap()
                .is_empty(),
            "the reader has lost it"
        );
        let n: i64 = c
            .query_one(
                "select count(*) from rebac.outbox where origin = 'watch' and op = 'delete'
                   and resource = 'container:acme/ext-doc'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 1, "recorded once, as applied from Watch");
    }

    // A RELAYED write is ours: its echo on Watch is skipped, not applied twice.
    let before: i64 = ctx
        .pool
        .get()
        .await
        .unwrap()
        .query_one(
            "select count(*) from rebac.outbox where origin = 'watch'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    sql(
        &ctx,
        "set rebac.tenant = 'acme'; select rebac.write('touch', 'container:acme/ours', 'member', 'user:acme/ext')",
    )
    .await;
    relay::relay_once(&ctx).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after: i64 = ctx
        .pool
        .get()
        .await
        .unwrap()
        .query_one(
            "select count(*) from rebac.outbox where origin = 'watch'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        before, after,
        "the relay's own write is not taken for an external one"
    );

    // A DISCONNECT: the stream stops (the daemon restarts), a revocation is made meanwhile, and the next stream resumes
    // from the applied position and narrows it.
    stop_tx.send(true).unwrap();
    w.await.unwrap();
    assert!(!wctx.watch_current.load(std::sync::atomic::Ordering::SeqCst));
    // Several revocations, each its own revision, so the resumed stream replays them as several messages.
    let backlog: Vec<String> = (2..8).map(|i| format!("ext-{i}")).collect();
    for d in &backlog {
        s.write(
            &[op(
                true,
                &format!("container:acme/{d}"),
                "member",
                "user:acme/ext",
            )],
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    }
    project::refresh_one(&ctx, "acme", "ext", None, ctx.config.deadline())
        .await
        .unwrap();
    assert!(set_of(&ctx, "acme", "ext")
        .await
        .contains(&"ext-2".to_string()));
    for d in &backlog {
        s.write(
            &[op(
                false,
                &format!("container:acme/{d}"),
                "member",
                "user:acme/ext",
            )],
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    }
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let w = tokio::spawn(loops::watch_loop(wctx.clone(), stop));
    // THE REPLAY IS NOT CURRENT: the stream is current (and the heartbeat may beat) only once SpiceDB's checkpoint says
    // every change through it was delivered, so at the first instant it is current, the whole backlog is narrowed.
    let mut current = false;
    for _ in 0..3000 {
        if wctx.watch_current.load(std::sync::atomic::Ordering::SeqCst) {
            current = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(current, "the resumed stream catches up");
    let left: Vec<String> = set_of(&ctx, "acme", "ext")
        .await
        .into_iter()
        .filter(|c| backlog.contains(c))
        .collect();
    assert!(
        left.is_empty(),
        "every revocation made while Watch was down is applied before the stream counts as current: {left:?}"
    );

    // AN IDLE STORE: the stream stops and reopens with nothing changed meanwhile. The stored position is at the head,
    // so the stream is current at once, with no change to wait for (an idle in-memory store sends nothing at all).
    stop_tx.send(true).unwrap();
    w.await.unwrap();
    assert!(!wctx.watch_current.load(std::sync::atomic::Ordering::SeqCst));
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let w = tokio::spawn(loops::watch_loop(wctx.clone(), stop));
    let reopened = std::time::Instant::now();
    let mut idle_current = false;
    for _ in 0..500 {
        if wctx.watch_current.load(std::sync::atomic::Ordering::SeqCst) {
            idle_current = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        idle_current,
        "a stream reopened on an idle store is current at once: its stored position is the head"
    );
    eprintln!("idle reopen current after {:?}", reopened.elapsed());
    // Current because the stored position reached the head, not because time passed: they are the same revision.
    {
        let c = ctx.pool.get().await.unwrap();
        let pos: String = c
            .query_one(
                "select zedtoken from rebac.watch_state where scope = 'watch_applied'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        let head = s
            .head_revision(
                "container:__rebac_head_probe",
                "view",
                "user:__rebac_head_probe",
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        let (pos, head) = (
            rebacd::revision::of_token(&pos).unwrap(),
            rebacd::revision::of_token(&head).unwrap(),
        );
        assert_eq!(
            rebacd::revision::covers(&pos, &head),
            Some(true),
            "idle: position {pos:?} is the head {head:?}"
        );
    }

    // A POSITION SPICEDB REFUSES restarts from head with a gap: sets looked up before fail closed, the sweep is forced.
    stop_tx.send(true).unwrap();
    w.await.unwrap();
    sql(
        &ctx,
        "update rebac.watch_state set zedtoken = 'not-a-zedtoken' where scope = 'watch_applied';
         insert into rebac.sweep_claim (id, last_at) values (true, now()) on conflict (id) do update set last_at = now()",
    )
    .await;
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let w = tokio::spawn(loops::watch_loop(wctx.clone(), stop));
    let mut gapped = false;
    for _ in 0..300 {
        let g: bool = ctx.pool.get().await.unwrap()
            .query_one("select coalesce((select gap_at > now() - interval '10 seconds' and zedtoken <> 'not-a-zedtoken'
                        from rebac.watch_state where scope = 'watch_applied'), false)", &[])
            .await.unwrap().get(0);
        if g && wctx.watch_current.load(std::sync::atomic::Ordering::SeqCst) {
            gapped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop_tx.send(true).unwrap();
    w.await.unwrap();
    assert!(
        gapped,
        "a refused position restarts from head and records the gap"
    );
    {
        let c = ctx.pool.get().await.unwrap();
        c.batch_execute("set rebac.tenant = 'acme'").await.unwrap();
        let err = c
            .query("select * from rebac.readable_set('ext')", &[])
            .await
            .unwrap_err();
        assert_eq!(
            err.code().map(|c| c.code()),
            Some("RB503"),
            "looked up before the gap"
        );
        let forced: bool = c
            .query_one(
                "select last_at < now() - interval '1 day' from rebac.sweep_claim",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(forced, "the sweep is forced");
    }
    // Everyone the test reads below is re-projected after the gap (what the forced sweep does).
    sql(&ctx, "select rebac.sweep(0, 0, false)").await;
    while loops::refresh_pass(&ctx, 4).await.unwrap() > 0 {}

    // FRESHNESS: every set the daemon projected carries when its lookup began, and Watch reports its own position.
    let c = ctx.pool.get().await.unwrap();
    let unknown: i64 = c
        .query_one(
            "select count(*) from rebac.principal_state where state = 'projected' and looked_up_at is null",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(unknown, 0, "the daemon stamps every lookup");
    let watched: bool = c
        .query_one(
            "select watched_at is not null from rebac.watch_state where scope = 'watch_applied'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(watched, "Watch records when it last heard from SpiceDB");
    drop(c);

    // WILDCARDS: a grant to user:* reaches every projected principal; its revocation is in force at commit for all.
    sql(
        &ctx,
        "set rebac.tenant = 'acme'; select rebac.write('touch', 'container:acme/public', 'member', 'user:*')",
    )
    .await;
    relay::relay_once(&ctx).await.unwrap();
    while loops::refresh_pass(&ctx, 4).await.unwrap() > 0 {}
    assert!(
        set_of(&ctx, "acme", "adm")
            .await
            .contains(&"public".to_string()),
        "a wildcard grant reaches everyone"
    );
    assert!(set_of(&ctx, "acme", "dee")
        .await
        .contains(&"public".to_string()));
    assert!(
        !set_of(&ctx, "globex", "ana")
            .await
            .contains(&"public".to_string()),
        "within its tenant only"
    );
    sql(
        &ctx,
        "set rebac.tenant = 'acme'; select rebac.write('delete', 'container:acme/public', 'member', 'user:*')",
    )
    .await;
    assert!(
        !set_of(&ctx, "acme", "adm")
            .await
            .contains(&"public".to_string()),
        "removed at commit"
    );
    assert!(!set_of(&ctx, "acme", "dee")
        .await
        .contains(&"public".to_string()));
    relay::relay_once(&ctx).await.unwrap();
    while loops::refresh_pass(&ctx, 4).await.unwrap() > 0 {}
    assert!(
        !set_of(&ctx, "acme", "adm")
            .await
            .contains(&"public".to_string()),
        "and stays removed"
    );

    // LIVE SCHEMA CHANGES. A new relation that grants view: the closure is re-derived, every tenant rebuilt, and the
    // grant it carries is projected.
    let with_viewer = SCHEMA.replace(
        "relation editor: user\n  permission view = member + editor",
        "relation editor: user\n  relation viewer: user\n  permission view = member + editor + viewer",
    );
    assert_ne!(with_viewer, SCHEMA);
    s.write_schema(&with_viewer, Duration::from_secs(5))
        .await
        .unwrap();
    s.write(
        &[op(true, "container:acme/vdoc", "viewer", "user:acme/ana")],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(
        closure::recheck(&ctx).await.unwrap(),
        closure::Recheck::Rebuilt
    );
    let c = ctx.pool.get().await.unwrap();
    let row = c
        .query_one(
            "select state, (select count(*) from rebac.view_relation where relation = 'viewer') from rebac.closure_state",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "valid", "rebuilt, valid again");
    assert_eq!(
        row.get::<_, i64>(1),
        1,
        "the new relation is in the closure"
    );
    assert!(
        set_of(&ctx, "acme", "ana")
            .await
            .contains(&"vdoc".to_string()),
        "the rebuild projected the new relation's grant"
    );
    assert_eq!(
        closure::recheck(&ctx).await.unwrap(),
        closure::Recheck::Unchanged
    );

    // AN INTERSECTION is projected, and a revocation of either branch is in force at commit: `& cleared` on top of the
    // union. The rebuild projects the intersection (ana is not cleared, so vdoc goes); a grant of `cleared` brings it
    // back; revoking `cleared` through rebac.write narrows at commit, before any relay, and no refresh restores it.
    let with_and = with_viewer
        .replace(
            "relation viewer: user\n",
            "relation viewer: user\n  relation cleared: user\n",
        )
        .replace(
            "permission view = member + editor + viewer + workspace->admin + parent->view",
            "permission view = (member + editor + viewer + workspace->admin + parent->view) & cleared",
        );
    assert!(with_and.contains("& cleared"));
    s.write_schema(&with_and, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        closure::recheck(&ctx).await.unwrap(),
        closure::Recheck::Rebuilt
    );
    assert!(
        !set_of(&ctx, "acme", "ana")
            .await
            .contains(&"vdoc".to_string()),
        "an intersection: viewer alone no longer grants view"
    );
    s.write(
        &[op(true, "container:acme/vdoc", "cleared", "user:acme/ana")],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    project::refresh_one(&ctx, "acme", "ana", None, ctx.config.deadline())
        .await
        .unwrap();
    assert!(
        set_of(&ctx, "acme", "ana")
            .await
            .contains(&"vdoc".to_string()),
        "both branches hold: projected"
    );
    sql(&ctx, "begin; set local rebac.tenant = 'acme'; select rebac.write('delete', 'container:acme/vdoc', 'cleared', 'user:acme/ana'); commit").await;
    assert!(
        !set_of(&ctx, "acme", "ana")
            .await
            .contains(&"vdoc".to_string()),
        "revoking one branch of an intersection is in force at commit"
    );
    relay::relay_once(&ctx).await.unwrap();
    while loops::refresh_pass(&ctx, 4).await.unwrap() > 0 {}
    assert!(
        !set_of(&ctx, "acme", "ana")
            .await
            .contains(&"vdoc".to_string()),
        "and no refresh restores it"
    );
    s.write_schema(&with_viewer, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        closure::recheck(&ctx).await.unwrap(),
        closure::Recheck::Rebuilt
    );

    // An unsupported schema (exclusion: removing a tuple can grant) fails every read closed until it is fixed.
    let excluding = with_viewer
        .replace(
            "relation viewer: user\n",
            "relation viewer: user\n  relation banned: user\n",
        )
        .replace("+ parent->view", "+ parent->view - banned");
    s.write_schema(&excluding, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        closure::recheck(&ctx).await.unwrap(),
        closure::Recheck::Unsupported
    );
    c.batch_execute("select rebac.heartbeat('*')")
        .await
        .unwrap();
    let err = c
        .batch_execute("set rebac.tenant = 'acme'; select * from rebac.readable_set('ana')")
        .await
        .unwrap_err();
    assert_eq!(
        err.code().map(|c| c.code()),
        Some("RB503"),
        "an unsupported closure is RB503"
    );

    // The closure loop notices on its own (no Watch here), and rebuilds once the schema is supported again.
    let loop_ctx = std::sync::Arc::new(
        Ctx::connect(Config {
            closure_check_ms: 200,
            ..config("REBAC_IT_DB", &endpoint, Mode::Live)
        })
        .await
        .unwrap(),
    );
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let cl = tokio::spawn(loops::closure_loop(loop_ctx.clone(), stop));
    // SpiceDB refuses to drop a relation that still has tuples.
    s.write(
        &[op(false, "container:acme/vdoc", "viewer", "user:acme/ana")],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    s.write_schema(SCHEMA, Duration::from_secs(5))
        .await
        .unwrap();
    let mut valid = false;
    for _ in 0..200 {
        let st: String = c
            .query_one("select state from rebac.closure_state", &[])
            .await
            .unwrap()
            .get(0);
        let viewer: i64 = c
            .query_one(
                "select count(*) from rebac.view_relation where relation = 'viewer'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if st == "valid" && viewer == 0 {
            valid = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop_tx.send(true).unwrap();
    cl.await.unwrap();
    assert!(
        valid,
        "the closure loop re-derived and rebuilt after the schema was fixed"
    );
    assert!(
        !set_of(&ctx, "acme", "ana")
            .await
            .contains(&"vdoc".to_string()),
        "the relation that left the closure no longer grants"
    );

    // SHADOW MODE (how a shadow deployment runs): a closure change rebuilds the shadow projection, and never touches
    // the live closure state, the live projection or the mirrors.
    let shadow = Ctx::connect(config("REBAC_IT_DB", &endpoint, Mode::Shadow))
        .await
        .unwrap();
    let snapshot = "select (select state || '@' || changed_at::text from rebac.closure_state),
                (select count(*) from rebac.readable), (select count(*) from rebac.edge),
                (select count(*) from rebac.set_grant)";
    let before = c.query_one(snapshot, &[]).await.unwrap();
    let before: (String, i64, i64, i64) =
        (before.get(0), before.get(1), before.get(2), before.get(3));
    s.write_schema(&with_viewer, Duration::from_secs(5))
        .await
        .unwrap();
    s.write(
        &[op(true, "container:acme/vdoc", "viewer", "user:acme/ana")],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(
        closure::recheck(&shadow).await.unwrap(),
        closure::Recheck::Rebuilt
    );
    let after = c.query_one(snapshot, &[]).await.unwrap();
    let after: (String, i64, i64, i64) = (after.get(0), after.get(1), after.get(2), after.get(3));
    assert_eq!(
        before, after,
        "shadow mode leaves the live closure state, projection and mirrors alone"
    );
    let shadow_ana: Vec<String> = c
        .query(
            "select container from rebac.readable_shadow where tenant = 'acme' and principal = 'ana'",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        shadow_ana.contains(&"vdoc".to_string()),
        "shadow mode rebuilt the shadow under the new closure"
    );
    // Put the store and the live closure back.
    s.write(
        &[op(false, "container:acme/vdoc", "viewer", "user:acme/ana")],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    s.write_schema(SCHEMA, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        closure::recheck(&ctx).await.unwrap(),
        closure::Recheck::Rebuilt
    );

    // ONE LEADER. Two daemons on one database: exactly one holds the lock; the other's Watch writes are refused by the
    // fence. The leader's lock connection is killed: before it even notices, its own Watch writes are refused (a
    // deposed leader cannot commit one more), and the other takes over.
    {
        let lc = |_: ()| Config {
            leader_election: true,
            leader_retry_ms: 100,
            heartbeat_ms: 100,
            ..config("REBAC_IT_DB", &endpoint, Mode::Live)
        };
        let a = std::sync::Arc::new(Ctx::connect(lc(())).await.unwrap());
        let b = std::sync::Arc::new(Ctx::connect(lc(())).await.unwrap());
        let (stop_tx, stop) = tokio::sync::watch::channel(false);
        let la = tokio::spawn(rebacd::leader::leader_loop(a.clone(), stop.clone()));
        let lb = tokio::spawn(rebacd::leader::leader_loop(b.clone(), stop.clone()));
        let leaders = || {
            [&a, &b]
                .iter()
                .filter(|c| rebacd::leader::is_leader(c))
                .count()
        };
        for _ in 0..100 {
            if leaders() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(leaders(), 1, "exactly one of two daemons leads");
        let (lead, follow) = if rebacd::leader::is_leader(&a) {
            (a.clone(), b.clone())
        } else {
            (b.clone(), a.clone())
        };
        rebacd::leader::fenced_execute(&lead, "select rebac.watch_alive()", &[])
            .await
            .expect("the leader's Watch write passes the fence");
        // The follower, forced to try (as a daemon that wrongly believed it led would): refused.
        follow.leader_pid.store(
            lead.leader_pid.load(std::sync::atomic::Ordering::SeqCst) + 100_000,
            std::sync::atomic::Ordering::SeqCst,
        );
        let e = rebacd::leader::fenced_execute(&follow, "select rebac.watch_alive()", &[])
            .await
            .unwrap_err();
        assert!(fenced(&e), "a pid that holds no lock is fenced off: {e:#}");
        follow
            .leader_pid
            .store(0, std::sync::atomic::Ordering::SeqCst);
        // Kill the leader's lock connection. Its pid is still set (it has not noticed): the fence refuses it anyway.
        let pid = lead.leader_pid.load(std::sync::atomic::Ordering::SeqCst);
        sql(&ctx, &format!("select pg_terminate_backend({pid})")).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        if lead.leader_pid.load(std::sync::atomic::Ordering::SeqCst) == pid {
            let e = rebacd::leader::fenced_execute(&lead, "select rebac.watch_alive()", &[])
                .await
                .unwrap_err();
            assert!(fenced(&e), "a deposed leader cannot commit: {e:#}");
        }
        let mut took_over = false;
        for _ in 0..200 {
            if rebacd::leader::is_leader(&follow) {
                took_over = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(took_over, "the other daemon takes over");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(leaders(), 1, "and only it leads");
        rebacd::leader::fenced_execute(&follow, "select rebac.watch_alive()", &[])
            .await
            .expect("the new leader's write passes");
        stop_tx.send(true).unwrap();
        la.await.unwrap();
        lb.await.unwrap();
        // A shadow daemon takes its own lock, so it never displaces the live leader.
        assert_eq!(
            rebacd::leader::lock_key(
                &Ctx::connect(config("REBAC_IT_DB", &endpoint, Mode::Shadow))
                    .await
                    .unwrap()
            ),
            2
        );
    }

    // THE LOOKUP ENDPOINT behind a bearer token: no token or a wrong one is 401 and asks SpiceDB nothing; the right one
    // projects. /healthz stays open.
    {
        use tower::ServiceExt;
        let ctx = std::sync::Arc::new(ctx);
        let app = rebacd::http::router(ctx.clone(), Some("lookup-secret".into()));
        let req = |auth: Option<&str>, path: &str| {
            let mut b = axum::http::Request::builder()
                .method(if path == "/lookup" { "POST" } else { "GET" })
                .uri(path)
                .header("content-type", "application/json");
            if let Some(a) = auth {
                b = b.header("authorization", a);
            }
            b.body(axum::body::Body::from(
                r#"{"tenant":"acme","principal":"tok-probe"}"#,
            ))
            .unwrap()
        };
        for (auth, path) in [
            (None, "/lookup"),
            (Some("Bearer wrong"), "/lookup"),
            (Some("lookup-secret"), "/lookup"),
            (None, "/status"),
        ] {
            let r = app.clone().oneshot(req(auth, path)).await.unwrap();
            assert_eq!(r.status(), 401, "{path} with {auth:?} is refused");
        }
        let n: i64 = ctx
            .pool
            .get()
            .await
            .unwrap()
            .query_one(
                "select count(*) from rebac.principal_state where principal = 'tok-probe'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 0, "a refused lookup projects nothing");
        let r = app
            .clone()
            .oneshot(req(Some("Bearer lookup-secret"), "/lookup"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "the right token is served");
        let r = app.clone().oneshot(req(None, "/healthz")).await.unwrap();
        assert_eq!(r.status(), 200, "/healthz needs no token");
        let r = app.clone().oneshot(req(None, "/metrics")).await.unwrap();
        assert_eq!(r.status(), 401, "/metrics is behind the token");
        let r = app
            .clone()
            .oneshot(req(Some("Bearer lookup-secret"), "/metrics"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("\nrebac_up 1\n"), "metrics: {text}");
        assert!(text.contains("rebac_pending_outbox "), "metrics: {text}");
        assert!(!text.contains("acme"), "no tenant id is ever a label");
    }
}
