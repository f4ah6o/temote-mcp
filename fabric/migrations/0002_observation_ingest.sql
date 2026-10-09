-- Temote Fabric cloud observation / knowledge plane C1
--
-- C0 intentionally did not persist a canonical immutable payload fingerprint.
-- C1 needs one to distinguish exact replay from conflicting replay without
-- overwriting an earlier observation. This migration is additive: existing C0
-- rows remain readable; all new observation inserts must carry the digest.

ALTER TABLE observations ADD COLUMN payload_digest TEXT;

CREATE TRIGGER observations_payload_digest_required
BEFORE INSERT ON observations
WHEN NEW.payload_digest IS NULL OR length(NEW.payload_digest) <> 64
BEGIN
  SELECT RAISE(ABORT, 'observation payload_digest is required');
END;

-- repository_key is part of the cloud namespace. Keep every new observation
-- aligned with its owning source row even under concurrent sync attempts.
CREATE TRIGGER observation_sources_repository_immutable
BEFORE UPDATE OF repository_key ON observation_sources
WHEN OLD.repository_key IS NOT NULL
  AND NEW.repository_key IS NOT OLD.repository_key
BEGIN
  SELECT RAISE(ABORT, 'observation source repository_key is immutable once resolved');
END;

CREATE TRIGGER observations_source_repository_match
BEFORE INSERT ON observations
WHEN NOT EXISTS (
  SELECT 1
  FROM observation_sources AS source
  WHERE source.owner_id = NEW.owner_id
    AND source.host_id = NEW.host_id
    AND source.session_id = NEW.session_id
    AND (
      source.repository_key IS NULL
      OR source.repository_key IS NEW.repository_key
    )
)
BEGIN
  SELECT RAISE(ABORT, 'observation repository_key does not match source');
END;
