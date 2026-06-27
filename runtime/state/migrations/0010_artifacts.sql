-- Migration 0010: content-addressed artifact store. Large payloads (e.g. model
-- node outputs) are written once keyed by content hash and referenced by a small
-- ArtifactRef sentinel, shrinking the event log. Tenant-scoped; no cross-tenant
-- dedup (content-inference safety); write-once (no GC in v1).
CREATE TABLE IF NOT EXISTS artifacts (
    tenant_id   TEXT    NOT NULL DEFAULT 'default',
    hash        TEXT    NOT NULL,
    bytes       BLOB    NOT NULL,
    size        INTEGER NOT NULL,
    media_type  TEXT,
    created_at  TEXT    NOT NULL,
    PRIMARY KEY (tenant_id, hash)
);
CREATE INDEX IF NOT EXISTS idx_artifacts_hash ON artifacts(hash);
