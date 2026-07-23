CREATE TABLE IF NOT EXISTS service_state (
    service_id TEXT PRIMARY KEY,
    desired_version TEXT NOT NULL,
    image TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_deployment_id TEXT NULL
);

CREATE TABLE IF NOT EXISTS deployments (
    id TEXT PRIMARY KEY,
    service_id TEXT NOT NULL,
    previous_version TEXT NULL,
    target_version TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued','running','succeeded','failed','interrupted')),
    started_at TEXT NOT NULL,
    finished_at TEXT NULL,
    command_log TEXT NOT NULL DEFAULT '',
    error_message TEXT NULL,
    rollback_status TEXT NULL CHECK (rollback_status IS NULL OR rollback_status IN ('not_needed','succeeded','failed','unavailable'))
);
CREATE INDEX IF NOT EXISTS deployments_started_at_idx ON deployments(started_at DESC);

