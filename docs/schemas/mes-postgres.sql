-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>
--
-- Proposed PostgreSQL schema for ingesting SPRD Flash Tool unit records
-- (see ../erp-mes-data-model.md and unit-record.schema.json) and computing
-- the manufacturing KPIs. Idempotent ingest, append-only history.

CREATE SCHEMA IF NOT EXISTS mes;
SET search_path TO mes;

-- ---------------------------------------------------------------------------
-- Master data
-- ---------------------------------------------------------------------------

CREATE TABLE firmware (
    firmware_id      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_name     text        NOT NULL,
    product_version  text        NOT NULL DEFAULT '',
    sdk_type         text        NOT NULL DEFAULT 'unknown',   -- LuatOS | CSDK | ...
    pac_sha256       char(64)    NOT NULL,
    file_name        text        NOT NULL,
    size_bytes       bigint      NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (pac_sha256)
);

CREATE TABLE station (
    station_id    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    label         text NOT NULL,                 -- "fixture-3"
    host          text NOT NULL DEFAULT '',      -- line PC hostname
    usb_location  text,                          -- stable USB topology id (NOT the COM name)
    UNIQUE (host, label)
);

CREATE TABLE work_order (
    work_order_id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    firmware_id   uuid NOT NULL REFERENCES firmware(firmware_id),
    erp_order_ref text,                          -- link back to the ERP production order
    planned_qty   integer NOT NULL DEFAULT 0,
    status        text    NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed')),
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE unit (
    imei             text PRIMARY KEY,           -- natural key
    product          text,
    first_flashed_at timestamptz,
    last_flashed_at  timestamptz
);

-- ---------------------------------------------------------------------------
-- Event data: one row per flashed unit (mapped 1:1 from the JSON line)
-- ---------------------------------------------------------------------------

CREATE TABLE flash_record (
    record_id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    ts                timestamptz NOT NULL,          -- from ts_ms
    station_id        uuid REFERENCES station(station_id),
    work_order_id     uuid REFERENCES work_order(work_order_id),
    firmware_id       uuid REFERENCES firmware(firmware_id),
    imei              text REFERENCES unit(imei),    -- nullable if the unit never booted
    port              text,
    result            text    NOT NULL CHECK (result IN ('pass', 'fail')),
    attempts          integer NOT NULL CHECK (attempts >= 1),
    bytes             bigint  NOT NULL DEFAULT 0,
    flash_seconds     double precision NOT NULL DEFAULT 0,
    total_seconds     double precision NOT NULL DEFAULT 0,
    firmware_readback text,
    error             text,
    -- idempotency: a replayed log line never double-counts
    UNIQUE (station_id, ts)
);

CREATE INDEX ix_flash_record_ts       ON flash_record (ts);
CREATE INDEX ix_flash_record_imei     ON flash_record (imei);
CREATE INDEX ix_flash_record_result   ON flash_record (result);
CREATE INDEX ix_flash_record_wo       ON flash_record (work_order_id);

-- Optional per-phase breakdown (roadmap; one row per phase per record)
CREATE TABLE phase_timing (
    record_id uuid NOT NULL REFERENCES flash_record(record_id) ON DELETE CASCADE,
    phase     text NOT NULL,   -- fdl1 | fdl2 | partitions | format | verify
    seconds   double precision NOT NULL,
    PRIMARY KEY (record_id, phase)
);

-- ---------------------------------------------------------------------------
-- KPI views
-- ---------------------------------------------------------------------------

-- First-Pass Yield: passed on the first attempt / distinct units seen.
CREATE VIEW v_first_pass_yield AS
SELECT
    work_order_id,
    count(*) FILTER (WHERE result = 'pass' AND attempts = 1)::numeric
        / NULLIF(count(DISTINCT imei), 0)                       AS fpy,
    count(*) FILTER (WHERE result = 'pass')                     AS passed,
    count(*)                                                    AS attempts_total
FROM flash_record
GROUP BY work_order_id;

-- Throughput + cycle time per station.
CREATE VIEW v_station_throughput AS
SELECT
    s.label,
    date_trunc('hour', fr.ts)                                   AS hour,
    count(*) FILTER (WHERE fr.result = 'pass')                  AS good_units,
    avg(fr.total_seconds)                                       AS avg_cycle_s,
    avg(fr.flash_seconds)                                       AS avg_flash_s
FROM flash_record fr
JOIN station s USING (station_id)
GROUP BY s.label, date_trunc('hour', fr.ts);

-- Defect Pareto: rank failure reasons.
CREATE VIEW v_defect_pareto AS
SELECT
    coalesce(error, 'unknown')                                 AS defect,
    count(*)                                                    AS n,
    round(100.0 * count(*) / sum(count(*)) OVER (), 1)         AS pct
FROM flash_record
WHERE result = 'fail'
GROUP BY coalesce(error, 'unknown')
ORDER BY n DESC;

-- Per-unit traceability: every firmware an IMEI has ever run.
CREATE VIEW v_unit_traceability AS
SELECT
    fr.imei,
    fr.ts,
    f.product_name,
    f.product_version,
    f.pac_sha256,
    fr.firmware_readback,
    fr.result
FROM flash_record fr
LEFT JOIN firmware f USING (firmware_id)
WHERE fr.imei IS NOT NULL
ORDER BY fr.imei, fr.ts;

-- ---------------------------------------------------------------------------
-- Landing zone: the raw JSON-lines events, ingested 1:1 (see tools/mes_ingest.py).
-- This is what the flasher writes; the normalized star schema above is the
-- warehouse it feeds. Idempotent on (station, ts_ms) so replays never dup.
-- ---------------------------------------------------------------------------

CREATE TABLE flash_event (
    station        text        NOT NULL,
    ts_ms          bigint      NOT NULL,
    ts             timestamptz GENERATED ALWAYS AS (to_timestamp(ts_ms / 1000.0)) STORED,
    work_order     text,
    operator       text,
    port           text,
    product        text,
    pac            text,
    result         text        NOT NULL CHECK (result IN ('pass', 'fail')),
    attempts       integer     NOT NULL,
    bytes          bigint      NOT NULL DEFAULT 0,
    flash_seconds  double precision NOT NULL DEFAULT 0,
    total_seconds  double precision NOT NULL DEFAULT 0,
    phases         jsonb       NOT NULL DEFAULT '[]',
    firmware       text,
    imei           text,
    error          text,
    PRIMARY KEY (station, ts_ms)
);

CREATE INDEX ix_flash_event_ts     ON flash_event (ts);
CREATE INDEX ix_flash_event_imei   ON flash_event (imei);
CREATE INDEX ix_flash_event_wo     ON flash_event (work_order);

-- Runnable KPIs straight off the landing zone (no ETL needed to get started).

-- First-Pass Yield per work order.
CREATE VIEW v_event_fpy AS
SELECT
    work_order,
    count(*) FILTER (WHERE result = 'pass' AND attempts = 1)::numeric
        / NULLIF(count(DISTINCT coalesce(imei, station || ts_ms::text)), 0) AS fpy,
    count(*) FILTER (WHERE result = 'pass')                                 AS passed,
    count(*)                                                                AS attempts_total
FROM flash_event
GROUP BY work_order;

-- Throughput + average phase breakdown per station-hour.
CREATE VIEW v_event_throughput AS
SELECT
    station,
    date_trunc('hour', ts)                                     AS hour,
    count(*) FILTER (WHERE result = 'pass')                    AS good_units,
    avg(total_seconds)                                         AS avg_cycle_s,
    avg(flash_seconds)                                         AS avg_flash_s
FROM flash_event
GROUP BY station, date_trunc('hour', ts);

-- Average seconds per phase (line balancing), unnested from the JSONB.
CREATE VIEW v_event_phase_avg AS
SELECT
    p ->> 'phase'                          AS phase,
    round(avg((p ->> 'seconds')::numeric), 2) AS avg_seconds,
    count(*)                               AS samples
FROM flash_event, jsonb_array_elements(phases) AS p
GROUP BY p ->> 'phase'
ORDER BY avg_seconds DESC;

-- Defect Pareto off the landing zone.
CREATE VIEW v_event_defects AS
SELECT
    coalesce(error, 'unknown')                          AS defect,
    count(*)                                            AS n,
    round(100.0 * count(*) / sum(count(*)) OVER (), 1)  AS pct
FROM flash_event
WHERE result = 'fail'
GROUP BY coalesce(error, 'unknown')
ORDER BY n DESC;
