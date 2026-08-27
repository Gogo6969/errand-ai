-- Whether a run was a rehearsal, said beside the mode rather than inside it.

-- Until now "rehearsal" was one of three values in the mode column, which made
-- it exclusive with the other two. Teaching was therefore always for real: the
-- first run of "clean my mailbox of spam" really moved the post, and the first
-- run of "book a tennis court" really booked it. That is exactly backwards,
-- because the first run is the one nobody has watched yet.
--
-- Teaching and rehearsing are two different questions: what is this run for,
-- and is anything it does real. A run can be both, so it takes two columns.
-- The mode keeps its meaning and its three values, and this answers the second
-- question on its own.
ALTER TABLE runs ADD COLUMN rehearsal INTEGER NOT NULL DEFAULT 0;

-- Runs recorded before this existed said it in the mode, so they keep the
-- meaning they were given. Without this an old rehearsal would read back as a
-- run that did things for real.
UPDATE runs SET rehearsal = 1 WHERE mode = 'dry_run';
