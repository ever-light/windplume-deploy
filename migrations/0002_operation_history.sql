ALTER TABLE deployments
    ADD COLUMN operation TEXT NOT NULL DEFAULT 'deploy'
    CHECK (operation IN ('deploy', 'recreate', 'stop', 'down'));
