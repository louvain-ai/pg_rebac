-- A schema change that alters what grants view: every read fails closed while the closure is invalid, and after the
-- rebuild each principal fails closed until it has been re-projected under the new closure.
select t.fixture_relations();
select t.beat();
set rebac.tenant = 'acme';
select rebac.project('acme', 'ana', array['c1'], 'tok');
select rebac.project('acme', 'bo', array['c2'], 'tok');
select t.eq(t.set_of('ana'), array['c1'], 'no closure state yet: valid');

select rebac.invalidate_closure('schema_changed');
select t.raises($$ select * from rebac.readable_set('ana') $$, 'RB503', 'an invalid closure fails every read closed');
select t.raises($$ select rebac.readable_by('bo', 'c2') $$, 'RB503', 'every principal');

-- The rebuild re-projects ana; bo's refresh has not happened yet.
select rebac.project('acme', 'ana', array['c1', 'c3'], 'tok', 'live', clock_timestamp());
select rebac.accept_closure();
select t.eq(t.set_of('ana'), array['c1', 'c3'], 'valid again, and re-projected after the change: read');
select t.raises($$ select * from rebac.readable_set('bo') $$, 'RB503', 'projected before the change: still closed');
select rebac.project('acme', 'bo', array['c2'], 'tok', 'live', clock_timestamp());
select t.eq(t.set_of('bo'), array['c2'], 're-projected: read');

-- With the freshness bound off, a set of unknown age is still closed after a closure change (unknown is not after).
update rebac.config set value = '0' where key = 'max_projection_age_ms';
insert into rebac.principal_state (tenant, principal, state, n_containers, version) values ('acme', 'mir', 'projected', 1, 1);
insert into rebac.readable values ('acme', 'mir', 'c1');
select t.raises($$ select * from rebac.readable_set('mir') $$, 'RB503', 'unknown lookup time after a change: closed');
update rebac.config set value = '3600000' where key = 'max_projection_age_ms';

-- Retired principals read nothing either way; accepting with no prior change gates nobody.
select rebac.retire('acme', 'bo');
select rebac.invalidate_closure('unsupported: exclusion');
select t.raises($$ select * from rebac.readable_set('bo') $$, 'RB503', 'invalid fails even a retired principal closed');
delete from rebac.closure_state;
select rebac.accept_closure();
select t.eq((select changed_at from rebac.closure_state), '-infinity'::timestamptz, 'accept without a change gates nobody');
select t.eq(t.set_of('ana'), array['c1', 'c3'], 'and reads go on');
