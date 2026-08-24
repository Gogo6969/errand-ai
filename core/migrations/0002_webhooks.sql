-- Webhooks and idempotency, for clients that drive Errand from outside.

CREATE TABLE webhooks (
  id            TEXT PRIMARY KEY,
  token_id      TEXT REFERENCES api_tokens(id) ON DELETE CASCADE,
  url           TEXT NOT NULL,
  events        TEXT NOT NULL,
  secret_hash   TEXT NOT NULL,
  active        INTEGER NOT NULL DEFAULT 1,
  failure_count INTEGER NOT NULL DEFAULT 0,
  last_error    TEXT,
  created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE webhook_deliveries (
  id            TEXT PRIMARY KEY,
  webhook_id    TEXT NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
  event         TEXT NOT NULL,
  payload       TEXT NOT NULL,
  attempts      INTEGER NOT NULL DEFAULT 0,
  status_code   INTEGER,
  next_retry_at TEXT,
  delivered_at  TEXT,
  last_error    TEXT,
  created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX idx_deliveries_due ON webhook_deliveries(delivered_at, next_retry_at);

-- A client that retries after a dropped connection must get the same run back,
-- not a second booking.
CREATE TABLE idempotency_keys (
  key             TEXT PRIMARY KEY,
  endpoint        TEXT NOT NULL,
  request_sha256  TEXT NOT NULL,
  response_status INTEGER NOT NULL,
  response_body   TEXT NOT NULL,
  created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
