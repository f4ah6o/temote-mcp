-- 0003 created memory_outbox empty, and only observation ingest writes rows
-- into it. A database migrated with retained observations therefore has no
-- outbox row at all, so the scheduled sweep can never wake the memory worker
-- and a rebuild from retained observations cannot start once the source host
-- is offline. Seed one pending outbox row per repository at its retained
-- MAX(cloud_seq) so the ordinary wake/consume path replays the backlog.
--
-- The upsert mirrors the ingest write: repositories already carrying an
-- outbox row keep their durable queued/attempt state unless the retained
-- head actually moved past it, and observations without a non-empty
-- repository_key never produce a row (ingest skips NULL and '' alike).
INSERT INTO memory_outbox (
  owner_id, repository_key, through_cloud_seq, queued_at,
  attempt_count, next_attempt_at, last_error_code, updated_at
)
SELECT owner_id, repository_key, MAX(cloud_seq), NULL, 0, 0, NULL,
       strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM observations
WHERE repository_key IS NOT NULL AND repository_key != ''
GROUP BY owner_id, repository_key
ON CONFLICT(owner_id, repository_key) DO UPDATE SET
  through_cloud_seq = MAX(memory_outbox.through_cloud_seq, excluded.through_cloud_seq),
  queued_at = CASE WHEN excluded.through_cloud_seq > memory_outbox.through_cloud_seq
                   THEN NULL ELSE memory_outbox.queued_at END,
  next_attempt_at = CASE WHEN excluded.through_cloud_seq > memory_outbox.through_cloud_seq
                         THEN 0 ELSE memory_outbox.next_attempt_at END,
  last_error_code = CASE WHEN excluded.through_cloud_seq > memory_outbox.through_cloud_seq
                        THEN NULL ELSE memory_outbox.last_error_code END,
  updated_at = excluded.updated_at;
