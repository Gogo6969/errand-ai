-- Errand-AI canonical schema, migration 0001.
--
-- This file is the schema of record referenced by the build plan. The enums
-- below are the single vocabulary: the same strings appear in the database,
-- the API wire objects, the SSE events, the Tauri events, and the webhooks.
-- There is no translation layer anywhere, deliberately.
--
-- Ids are UUIDv7 strings (time sortable). Timestamps are ISO-8601 UTC text.
-- Secrets never appear in this database. Credentials are references only.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------- settings --

CREATE TABLE settings (
  key        TEXT PRIMARY KEY,
  value_json TEXT NOT NULL,
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- ------------------------------------------------------------------- tasks --

-- status: draft | teaching | ready | paused | archived
CREATE TABLE tasks (
  id                      TEXT PRIMARY KEY,
  name                    TEXT NOT NULL,
  emoji                   TEXT,
  description_md          TEXT NOT NULL,
  understanding_md        TEXT,
  status                  TEXT NOT NULL DEFAULT 'draft'
                            CHECK (status IN ('draft','teaching','ready','paused','archived')),
  paused_reason           TEXT,
  auto_paused             INTEGER NOT NULL DEFAULT 0,
  schedule_json           TEXT NOT NULL DEFAULT '{"kind":"manual"}',
  notify_json             TEXT NOT NULL DEFAULT '{}',
  limits_json             TEXT NOT NULL
                            DEFAULT '{"max_steps":60,"max_minutes":15,"max_usd":0.5,"max_heal_cycles":2,"max_messages":3}',
  model_roles_json        TEXT,
  allowed_domains_json    TEXT NOT NULL DEFAULT '[]',
  strict_network          INTEGER NOT NULL DEFAULT 0,
  preauth_json            TEXT NOT NULL DEFAULT '{}',
  browser_profile_id      TEXT REFERENCES browser_profiles(id),
  active_playbook_version INTEGER,
  next_run_at             TEXT,
  created_at              TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  updated_at              TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX idx_tasks_status   ON tasks(status);
CREATE INDEX idx_tasks_next_run ON tasks(next_run_at) WHERE next_run_at IS NOT NULL;

-- -------------------------------------------------------------------- runs --

-- status: armed | queued | preflight | holding | running | healing
--       | waiting_input | takeover | succeeded | failed | cancelled | skipped
-- trigger: schedule | manual | api | teach | heal_retry | catch_up
-- mode: normal | teach | dry_run
CREATE TABLE runs (
  id                TEXT PRIMARY KEY,
  task_id           TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  occurrence_id     TEXT NOT NULL,
  playbook_version  INTEGER,
  mode              TEXT NOT NULL DEFAULT 'normal'
                      CHECK (mode IN ('normal','teach','dry_run')),
  trigger           TEXT NOT NULL
                      CHECK (trigger IN ('schedule','manual','api','teach','heal_retry','catch_up')),
  triggered_by      TEXT,
  status            TEXT NOT NULL DEFAULT 'queued'
                      CHECK (status IN ('armed','queued','preflight','holding','running','healing',
                                        'waiting_input','takeover','succeeded','failed','cancelled','skipped')),
  scheduled_for     TEXT,
  started_at        TEXT,
  finished_at       TEXT,
  lease_owner       TEXT,
  lease_expires_at  TEXT,
  heal_cycles       INTEGER NOT NULL DEFAULT 0,
  summary_md        TEXT,
  notes_md          TEXT,
  failure_code      TEXT,
  failure_human     TEXT,
  failure_technical TEXT,
  tokens_in         INTEGER NOT NULL DEFAULT 0,
  tokens_out        INTEGER NOT NULL DEFAULT 0,
  cost_usd          REAL    NOT NULL DEFAULT 0.0,
  created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

  -- A terminal failure must carry both a taxonomy code and a plain-language
  -- explanation. This is the Rust invariant from the plan, enforced in SQL too
  -- so no code path can close a run without answering the user's question.
  CHECK (status <> 'failed' OR (failure_code IS NOT NULL AND failure_human IS NOT NULL))
);

-- One scheduled occurrence can only ever produce one run.
CREATE UNIQUE INDEX idx_runs_occurrence ON runs(task_id, occurrence_id);
CREATE INDEX idx_runs_task_created ON runs(task_id, created_at DESC);
CREATE INDEX idx_runs_status ON runs(status);

-- kind: plan | navigate | act | read | decide | credential | wait
--     | message | screenshot | heal | intervention | note
CREATE TABLE run_steps (
  run_id      TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL,
  ts          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  kind        TEXT NOT NULL
                CHECK (kind IN ('plan','navigate','act','read','decide','credential','wait',
                                'message','screenshot','heal','intervention','note')),
  title       TEXT NOT NULL,
  detail_json TEXT,
  artifact_id TEXT,
  duration_ms INTEGER,
  ok          INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (run_id, seq)
);

-- Artifacts are addressed by id, never by client-supplied filename.
CREATE TABLE run_artifacts (
  id         TEXT PRIMARY KEY,
  run_id     TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  seq        INTEGER,
  kind       TEXT NOT NULL,
  rel_path   TEXT NOT NULL,
  masked     INTEGER NOT NULL DEFAULT 1,
  bytes      INTEGER,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX idx_artifacts_run ON run_artifacts(run_id);

-- --------------------------------------------------------------- playbooks --

CREATE TABLE playbook_versions (
  task_id           TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  version           INTEGER NOT NULL,
  rel_path          TEXT NOT NULL,
  sha256            TEXT NOT NULL,
  source            TEXT NOT NULL
                      CHECK (source IN ('teach','refine','fixer','manual_edit')),
  created_by_run_id TEXT REFERENCES runs(id) ON DELETE SET NULL,
  approved          INTEGER NOT NULL DEFAULT 0,
  changelog_md      TEXT,
  confidence        REAL,
  created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  PRIMARY KEY (task_id, version)
);

-- ------------------------------------------------------------- credentials --

-- Metadata only. Secret material lives in the macOS login keychain and is read
-- exclusively by errandd at the moment of use.
CREATE TABLE credentials (
  id                TEXT PRIMARY KEY,
  label             TEXT NOT NULL,
  kind              TEXT NOT NULL
                      CHECK (kind IN ('password','totp','api_key','note')),
  username          TEXT,
  domain            TEXT NOT NULL,
  keychain_service  TEXT NOT NULL,
  keychain_account  TEXT NOT NULL,
  require_biometric INTEGER NOT NULL DEFAULT 0,
  created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  last_used_at      TEXT,
  use_count         INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE task_credentials (
  task_id       TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  credential_id TEXT NOT NULL REFERENCES credentials(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, credential_id)
);

-- ---------------------------------------------------------------- browsers --

CREATE TABLE browser_profiles (
  id             TEXT PRIMARY KEY,
  name           TEXT NOT NULL UNIQUE,
  dir_name       TEXT NOT NULL,
  default_domain TEXT,
  locked_by_run  TEXT REFERENCES runs(id) ON DELETE SET NULL,
  created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  last_used_at   TEXT
);

-- ----------------------------------------------------------- side effects --

-- The fence. Key is occurrence-scoped, never outcome-scoped, so a retry that
-- picks a different resource cannot slip past it and double-book.
-- state: armed | committed | aborted
CREATE TABLE side_effects (
  id              TEXT PRIMARY KEY,
  run_id          TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  task_id         TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  occurrence_id   TEXT NOT NULL,
  action_kind     TEXT NOT NULL
                    CHECK (action_kind IN ('booking','purchase','message','form_submit','deletion')),
  idempotency_key TEXT NOT NULL,
  state           TEXT NOT NULL DEFAULT 'armed'
                    CHECK (state IN ('armed','committed','aborted')),
  evidence_json   TEXT,
  armed_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  committed_at    TEXT
);

CREATE UNIQUE INDEX idx_fence ON side_effects(idempotency_key);

-- ------------------------------------------------------------------ outbox --

-- class: outreach | notify | test
-- channel: telegram | whatsapp | apple_mail | imessage
-- state: queued | sending | uncertain | sent | retry_wait | deferred_quiet
--      | needs_user | dead
CREATE TABLE msg_outbox (
  id              TEXT PRIMARY KEY,
  run_id          TEXT REFERENCES runs(id) ON DELETE SET NULL,
  task_id         TEXT REFERENCES tasks(id) ON DELETE SET NULL,
  class           TEXT NOT NULL CHECK (class IN ('outreach','notify','test')),
  channel         TEXT NOT NULL CHECK (channel IN ('telegram','whatsapp','apple_mail','imessage')),
  recipient       TEXT NOT NULL,
  recipient_label TEXT,
  subject         TEXT,
  body            TEXT NOT NULL,
  body_hash       TEXT NOT NULL,
  state           TEXT NOT NULL DEFAULT 'queued'
                    CHECK (state IN ('queued','sending','uncertain','sent','retry_wait',
                                     'deferred_quiet','needs_user','dead')),
  attempts        INTEGER NOT NULL DEFAULT 0,
  next_attempt_at TEXT,
  last_error      TEXT,
  provider_receipt TEXT,
  created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  sent_at         TEXT
);

CREATE INDEX idx_outbox_due ON msg_outbox(state, next_attempt_at);

-- --------------------------------------------------------------- providers --

CREATE TABLE provider_endpoints (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL
                  CHECK (kind IN ('claude_cli','anthropic_api','openai_compat')),
  label         TEXT NOT NULL,
  base_url      TEXT,
  dialect       TEXT,
  model         TEXT,
  auth_ref      TEXT,
  enabled       INTEGER NOT NULL DEFAULT 1,
  pinned        INTEGER NOT NULL DEFAULT 0,
  caps_json     TEXT,
  health_status TEXT,
  health_detail TEXT,
  checked_at    TEXT,
  discovered_at TEXT,
  created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- role: executor | planner | fixer | narrator
CREATE TABLE role_bindings (
  role        TEXT NOT NULL CHECK (role IN ('executor','planner','fixer','narrator')),
  scope       TEXT NOT NULL DEFAULT 'global',
  position    INTEGER NOT NULL,
  endpoint_id TEXT NOT NULL REFERENCES provider_endpoints(id) ON DELETE CASCADE,
  model       TEXT,
  params_json TEXT,
  PRIMARY KEY (role, scope, position)
);

-- ---------------------------------------------------------------- api auth --

-- scopes CSV drawn from: read, run, webhook, approve, manage, admin
CREATE TABLE api_tokens (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL UNIQUE,
  token_hash   TEXT NOT NULL,
  scopes       TEXT NOT NULL,
  created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  last_used_at TEXT,
  revoked_at   TEXT
);

CREATE TABLE api_audit (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  ts              TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  token_id        TEXT,
  token_name      TEXT,
  remote_ip       TEXT NOT NULL,
  method          TEXT NOT NULL,
  path            TEXT NOT NULL,
  status          INTEGER NOT NULL,
  latency_ms      INTEGER NOT NULL,
  request_id      TEXT NOT NULL,
  idempotency_key TEXT
);

CREATE INDEX idx_audit_ts ON api_audit(ts DESC);
