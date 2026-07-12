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

## 5. Roadmap

- **Phase timings** (`phase_timing`): per-phase seconds (FDL/partitions/format/
  verify) for finer line balancing.
- **Prometheus exporter**: live `units_total{result=}`, `flash_seconds` histogram,
  `station_up` for Grafana alongside the batch KPIs.
- **Operator / work-order stamping**: pass `--work-order` / `--operator` to embed
  ERP context directly in each record.
