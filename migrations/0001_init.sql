CREATE TABLE service_state (
    project_id TEXT NOT NULL,
    service_id TEXT NOT NULL,
    desired_version TEXT NOT NULL,
    image TEXT NOT NULL,
    pinned_image TEXT NOT NULL,
    image_digest TEXT NOT NULL,
    image_id TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_deployment_id TEXT NOT NULL,
    PRIMARY KEY (project_id, service_id)
);

CREATE TABLE deployments (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    service_id TEXT NOT NULL,
    operation TEXT NOT NULL
        CHECK (operation IN ('deploy','rollback','recreate','stop','down')),
    previous_version TEXT NULL,
    target_version TEXT NOT NULL,
    status TEXT NOT NULL
        CHECK (status IN ('queued','running','succeeded','failed','interrupted')),
    started_at TEXT NOT NULL,
    finished_at TEXT NULL,
    command_log TEXT NOT NULL DEFAULT '',
    error_message TEXT NULL,
    rollback_status TEXT NULL
        CHECK (rollback_status IS NULL OR rollback_status IN ('not_needed','succeeded','failed','unavailable'))
);
CREATE INDEX deployments_started_at_idx ON deployments(started_at DESC);
CREATE INDEX deployments_project_service_idx
    ON deployments(project_id, service_id, started_at DESC);

CREATE TABLE deployment_artifacts (
    deployment_id TEXT PRIMARY KEY REFERENCES deployments(id) ON DELETE CASCADE,
    phase TEXT NOT NULL DEFAULT 'queued',
    target_image TEXT NULL,
    target_pinned_image TEXT NULL,
    target_digest TEXT NULL,
    target_image_id TEXT NULL
);

CREATE TABLE service_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    deployment_id TEXT NOT NULL UNIQUE,
    project_id TEXT NOT NULL,
    service_id TEXT NOT NULL,
    version TEXT NOT NULL,
    image TEXT NOT NULL,
    pinned_image TEXT NOT NULL,
    image_digest TEXT NOT NULL,
    image_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX service_revisions_service_idx
    ON service_revisions(project_id, service_id, id DESC);
