-- Every reading from every check, kept for 24 hours.
CREATE TABLE samples (
    ts         INTEGER NOT NULL, -- Unix time, seconds
    collector  TEXT    NOT NULL, -- "host", "services", ...
    check_name TEXT    NOT NULL, -- "CPU", "Jellyfin", ...
    status     INTEGER NOT NULL, -- 0 ok, 1 unknown, 2 warning, 3 critical
    value      REAL              -- NULL for checks with nothing to graph
);

-- History queries filter by check and time range.
CREATE INDEX samples_by_check ON samples (collector, check_name, ts);

-- One row per check per hour, rolled up from samples. Kept for 30 days.
CREATE TABLE hourly (
    hour         INTEGER NOT NULL, -- Unix time of the hour's start
    collector    TEXT    NOT NULL,
    check_name   TEXT    NOT NULL,
    worst_status INTEGER NOT NULL,
    avg_value    REAL,
    min_value    REAL,
    max_value    REAL,
    sample_count INTEGER NOT NULL,
    PRIMARY KEY (collector, check_name, hour)
);
