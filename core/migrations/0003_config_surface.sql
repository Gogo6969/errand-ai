-- Everything a task's settings screen needs to be able to change.
--
-- Until now a task's schedule, sites, limits and notify preferences could only
-- be chosen at creation, and the allowlist was never written by anything at
-- all, so every task shipped with an empty one and could not open a page.

-- ------------------------------------------------------- catch-up floor --

-- The line below which a task's schedule did not exist yet.
--
-- The scheduler's catch-up cursor is global: one settings row, scheduler
-- last_tick, shared by every task. So the moment somebody changes one task's
-- schedule, every occurrence of the NEW schedule between that shared cursor
-- and now reads as missed and eligible to run. Switching a task to a daily
-- cron would fire a historical run on the spot, and burn that occurrence id
-- permanently, because (task_id, occurrence_id) is unique and a burnt id can
-- never run again.
--
-- NULL means no floor, which is right for every task that already exists: they
-- have been running against their current schedule all along.
ALTER TABLE tasks ADD COLUMN catch_up_floor_at TEXT;

-- ---------------------------------------------------------- fence scope --

-- An optional extra discriminator on the idempotency key.
--
-- The key is built as task:occurrence:action_kind, and folding this in happens
-- ONLY when it is non-empty, so every row written before this column existed
-- keeps a byte-identical key. That matters more than it looks: if a committed
-- booking's key changed shape, the fence would stop recognising it and the next
-- attempt would read an already-taken slot as free and book it twice.
ALTER TABLE side_effects ADD COLUMN scope TEXT NOT NULL DEFAULT '';

-- ------------------------------------------------------------ recipients --

-- People a task may contact. Addresses only; nothing here is a secret, but a
-- phone number or an email address is somebody's personal data, so the agent
-- is shown a masked form and never the address itself.
CREATE TABLE recipients (
  id         TEXT PRIMARY KEY,
  label      TEXT NOT NULL,
  channel    TEXT NOT NULL
               CHECK (channel IN ('telegram','whatsapp','apple_mail','imessage')),
  address    TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- A join table rather than a JSON list on the task, for the same reason
-- credentials use one: "this task may contact these people" is a per-task
-- grant, and the grant is the security boundary. A boundary kept in a JSON blob
-- is a boundary nothing can constrain, index, or cascade.
CREATE TABLE task_recipients (
  task_id      TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  recipient_id TEXT NOT NULL REFERENCES recipients(id) ON DELETE CASCADE,
  -- What this task may tell this person about. Both on by default: a person
  -- added to a task is presumed to want to hear how it went either way.
  on_success   INTEGER NOT NULL DEFAULT 1,
  on_failure   INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (task_id, recipient_id)
);
