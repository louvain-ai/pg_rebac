-- The sample dataset: one tenant (acme), four containers, seventeen passages, two people.
--   handbook   everyone, through the group all-staff
--   payroll    alice, directly
--   deals      bob, directly
--   board      neither of them (and the most passages about pay)
-- Every relationship goes through rebac.write, in an ordinary transaction, as an application would write it.

create table if not exists passage (
  id        int primary key,
  container text not null,
  title     text not null,
  body      text not null,
  tsv       tsvector generated always as (to_tsvector('english', title || ' ' || body)) stored
);
create index if not exists passage_container_idx on passage (container);
create index if not exists passage_tsv_idx on passage using gin (tsv);

insert into passage (id, container, title, body) values
  (1,  'handbook', 'Pay days',              'Salaries are paid on the last working day of each month.'),
  (2,  'handbook', 'Annual pay review',      'Every salary is reviewed once a year, in March.'),
  (3,  'handbook', 'Expenses',              'Claim expenses within thirty days with a receipt.'),
  (4,  'payroll',  'Salary bands 2026',      'Engineering salary bands rise by four percent in the March pay review.'),
  (5,  'payroll',  'Payroll cut-off',        'Changes to salary must reach payroll by the 20th.'),
  (6,  'deals',    'Kestrel renewal',      'The renewal discount is capped at ten percent; pay terms stay at thirty days.'),
  (7,  'deals',    'Commission plan',        'Sales commission is paid quarterly on salary plus bonus.'),
  (8,  'board',    'CEO salary review',      'The board approved the CEO salary and pay review for 2026.'),
  (9,  'board',    'Executive pay',          'Executive pay and salary increases are set by the remuneration committee.'),
  (10, 'board',    'Pay review budget',      'The pay review budget for salary rises is capped at five percent.'),
  (11, 'board',    'Salary benchmark',       'The salary benchmark compares executive pay with ten peers.'),
  (12, 'board',    'Bonus pool',             'The bonus pool is twelve percent of salary and pay.'),
  (13, 'board',    'Pay ratio',              'The pay ratio of CEO salary to median salary is reported annually.'),
  (14, 'board',    'Remuneration minutes',   'Minutes: salary, pay review, and pay ratio for the executive team.'),
  (15, 'board',    'Severance terms',        'Severance pay is six months of salary.'),
  (16, 'payroll',  'Overtime pay',           'Overtime is paid at one and a half times the hourly salary rate.'),
  (17, 'deals',    'Reseller pay terms',     'Resellers are paid within forty-five days; the review is yearly.')
on conflict (id) do nothing;

-- Touched on every run (a touch is idempotent), so a run after the demo's revocation starts from the same grants.
begin;
set local rebac.tenant = 'acme';
select rebac.write('touch', 'group:acme/all-staff',   'member', 'user:acme/alice');
select rebac.write('touch', 'group:acme/all-staff',   'member', 'user:acme/bob');
select rebac.write('touch', 'container:acme/handbook', 'member', 'group:acme/all-staff#member');
select rebac.write('touch', 'container:acme/payroll',  'member', 'user:acme/alice');
select rebac.write('touch', 'container:acme/deals',    'member', 'user:acme/bob');
commit;
