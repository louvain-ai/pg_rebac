-- A principal id is not unique across tenants: each tenant's projection is its own.
select t.beat();
select rebac.project('acme', 'sam', array['a1'], 't');
select rebac.project('globex', 'sam', array['g1', 'g2'], 't');
set rebac.tenant = 'acme';
select t.eq(t.set_of('sam'), array['a1'], 'acme sees only acme');
set rebac.tenant = 'globex';
select t.eq(t.set_of('sam'), array['g1', 'g2'], 'globex sees only globex');
-- Re-projecting in one tenant never clears the other.
select rebac.project('acme', 'sam', '{}', 't');
select t.eq(t.set_of('sam'), array['g1', 'g2'], 'a clear in acme leaves globex');
-- A write whose ids carry another tenant's prefix is refused.
select t.fixture_relations();
set rebac.tenant = 'acme';
select t.raises($$ select rebac.write('touch', 'container:globex/x', 'member', 'user:acme/sam') $$, 'RB422',
                'a resource from another tenant');
select t.raises($$ select rebac.write('touch', 'container:acme/x', 'member', 'user:globex/sam') $$, 'RB422',
                'a subject from another tenant');
select t.raises($$ select rebac.write('grant', 'container:acme/x', 'member', 'user:acme/sam') $$, 'RB422',
                'an unknown op');
-- principals_of refuses a foreign prefix.
select t.eq((select count(*) from rebac.principals_of('acme', 'user:globex/sam'))::int, 0, 'foreign subject maps to nobody');

-- A tenant name containing the tenant separator would make '<tenant><sep><id>' ambiguous (tenant 'a/b' and tenant
-- 'a' with id 'b/...'): refused everywhere a tenant is named.
set rebac.tenant = 'acme/eu';
select t.raises($$select * from rebac.readable_set('priya')$$, 'RB422', 'a read under a tenant with the separator');
select t.raises($$select rebac.write('touch', 'container:acme/eu/x', 'member', 'user:acme/eu/priya')$$, 'RB422',
                'a write under a tenant with the separator');
select t.raises($$select rebac.project('acme/eu', 'priya', array['x'], 't')$$, 'RB422', 'a projection of such a tenant');
select t.raises($$select rebac.retire('acme/eu', 'priya')$$, 'RB422', 'a retirement in such a tenant');
reset rebac.tenant;
