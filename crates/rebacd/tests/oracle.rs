//! SpiceDB as the oracle. Each round draws a schema from the shapes the closure accepts and a set of relationships,
//! then compares every principal's readable set with SpiceDB's own `LookupResources`:
//!
//! - after a backfill and the queues it leaves have drained: equal;
//! - at the commit of a revocation written through `rebac.write`, before the relay runs: never wider than SpiceDB
//!   after it;
//! - after changes written straight to SpiceDB and applied as the Watch loop applies them: never wider;
//! - once every queue has drained again: equal.
//!
//! Grant nodes are on (the default), so grants to sets and wildcards are read through their nodes. Runs only with
//! `REBAC_IT=1` (`docker compose up -d --wait`), like the integration test:
//!
//! ```text
//! REBAC_IT=1 cargo test -p rebacd --test oracle     # REBAC_ORACLE_ROUNDS (default 8), REBAC_ORACLE_SEED
//! ```

use rebacd::{
    backfill, closure,
    config::{Config, Consistency, Mode},
    loops, relay,
    spicedb::{SpiceDb, TupleOp},
    Ctx,
};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::Duration;

const KEY: &str = "rebac-test-key";
const T: &str = "ora";
const USERS: usize = 14;
const GROUPS: usize = 4;
const WORKSPACES: usize = 2;
const CONTAINERS: usize = 30;

type Reads = BTreeMap<String, Option<BTreeSet<String>>>;
type Truth = BTreeMap<String, BTreeSet<String>>;

fn enabled() -> bool {
    if std::env::var("REBAC_IT").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("!!!!!!!! SKIPPED: the SpiceDB oracle test needs REBAC_IT=1 and `docker compose up -d --wait`");
    false
}

fn pg() -> String {
    std::env::var("REBAC_IT_PG")
        .unwrap_or_else(|_| "postgres://postgres:rebac@127.0.0.1:55433".into())
}

/// xorshift64*, so a failing round is reproduced from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).expect("below a usize")
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feature {
    /// `container#member` accepts `group#member`.
    Sets,
    /// `container#member` accepts `user:*`.
    Wildcard,
    /// `parent->view`, a self-arrow.
    Parent,
    /// `workspace->admin`, an arrow to another type.
    Workspace,
    /// `workspace#admin` accepts `group#member`.
    ViaSets,
}

#[derive(Debug)]
struct Shape(Vec<Feature>);

impl Shape {
    fn draw(r: &mut Rng) -> Self {
        let odds = [
            (Feature::Sets, 75),
            (Feature::Wildcard, 50),
            (Feature::Parent, 70),
            (Feature::Workspace, 60),
            (Feature::ViaSets, 40),
        ];
        Shape(
            odds.into_iter()
                .filter(|&(_, p)| r.chance(p))
                .map(|(f, _)| f)
                .collect(),
        )
    }

    fn has(&self, f: Feature) -> bool {
        self.0.contains(&f)
    }

    fn schema(&self) -> String {
        let mut member = vec!["user"];
        if self.has(Feature::Sets) {
            member.push("group#member");
        }
        if self.has(Feature::Wildcard) {
            member.push("user:*");
        }
        let admin = if self.has(Feature::ViaSets) {
            "user | group#member"
        } else {
            "user"
        };
        let mut view = vec!["member", "editor"];
        if self.has(Feature::Workspace) {
            view.push("workspace->admin");
        }
        if self.has(Feature::Parent) {
            view.push("parent->view");
        }
        format!(
            "definition user {{}}\n\
             definition group {{\n  relation member: user\n}}\n\
             definition workspace {{\n  relation admin: {admin}\n}}\n\
             definition container {{\n  relation workspace: workspace\n  relation parent: container\n  \
             relation member: {}\n  relation editor: user\n  permission view = {}\n}}\n",
            member.join(" | "),
            view.join(" + ")
        )
    }

