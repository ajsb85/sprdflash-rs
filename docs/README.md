<!--
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>
-->

# SPRD Flash Tool — Documentation

Reference material and the proposed data contract between the flasher and the
plant's **ERP / MES**.

| Document | What it covers |
|----------|----------------|
| [erp-mes-data-model.md](erp-mes-data-model.md) | Entities, ERD, KPIs, and the ingestion flow from the tool to the MES/ERP |
| [reports.md](reports.md) | The reports the tool produces and the KPIs the MES derives from them |
| [schemas/unit-record.schema.json](schemas/unit-record.schema.json) | JSON Schema for the per-unit record the tool emits (`--records`) |
| [schemas/mes-postgres.sql](schemas/mes-postgres.sql) | PostgreSQL schema: normalized warehouse + a `flash_event` landing zone with runnable KPI views |
| [schemas/sample-units.jsonl](schemas/sample-units.jsonl) | Example JSON-lines output |
| [grafana/](grafana/) | Importable Grafana dashboard + Prometheus alert rules for the live `--metrics-addr` exporter |
| [../tools/mes_ingest.py](../tools/mes_ingest.py) | Dependency-free reference ingester (JSON-lines → idempotent UPSERT SQL) |

## The one thing to know

The tool emits **one JSON object per flashed unit** to the file given by
`--records` (append-only, JSON Lines). That record is the atomic event the whole
data model is built on — it is traceable (IMEI + firmware read back from the
booted device), self-describing, and stable. Everything in the ERP/MES is a
rollup of these events.

```
sprdflash line fw.pac --station f1:COM12 --station f2:COM22 \
    --format --work-order WO-2026-0142 --operator alice \
    --records /var/log/flash/units.jsonl
                         └── one JSON line per unit ──► MES ingest ──► ERP KPIs
```

## Runnable loop (landing zone)

The [ingester](../tools/mes_ingest.py) needs no packages — it turns the JSON
lines into idempotent `UPSERT`s for the `flash_event` table, which you pipe
straight into psql:

```
psql "$DATABASE_URL" -f docs/schemas/mes-postgres.sql        # once: schema + views
python tools/mes_ingest.py units.jsonl | psql "$DATABASE_URL"  # load (re-runnable)
psql "$DATABASE_URL" -c 'SELECT * FROM v_event_fpy'            # KPIs, immediately
```

`v_event_fpy`, `v_event_throughput`, `v_event_phase_avg`, and `v_event_defects`
give first-pass yield, throughput, per-phase line-balancing, and the defect
Pareto with zero ETL. The normalized star schema above is the warehouse it feeds.

Running the line unattended with `sprdflash line … --loop` appends to the same
JSONL all shift. Because the ingester is idempotent (keyed on `station, ts_ms`),
you can re-run it on the growing file on a timer — or tail it with any log
shipper — to load new units incrementally without double-counting.
