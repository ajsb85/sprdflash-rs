<!--
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>
-->

# Reports

What the SPRD Flash Tool produces, and how the MES/ERP turns it into KPIs.

## 1. Per-unit record (machine, primary)

`sprdflash line … --records units.jsonl` appends **one JSON line per unit**,
conforming to [unit-record.schema.json](schemas/unit-record.schema.json). This is
the only report the tool *persists*, and every KPI is a rollup of it.

```json
{"ts_ms":1783821948614,"station":"station-1","port":"COM34","product":"UIX8910_MODEM","pac":"LuatOS-Air_V4035_...pac","result":"pass","attempts":1,"bytes":6201746,"flash_seconds":33.744,"total_seconds":39.18,"firmware":"LuatOS-Air_V4035_...","imei":"863488050987562","error":null}
```

Properties that make it MES-ready:

- **Traceable** — IMEI and firmware banner are *read back from the booted unit*,
  not assumed from the PAC.
- **Idempotent** — `(station, ts_ms)` de-duplicates replays.
- **Self-describing** — product, PAC, bytes, timings, attempts, error in one row.
- **Streaming-friendly** — append-only JSON Lines; tail with any log shipper.

## 2. End-of-run summary (human, console)

Printed when a `line` run finishes — a shift-lead's at-a-glance view:

```
── results ──
  [PASS] fixture-1     41.0s  LuatOS-Air_V4035_...  IMEI 8634880509...
  [PASS] fixture-2     41.3s  LuatOS-Air_V4035_...  IMEI 8634880511...
  [FAIL] fixture-3     62.1s  -
             error: verify: no ATI response from COM53

2/3 passed in 62.1s  →  ~116 good units/hour at this concurrency
```

Exit code is non-zero if any unit failed, so a line-control script can gate on it.

## 3. KPIs derived by the MES

All computed from `flash_record` (see
[mes-postgres.sql](schemas/mes-postgres.sql) for the views):

| Report | View / query | Consumer |
|--------|--------------|----------|
| First-Pass Yield | `v_first_pass_yield` | Quality, ERP work-order close-out |
| Throughput / cycle time | `v_station_throughput` | Capacity planning, line balancing |
| Defect Pareto | `v_defect_pareto` | Process engineering (top failure modes) |
| Unit traceability | `v_unit_traceability` | RMA, field-return root cause, audit |
| Station availability | `station_id` uptime − recovery | Maintenance |

## 4. Mapping to the classic MES/ERP KPIs

- **FPY / Yield** → quality gate on the ERP production order; a work order only
  closes when `planned_qty` good units exist.
- **Throughput (units/hour)** → the tool prints it live; the MES trends it to
  size the fixture bank (a device is flash-write-bound at ~33 s, so
  `good_units/hour ≈ 3600 / cycle_seconds × stations`).
- **Defect Pareto** → drives corrective action; the `error` strings come straight
  from the driver (`verify: …`, `flash: …`, `open …`), so they are precise.
- **Genealogy / traceability** → for any returned IMEI, `v_unit_traceability`
  lists every firmware and timestamp it ran — the audit trail regulated
  industries require.

## 5. Delivered

- ✅ **Phase timings** (`phases[]` in each record): per-phase seconds — `fdl1`,
  `fdl2`, `partitions`, `format`, `verify` — for line balancing
  (`v_event_phase_avg`).
- ✅ **Operator / work-order stamping**: `--work-order` / `--operator` embed the
  ERP context in every record.
- ✅ **Reference ingester** (`tools/mes_ingest.py`): loads the JSON lines into the
  `flash_event` landing zone; the KPI views make yield/throughput/defects
  queryable with zero ETL.
- ✅ **Prometheus exporter** (`--metrics-addr`): live `sprdflash_units_total`
  (by result), `sprdflash_retries_total`, `sprdflash_bytes_total`, and
  `sprdflash_flash_seconds_total` at `/metrics`, with a ready-to-import
  [Grafana dashboard + alert rules](grafana/) alongside the batch KPIs.
- ✅ **Continuous line mode** (`--loop`): stations flash units in a loop until
  Ctrl-C, so the exporter is live indefinitely and the line runs unattended;
  records stream to the JSONL sink the whole shift.
- ✅ **Post-write read-back verify** (`--verify-readback`): each partition is read
  back off the device and compared byte-for-byte, so a marginal write is caught
  as a `read-back verify failed for …` defect on the line, not in the field.
  Hardware-verified on an Air724UG (RDA8910).

## Roadmap

- **Per-fixture presence tracking** for continuous mode: key device-removal
  detection on each station's USB location so multi-fixture `--loop` lines can
  tell which fixture's unit was swapped.
