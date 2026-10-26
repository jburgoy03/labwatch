-- One row per problem: a check going Warning or Critical, until it recovers.
CREATE TABLE incidents (
    id           INTEGER PRIMARY KEY,
    collector    TEXT    NOT NULL,           -- "services", "docker", ...
    check_name   TEXT    NOT NULL,           -- "Jellyfin", "gluetun", ...
    started_at   INTEGER NOT NULL,           -- Unix time, seconds
    ended_at     INTEGER,                    -- NULL while still open
    worst_status INTEGER NOT NULL,           -- 2 warning, 3 critical
    detail       TEXT    NOT NULL,           -- what the check said when it broke
    interrupted  INTEGER NOT NULL DEFAULT 0  -- 1 if Labwatch restarted mid-incident
);

-- A check can only have one open incident at a time. A partial index only
-- covers rows matching its WHERE, so closed incidents don't count.
CREATE UNIQUE INDEX incidents_one_open
    ON incidents (collector, check_name)
    WHERE ended_at IS NULL;

-- The timeline asks for "everything since X", newest first.
CREATE INDEX incidents_by_start ON incidents (started_at);
