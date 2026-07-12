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
| [schemas/mes-postgres.sql](schemas/mes-postgres.sql) | Proposed PostgreSQL schema to ingest records and compute KPIs |
| [schemas/sample-units.jsonl](schemas/sample-units.jsonl) | Example JSON-lines output |

## The one thing to know

The tool emits **one JSON object per flashed unit** to the file given by
`--records` (append-only, JSON Lines). That record is the atomic event the whole
data model is built on — it is traceable (IMEI + firmware read back from the
booted device), self-describing, and stable. Everything in the ERP/MES is a
rollup of these events.

```
sprdflash line fw.pac --station f1:COM12 --station f2:COM22 \
    --format --records /var/log/flash/units.jsonl
                         └── one JSON line per unit ──► MES ingest ──► ERP KPIs
```
