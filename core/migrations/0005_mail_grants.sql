-- Which tasks may look at the person's mail, and which may tidy it.
--
-- A table rather than a column on tasks, for the reason task_recipients is a
-- table: "this task may read my post" is a per-task grant, and a grant is a
-- security boundary. A boundary kept in a JSON blob is a boundary nothing can
-- constrain, index, or cascade. A row here exists only because somebody chose
-- to create it, so the absence of a row is a refusal rather than a default
-- nobody noticed.
--
-- Nothing here records WHICH mailboxes: the grant is over the mail account as
-- Mail presents it, and pretending otherwise would be a promise the AppleScript
-- underneath cannot keep.
CREATE TABLE task_mail_grants (
  task_id    TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
  -- Reading and filing are two different sizes of trust, so they are two
  -- different answers. Reading is off nobody's machine and undoable by nothing;
  -- moving a message is a change to the person's own mailbox that Errand cannot
  -- put back. Off by default: a task allowed to read is not thereby allowed to
  -- rearrange.
  may_file   INTEGER NOT NULL DEFAULT 0,
  granted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
