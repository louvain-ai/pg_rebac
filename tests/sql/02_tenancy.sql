-- A principal id is not unique across tenants: each tenant's projection is its own.
select t.beat();
select fence.project('acme', 'sam', array['a1'], 't');
select fence.project('globex', 'sam', array['g1', 'g2'], 't');
set fence.tenant = 'acme';
select t.eq(t.set_of('sam'), array['a1'], 'acme sees only acme');
set fence.tenant = 'globex';
select t.eq(t.set_of('sam'), array['g1', 'g2'], 'globex sees only globex');
-- Re-projecting in one tenant never clears the other.
select fence.project('acme', 'sam', '{}', 't');
select t.eq(t.set_of('sam'), array['g1', 'g2'], 'a clear in acme leaves globex');
-- A write whose ids carry another tenant's prefix is refused.
select t.fixture_relations();
set fence.tenant = 'acme';
select t.raises($$ select fence.write('touch', 'container:globex/x', 'member', 'user:acme/sam') $$, 'FN422',
                'a resource from another tenant');
select t.raises($$ select fence.write('touch', 'container:acme/x', 'member', 'user:globex/sam') $$, 'FN422',
                'a subject from another tenant');
select t.raises($$ select fence.write('grant', 'container:acme/x', 'member', 'user:acme/sam') $$, 'FN422',
                'an unknown op');
-- principals_of refuses a foreign prefix.
select t.eq((select count(*) from fence.principals_of('acme', 'user:globex/sam'))::int, 0, 'foreign subject maps to nobody');
