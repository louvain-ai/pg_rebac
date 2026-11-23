-- The relay's claim, delivery recording, the refresh queue and the sweep.
select t.fixture_relations();
select t.beat();

-- Deletes before touches, per tenant; never a row behind an undelivered row for the same relationship.
set fence.tenant = 'acme';
select fence.write('touch', 'container:acme/a', 'member', 'user:acme/u1');    -- seq 1
select fence.write('touch', 'container:acme/b', 'member', 'user:acme/u2');    -- seq 2
select fence.write('delete', 'container:acme/a', 'member', 'user:acme/u1');   -- seq 3: waits behind seq 1
select fence.write('delete', 'container:acme/c', 'member', 'user:acme/u3');   -- seq 4
set fence.tenant = 'globex';
select fence.write('touch', 'container:globex/g', 'member', 'user:globex/v1'); -- seq 5
select fence.write('delete', 'container:globex/h', 'member', 'user:globex/v2'); -- seq 6

begin;
create temp table claimed as select * from fence.claim_relay(100, 50, 5000);
select t.eq((select array_agg(seq order by seq) from claimed), array[1, 2, 4, 5, 6]::bigint[],
            'seq 3 waits for its predecessor');
-- Revocations first across tenants: both tenants' deletes come before any touch.
select t.eq((select array_agg(op) from (select op from claimed limit 2) x), array['delete', 'delete'],
            'deletes first, across tenants');
commit;

-- A second relay claiming at the same time skips locked rows.
begin;
select count(*) from fence.claim_relay(100, 50, 5000);
rollback;

-- Fairness: at most per_tenant from any one tenant.
begin;
select t.eq((select count(*) from fence.claim_relay(100, 1, 5000))::int, 2, 'one per tenant');
rollback;

-- Delivery: rows marked, refused rows quarantined, refreshes queued (a revocation at priority 0).
select fence.record_delivery(array[1, 2, 4, 5]::bigint[], 'zed-1', array[6]::bigint[], array['INVALID_ARGUMENT']);
select t.eq((select count(*) from fence.outbox where synced_at is null)::int, 1, 'only seq 3 left');
select t.eq((select error from fence.outbox where seq = 6), 'INVALID_ARGUMENT', 'quarantined, kept');
select t.eq((select priority from fence.refresh where tenant = 'acme' and principal = 'u3'), 0::smallint, 'revocation priority');
select t.eq((select zedtoken from fence.refresh where tenant = 'acme' and principal = 'u1'), 'zed-1', 'token carried');
select t.ok(not exists (select 1 from fence.refresh where tenant = 'globex' and principal = 'v2'), 'a refused row refreshes nobody');
begin;
select t.eq((select array_agg(seq) from fence.claim_relay(100, 50, 5000)), array[3]::bigint[], 'seq 3 now claimable');
rollback;

-- Arrow deliveries queue their resource for LookupSubjects.
set fence.tenant = 'acme';
select fence.write('touch', 'container:acme/file', 'parent', 'container:acme/space');
select fence.record_delivery(array[(select max(seq) from fence.outbox)], 'zed-2', '{}', '{}');
select t.ok(exists (select 1 from fence.refresh_resource where resource = 'container:acme/file'), 'arrow queued');
select fence.complete_refresh_resource('acme', 'container:acme/file',
  (select generation from fence.refresh_resource where resource = 'container:acme/file'),
  array['user:acme/u9', 'user:globex/x'], 'zed-3');
select t.ok(exists (select 1 from fence.refresh where tenant = 'acme' and principal = 'u9'), 'viewer refreshed');
select t.ok(not exists (select 1 from fence.refresh where principal = 'x'), 'a foreign subject refreshes nobody');
select t.ok(not exists (select 1 from fence.refresh_resource), 'resource done');

-- Refresh claim, a re-enqueue while it runs, completion by generation, failure backoff.
delete from fence.refresh;
select fence.enqueue_refresh('acme', 'p1', 1::smallint, null);
create temp table c1 as select * from fence.claim_refresh(10, 30000);
select t.eq((select count(*) from fence.claim_refresh(10, 30000))::int, 0, 'leased rows are not claimed twice');
select fence.enqueue_refresh('acme', 'p1', 1::smallint, 'zed-9');
select fence.complete_refresh('acme', 'p1', (select generation from c1));
select t.ok(exists (select 1 from fence.refresh where principal = 'p1'), 're-enqueued during the refresh: kept');
select fence.complete_refresh('acme', 'p1', (select generation from fence.refresh where principal = 'p1'));
select t.ok(not exists (select 1 from fence.refresh where principal = 'p1'), 'completed');
select fence.enqueue_refresh('acme', 'p2', 1::smallint, null);
select fence.fail_refresh('acme', 'p2', 'unavailable');
select t.ok((select due_at > now() from fence.refresh where principal = 'p2'), 'backed off');

-- The sweep: one per interval; a recent delivered revocation re-sent exactly once, after it settles.
delete from fence.outbox;
set fence.tenant = 'acme';
select fence.write('delete', 'container:acme/s', 'member', 'user:acme/q');
update fence.outbox set synced_at = now() - interval '10 minutes';
select t.eq((select requeued from fence.sweep(900000, 300000)), 1::bigint, 'requeued once');
update fence.outbox set synced_at = now() - interval '10 minutes';
update fence.sweep_claim set last_at = now() - interval '1 hour';
select t.eq((select requeued from fence.sweep(900000, 300000)), 0::bigint, 'never twice');
select t.eq((select requeued from fence.sweep(900000, 300000)), 0::bigint, 'one sweep per interval');
