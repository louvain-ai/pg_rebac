-- A revocation written through rebac.write is gone from the projection when the transaction commits.
select t.fixture_relations();
select t.beat();
set rebac.tenant = 'acme';

-- Direct revocation.
select rebac.project('acme', 'priya', array['space-7', 'space-8'], 't');
begin;
select rebac.write('delete', 'container:acme/space-7', 'member', 'user:acme/priya');
select t.eq(t.set_of('priya'), array['space-7', 'space-8'], 'not narrowed before commit');
commit;
select t.eq(t.set_of('priya'), array['space-8'], 'narrowed at commit');
select t.ok(rebac.version('priya') > 1, 'version moved');

-- A demotion is not a revocation: editor deleted, member still held by the outbox's net state.
select rebac.write('touch', 'container:acme/space-8', 'member', 'user:acme/priya');
select rebac.write('touch', 'container:acme/space-8', 'editor', 'user:acme/priya');
begin;
select rebac.write('delete', 'container:acme/space-8', 'editor', 'user:acme/priya');
commit;
select t.eq(t.set_of('priya'), array['space-8'], 'demotion keeps the container');

-- Grant then revoke in one transaction nets to a revocation; revoke then grant nets to nothing.
select rebac.project('acme', 'omar', array['s1', 's2'], 't');
begin;
select rebac.write('delete', 'container:acme/s1', 'member', 'user:acme/omar');
select rebac.write('touch', 'container:acme/s1', 'member', 'user:acme/omar');
select rebac.write('touch', 'container:acme/s2', 'member', 'user:acme/omar');
select rebac.write('delete', 'container:acme/s2', 'member', 'user:acme/omar');
commit;
select t.eq(t.set_of('omar'), array['s1'], 'net op per relationship');

-- Linked subjects: a principal that reads as another subject loses what that subject loses.
insert into rebac.subject values ('acme', 'lee', 'user:acme/chat_U1', true), ('acme', 'kim', 'user:acme/chat_U2', false);
select rebac.project('acme', 'lee', array['chan-1'], 't');
select rebac.project('acme', 'kim', array['chan-1'], 't');
select rebac.write('delete', 'container:acme/chan-1', 'member', 'user:acme/chat_U1');
select rebac.write('delete', 'container:acme/chan-1', 'member', 'user:acme/chat_U2');
select t.eq(t.set_of('lee'), '{}'::text[], 'a trusted linked subject narrows its principal');
select t.eq(t.set_of('kim'), array['chan-1'], 'an untrusted link is not a subject');

-- A space's reader who loses the space loses the files under it (self-arrow edges).
select rebac.write('touch', 'container:acme/file-1', 'parent', 'container:acme/space-9');
select rebac.write('touch', 'container:acme/file-2', 'parent', 'container:acme/file-1');
select rebac.project('acme', 'ana', array['space-9', 'file-1', 'file-2', 'other'], 't');
select rebac.write('delete', 'container:acme/space-9', 'member', 'user:acme/ana');
select t.eq(t.set_of('ana'), array['other'], 'children go with the parent, transitively');

-- A file leaving its space: everyone who holds it loses it (its direct sharees come back on refresh).
select rebac.project('acme', 'ben', array['space-9', 'file-1'], 't');
select rebac.project('acme', 'cy', array['file-1'], 't');
select rebac.write('delete', 'container:acme/file-1', 'parent', 'container:acme/space-9');
select t.eq(t.set_of('ben'), array['space-9'], 'detached file narrowed for a space reader');
select t.eq(t.set_of('cy'), '{}'::text[], 'detached file narrowed for everyone');
select t.ok(not exists (select 1 from rebac.edge where child = 'file-1' and parent = 'container:space-9'), 'edge removed');

-- A workspace admin removed loses every container of that workspace, found by edge, not by id convention.
select rebac.write('touch', 'container:acme/w-a', 'workspace', 'workspace:acme/w1');
select rebac.write('touch', 'container:acme/w-b', 'workspace', 'workspace:acme/w1');
select rebac.write('touch', 'container:acme/w-c', 'workspace', 'workspace:acme/w2');
select rebac.project('acme', 'adm', array['w-a', 'w-b', 'w-c'], 't');
select rebac.write('delete', 'workspace:acme/w1', 'admin', 'user:acme/adm');
select t.eq(t.set_of('adm'), array['w-c'], 'via relation narrows through edges');

-- A row written inside a savepoint is still this transaction's.
select rebac.project('acme', 'sv', array['x1'], 't');
begin;
savepoint a;
select rebac.write('delete', 'container:acme/x1', 'member', 'user:acme/sv');
release savepoint a;
commit;
select t.eq(t.set_of('sv'), '{}'::text[], 'savepoint write narrowed');

-- A rolled-back revocation narrows nothing.
select rebac.project('acme', 'rb', array['y1'], 't');
begin;
select rebac.write('delete', 'container:acme/y1', 'member', 'user:acme/rb');
rollback;
select t.eq(t.set_of('rb'), array['y1'], 'rollback leaves the projection');

-- Narrowing in another tenant is not visible here: same principal id, same container name.
select rebac.project('globex', 'priya', array['space-7'], 't');
select t.eq((select count(*) from rebac.readable where tenant = 'globex' and principal = 'priya')::int, 1,
            'acme revocation did not touch globex');

-- Calling narrow_pending explicitly first, then commit, removes exactly once.
select rebac.project('acme', 'ex', array['z1', 'z2'], 't');
begin;
select rebac.write('delete', 'container:acme/z1', 'member', 'user:acme/ex');
select t.eq(rebac.narrow_pending(), 1, 'explicit narrow');
select rebac.write('delete', 'container:acme/z2', 'member', 'user:acme/ex');
commit;
select t.eq(t.set_of('ex'), '{}'::text[], 'rows written after an explicit narrow are narrowed at commit');

-- Why an intersection is refused: removing one granted relation keeps a container another relation the outbox shows
-- granted still holds. That is exact for a union (`member + editor`) and would fail open for `member & editor`.
insert into rebac.view_relation (resource_type, relation, kind) values ('container', 'editor', 'direct')
  on conflict do nothing;
select rebac.write('touch', 'container:acme/both', 'member', 'user:acme/kim');
select rebac.write('touch', 'container:acme/both', 'editor', 'user:acme/kim');
select rebac.project('acme', 'kim', array['both'], 't', 'live', now());
select rebac.write('delete', 'container:acme/both', 'editor', 'user:acme/kim');
select rebac.narrow_pending();
select t.eq(t.set_of('kim'), array['both'], 'a union: still granted through member, so kept');

-- "Still held" counts only what SpiceDB holds or will: a grant SpiceDB refused (quarantined) holds nothing, so
-- revoking the relation that did grant narrows at commit.
select rebac.project('acme', 'nadia', array['d1'], 't');
select rebac.write('touch', 'container:acme/d1', 'editor', 'user:acme/nadia');
select rebac.record_delivery('{}', null, array[(select max(seq) from rebac.outbox)], array['INVALID_ARGUMENT: refused']);
begin;
select rebac.write('delete', 'container:acme/d1', 'member', 'user:acme/nadia');
commit;
select t.eq(t.set_of('nadia'), '{}'::text[], 'a quarantined grant does not keep a revoked container');
