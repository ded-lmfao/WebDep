-- Times an app reached or crossed one of its limits (cpu, ram, disk). The panel shows each one to
-- the people who can see the app, once, as a pop-up.
CREATE TABLE IF NOT EXISTS limit_events (
    id BIGSERIAL PRIMARY KEY,
    created_at BIGINT NOT NULL DEFAULT 0,
    kind TEXT NOT NULL,
    app_id BIGINT NOT NULL,
    app_name TEXT NOT NULL DEFAULT '',
    limit_kind TEXT NOT NULL,
    message TEXT NOT NULL DEFAULT '',
    stopped BOOLEAN NOT NULL DEFAULT FALSE
);
