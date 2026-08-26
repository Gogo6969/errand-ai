-- The answer a run produced, as opposed to the story of producing it.
--
-- A run already had summary_md, which is one line about the work: where it
-- went and what it had to do. It never had anywhere to put the thing the
-- person actually asked for. So the agent was told, in the system prompt, to
-- write the answer into Apple Notes instead, and the app showed a receipt for
-- the filing while the answer sat in a note nobody asked for.
--
-- Nullable on purpose. Every run recorded before this column existed has no
-- answer, and that is not the same as having produced an empty one.
ALTER TABLE runs ADD COLUMN answer_md TEXT;

-- Where else a run put its answer.
--
-- A task may reasonably ask for a note, a file or a message, and then the
-- answer exists in two places: here, and there. Recorded as rows written by the
-- tool that actually did it, rather than read back out of the journal's
-- sentences, so a link the person clicks cannot point at something that never
-- happened.
CREATE TABLE IF NOT EXISTS run_answer_copies (
    id          TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    -- 'note' | 'file' | 'message'
    kind        TEXT NOT NULL,
    -- What to call it on screen: a note title, a file name, a person's label.
    label       TEXT NOT NULL,
    -- Enough to open it again, and nothing a caller may choose for itself.
    locator     TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS answer_copies_run ON run_answer_copies(run_id);
