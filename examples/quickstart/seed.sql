-- The sample dataset: one tenant (acme), four containers, seventeen passages, two people.
--   handbook   everyone, through the group all-staff
--   payroll    alice, directly
--   deals      bob, directly
--   board      neither of them (and the most passages about pay)
-- Every relationship goes through rebac.write, in an ordinary transaction, as an application would write it.
-- The embeddings are three numbers each (pay, sales, policy), written by hand so the demo needs no model; a real
-- table holds a model's vectors, and the fence is the same.

create extension if not exists vector;

create table if not exists passage (
  id        int primary key,
  container text not null,
  title     text not null,
  body      text not null,
  embedding vector(3) not null
);
create index if not exists passage_container_idx on passage (container);
create index if not exists passage_embedding_idx on passage using hnsw (embedding vector_cosine_ops);

insert into passage (id, container, title, body, embedding) values
  (1,  'handbook', 'Pay days',              'Salaries are paid on the last working day of each month.', '[0.90,0.10,0.45]'),
  (2,  'handbook', 'Annual pay review',      'Every salary is reviewed once a year, in March.', '[0.97,0.05,0.21]'),
  (3,  'handbook', 'Expenses',              'Claim expenses within thirty days with a receipt.', '[0.20,0.10,0.95]'),
  (4,  'payroll',  'Salary bands 2026',      'Engineering salary bands rise by four percent in the March pay review.', '[0.92,0.05,0.30]'),
  (5,  'payroll',  'Payroll cut-off',        'Changes to salary must reach payroll by the 20th.', '[0.85,0.10,0.50]'),
  (6,  'deals',    'Kestrel renewal',      'The renewal discount is capped at ten percent; pay terms stay at thirty days.', '[0.30,0.90,0.20]'),
  (7,  'deals',    'Commission plan',        'Sales commission is paid quarterly on salary plus bonus.', '[0.70,0.60,0.10]'),
  (8,  'board',    'CEO salary review',      'The board approved the CEO salary and pay review for 2026.', '[0.99,0.01,0.10]'),
  (9,  'board',    'Executive pay',          'Executive pay and salary increases are set by the remuneration committee.', '[0.98,0.05,0.12]'),
  (10, 'board',    'Pay review budget',      'The pay review budget for salary rises is capped at five percent.', '[0.97,0.02,0.18]'),
  (11, 'board',    'Salary benchmark',       'The salary benchmark compares executive pay with ten peers.', '[0.90,0.15,0.30]'),
  (12, 'board',    'Bonus pool',             'The bonus pool is twelve percent of salary and pay.', '[0.85,0.25,0.20]'),
  (13, 'board',    'Pay ratio',              'The pay ratio of CEO salary to median salary is reported annually.', '[0.92,0.00,0.33]'),
  (14, 'board',    'Remuneration minutes',   'Minutes: salary, pay review, and pay ratio for the executive team.', '[0.96,0.05,0.25]'),
  (15, 'board',    'Severance terms',        'Severance pay is six months of salary.', '[0.80,0.00,0.55]'),
  (16, 'payroll',  'Overtime pay',           'Overtime is paid at one and a half times the hourly salary rate.', '[0.80,0.10,0.55]'),
  (17, 'deals',    'Reseller pay terms',     'Resellers are paid within forty-five days; the review is yearly.', '[0.50,0.80,0.20]')
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