    /// One relationship this shape allows.
    fn tuple(&self, r: &mut Rng) -> TupleOp {
        let c = r.below(CONTAINERS);
        let container = format!("container:{T}/c{c}");
        let (resource, relation, subject) = match r.below(8) {
            0 | 1 => (
                container,
                "member",
                Self::grantee(r, self.has(Feature::Sets), self.has(Feature::Wildcard)),
            ),
            3 if self.has(Feature::Parent) && c > 0 => (
                container,
                "parent",
                format!("container:{T}/c{}", r.below(c)),
            ),
            4 if self.has(Feature::Workspace) => (
                container,
                "workspace",
                format!("workspace:{T}/w{}", r.below(WORKSPACES)),
            ),
            5 if self.has(Feature::Workspace) => (
                format!("workspace:{T}/w{}", r.below(WORKSPACES)),
                "admin",
                Self::grantee(r, self.has(Feature::ViaSets), false),
            ),
            6 | 7 if self.has(Feature::Sets) || self.has(Feature::ViaSets) => (
                format!("group:{T}/g{}", r.below(GROUPS)),
                "member",
                user(r.below(USERS)),
            ),
            2 => (container, "editor", user(r.below(USERS))),
            _ => (container, "member", user(r.below(USERS))),
        };
        TupleOp {
            touch: true,
            resource,
            relation: relation.into(),
            subject,
        }
    }

    fn grantee(r: &mut Rng, sets: bool, wildcard: bool) -> String {
        if wildcard && r.chance(8) {
            "user:*".into()
        } else if sets && r.chance(35) {
            format!("group:{T}/g{}#member", r.below(GROUPS))
        } else {
            user(r.below(USERS))
        }
    }
}

fn user(i: usize) -> String {
    format!("user:{T}/u{i}")
}

fn key(o: &TupleOp) -> (String, String, String) {
    (o.resource.clone(), o.relation.clone(), o.subject.clone())
}

/// Retries a call that a dropped connection to SpiceDB (Docker's port proxy, SpiceDB's connection age) can fail.
async fn retried<T, E: std::fmt::Debug, F: Future<Output = Result<T, E>>>(
    mut call: impl FnMut() -> F,
) -> T {
    let mut last = None;
    for _ in 0..10 {
        match call().await {
            Ok(v) => return v,
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("still failing after 10 tries: {last:?}");
}

/// What SpiceDB says each user may view.
async fn truth(s: &SpiceDb) -> Truth {
    let mut out = Truth::new();
    for i in 0..USERS {
        let subject = user(i);
        let ids = retried(|| {
            s.lookup_resources("container", "view", &subject, None, Duration::from_secs(10))
        })
        .await;
        let prefix = format!("{T}/");
        out.insert(
            format!("u{i}"),
            ids.iter()
                .filter_map(|x| x.strip_prefix(&prefix))
                .map(String::from)
                .collect(),
        );
    }
    out
}

/// What each principal reads through the fence now (`None`: not projected, or failed closed).
async fn reads(ctx: &Ctx) -> Reads {
    let mut c = ctx.pool.get().await.unwrap();
    let mut out = Reads::new();
    for i in 0..USERS {
        let principal = format!("u{i}");
        let tx = c.transaction().await.unwrap();
        tx.batch_execute(&format!("set local rebac.tenant = '{T}'; select t_beat()"))
            .await
            .unwrap();
        let rows = tx
            .query(
                "select distinct c from rebac.readable_set($1) c",
                &[&principal],
            )
            .await;
        out.insert(
            principal,
            rows.ok()
                .map(|rows| rows.iter().map(|r| r.get(0)).collect()),
        );
        tx.rollback().await.unwrap();
    }
    out
}

/// Relays and refreshes until nothing is queued. A failed refresh backs off on a clock this test does not wait for,
/// so whatever is still queued is made due now.
async fn drain(ctx: &Ctx) {
    for _ in 0..200 {
        let (Ok(relayed), Ok(refreshed)) = (
            relay::relay_once(ctx).await,
            loops::refresh_pass(ctx, 4).await,
        ) else {
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        };
        let queued: i64 = ctx
            .pool
            .get()
            .await
            .unwrap()
            .query_one(
                "with due as (update rebac.refresh set due_at = now() returning 1),
                      due_r as (update rebac.refresh_resource set due_at = now() returning 1)
                 select (select count(*) from due) + (select count(*) from due_r)
                        + (select count(*) from rebac.outbox where synced_at is null)",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if relayed == 0 && refreshed == 0 && queued == 0 {
            return;
        }
    }
    panic!("the queues did not drain");
}

fn assert_within(have: &Reads, want: &Truth, at: &str) {
    for (p, h) in have {
        if let Some(h) = h {
            let extra: Vec<_> = h.difference(&want[p]).collect();
            assert!(
                extra.is_empty(),
                "{at}: {p} reads {extra:?}, which SpiceDB does not grant"
            );
        }
    }
}

fn assert_equal(have: &Reads, want: &Truth, at: &str) {
    for (p, w) in want {
        match &have[p] {
            Some(h) => assert_eq!(h, w, "{at}: {p}"),
            None => assert!(
                w.is_empty(),
                "{at}: {p} is not readable but SpiceDB grants {w:?}"
            ),
        }
    }
}

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
    db.batch_execute(&format!(
        "set client_min_messages = warning; {sql}
         create function t_beat() returns void language sql as $$
           insert into rebac.watch_state (scope, heartbeat_at) values ('*', clock_timestamp())
           on conflict (scope) do update set heartbeat_at = clock_timestamp() $$;"
    ))
    .await
    .unwrap();
    url
}

async fn daemon(endpoint: &str) -> Ctx {
    std::env::set_var("REBAC_ORACLE_DB", fresh_db("rebacd_oracle").await);
    Ctx::connect(Config {
        database_url_env: "REBAC_ORACLE_DB".into(),
        spicedb_endpoint: endpoint.into(),
        spicedb_token_env: "REBAC_IT_TOKEN".into(),
        mode: Mode::Live,
        deadline_ms: 5_000,
        read_deadline_ms: 5_000,
        consistency: Consistency::FullyConsistent,
        ..Config::default()
    })
    .await
    .unwrap()
}

/// One round: a schema, relationships, a backfill, then revocations and grants three ways.
struct Round<'a> {
    name: String,
    rng: Rng,
    shape: Shape,
    spicedb: &'a SpiceDb,
    ctx: Ctx,
    live: BTreeMap<(String, String, String), TupleOp>,
}

