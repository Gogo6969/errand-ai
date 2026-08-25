-- Whether a queued message is bad news, said in a column of its own.

-- Until now the flag travelled as the literal string 'failure-notice' in
-- last_error, which is also where a real delivery error is written. The two
-- collided the moment anything went wrong: a Telegram outage puts the message
-- into retry_wait and writes the actual error over the top, so on the next
-- attempt the message is no longer bad news. Quiet hours then hold it until
-- morning — and the one message a person wants their phone to wake them for is
-- the one saying their task failed.
--
-- It also meant a perfectly healthy queued notice reported its own error as
-- "failure-notice" in the run's list of messages, which reads as a failure to
-- anybody looking at it.
--
-- Rows queued before this migration are carried across rather than reinterpreted:
-- the flag is copied from the sentinel and the sentinel is cleared, so a message
-- already sitting in the queue at upgrade time keeps the meaning it was given.
-- The reader still recognises the old sentinel as well, for anything an older
-- daemon queued after this ran.
ALTER TABLE msg_outbox ADD COLUMN is_failure INTEGER NOT NULL DEFAULT 0;

UPDATE msg_outbox SET is_failure = 1, last_error = NULL WHERE last_error = 'failure-notice';