impl Round<'_> {
    async fn sql(&self, q: &str) {
        self.ctx
            .pool
            .get()
            .await
            .unwrap()
            .batch_execute(q)
            .await
            .unwrap();
    }

    async fn backfill(&mut self) {
        retried(|| backfill::backfill(&self.ctx, T)).await;
        // Every user is projected, so one nobody names reads nothing, a known answer.
        for i in 0..USERS {
            self.sql(&format!(
                "select rebac.enqueue_refresh('{T}', 'u{i}', 0::smallint, null)"
            ))
            .await;
        }
        drain(&self.ctx).await;
        assert_equal(
            &reads(&self.ctx).await,
            &truth(self.spicedb).await,
            &format!("{}, after backfill", self.name),
        );
    }

    /// Revocations through rebac.write, each checked at its commit; a grant now and then.
    async fn through_write(&mut self) {
        for step in 0..12 {
            let keys: Vec<_> = self.live.keys().cloned().collect();
            let victim = self.live.remove(&keys[self.rng.below(keys.len())]).unwrap();
            self.sql(&format!(
                "begin; set local rebac.tenant = '{T}'; select rebac.write('delete', '{}', '{}', '{}'); commit",
                victim.resource, victim.relation, victim.subject
            ))
            .await;
            let at_commit = reads(&self.ctx).await;
            retried(|| relay::relay_once(&self.ctx)).await;
            let at = format!("{}, revocation {step} ({victim:?}) at commit", self.name);
            assert_within(&at_commit, &truth(self.spicedb).await, &at);
            if self.rng.chance(60) {
                let o = self.shape.tuple(&mut self.rng);
                self.sql(&format!(
                    "begin; set local rebac.tenant = '{T}'; select rebac.write('touch', '{}', '{}', '{}'); commit",
                    o.resource, o.relation, o.subject
                ))
                .await;
                self.live.insert(key(&o), o);
            }
        }
        drain(&self.ctx).await;
        let at = format!("{}, after rebac.write and the queues", self.name);
        assert_equal(&reads(&self.ctx).await, &truth(self.spicedb).await, &at);
    }

    /// Changes written straight to SpiceDB, applied as the Watch loop applies them.
    async fn through_watch(&mut self) {
        for step in 0..8 {
            let mut batch: Vec<TupleOp> = vec![];
            for _ in 0..3 {
                let keys: Vec<_> = self.live.keys().cloned().collect();
                if self.rng.chance(60) && !keys.is_empty() {
                    let mut o = self.live.remove(&keys[self.rng.below(keys.len())]).unwrap();
                    o.touch = false;
                    batch.push(o);
                } else {
                    let o = self.shape.tuple(&mut self.rng);
                    self.live.insert(key(&o), o.clone());
                    batch.push(o);
                }
            }
            let mut seen = BTreeSet::new();
            batch.retain(|o| seen.insert(key(o)));
            let token = retried(|| self.spicedb.write(&batch, Duration::from_secs(5)))
                .await
                .unwrap_or_default();
            let ops: Vec<&str> = batch
                .iter()
                .map(|o| if o.touch { "touch" } else { "delete" })
                .collect();
            let resources: Vec<&str> = batch.iter().map(|o| o.resource.as_str()).collect();
            let relations: Vec<&str> = batch.iter().map(|o| o.relation.as_str()).collect();
            let subjects: Vec<&str> = batch.iter().map(|o| o.subject.as_str()).collect();
            self.ctx
                .pool
                .get()
                .await
                .unwrap()
                .query(
                    "select * from rebac.apply_watch($1, $2, $3, $4, $5)",
                    &[&ops, &resources, &relations, &subjects, &token],
                )
                .await
                .unwrap();
            let at = format!("{}, Watch step {step}", self.name);
            assert_within(&reads(&self.ctx).await, &truth(self.spicedb).await, &at);
        }
        drain(&self.ctx).await;
        let at = format!("{}, after Watch and the queues", self.name);
        assert_equal(&reads(&self.ctx).await, &truth(self.spicedb).await, &at);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn readable_sets_match_spicedb_at_every_commit() {
    if !enabled() {
        return;
    }
    std::env::set_var("REBAC_IT_TOKEN", KEY);
    let endpoint =
        std::env::var("REBAC_IT_SPICEDB").unwrap_or_else(|_| "http://127.0.0.1:50061".into());
    let env_u64 = |k: &str, d: u64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (rounds, seed0) = (
        env_u64("REBAC_ORACLE_ROUNDS", 8),
        env_u64("REBAC_ORACLE_SEED", 0x5ca1e),
    );
    for round in 0..rounds {
        let seed = seed0
            .wrapping_add(round)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            | 1;
        let mut rng = Rng(seed);
        let shape = Shape::draw(&mut rng);
        let schema = shape.schema();
        if let Err(e) =
            rebac_schema::closure(&rebac_schema::parse(&schema).unwrap(), "container", "view")
        {
            eprintln!("round {round} ({shape:?}) skipped, the closure refuses it: {e}");
            continue;
        }
        eprintln!("round {round}, seed {seed}: {shape:?}");
        // A client per round: SpiceDB ends a connection at its maximum age, and a round must not inherit one.
        let spicedb = SpiceDb::connect(&endpoint, KEY, Consistency::FullyConsistent)
            .await
            .unwrap();
        for ty in ["container", "workspace", "group"] {
            let _ = spicedb.delete_all(ty, Duration::from_secs(5)).await;
        }
        retried(|| spicedb.write_schema(&schema, Duration::from_secs(5))).await;
        let mut live = BTreeMap::new();
        for _ in 0..90 {
            let o = shape.tuple(&mut rng);
            live.insert(key(&o), o);
        }
        let all: Vec<TupleOp> = live.values().cloned().collect();
        retried(|| spicedb.write(&all, Duration::from_secs(10))).await;
        let ctx = daemon(&endpoint).await;
        closure::check_at_start(&ctx, true).await.unwrap();
        let mut r = Round {
            name: format!("round {round}"),
            rng,
            shape,
            spicedb: &spicedb,
            ctx,
            live,
        };
        r.backfill().await;
        r.through_write().await;
        r.through_watch().await;
    }
}
