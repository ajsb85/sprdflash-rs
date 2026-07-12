<!--
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>
-->

# Grafana + Prometheus for the flash line

Live yield/throughput monitoring for `sprdflash line … --metrics-addr`. The
exporter serves the Prometheus text format at `/metrics`; point Prometheus at it,
import the dashboard, and (optionally) load the alert rules.

| File | What it is |
|------|------------|
| [sprdflash-dashboard.json](sprdflash-dashboard.json) | Importable Grafana dashboard (FPY, throughput, retries, bytes, uptime) |
| [sprdflash-alerts.yml](sprdflash-alerts.yml) | Prometheus alerting rules (exporter down, low yield, all-failing, retry spike) |

## 1. Serve metrics

```
sprdflash line firmware.pac --station f1:COM12 --station f2:COM22 \
    --loop --metrics-addr 0.0.0.0:9184
# -> http://<host>:9184/metrics
```

## 2. Scrape it

Add a job to `prometheus.yml` (the job name **must** be `sprdflash` for the alert
rules' `up{job="sprdflash"}` to match):

```yaml
scrape_configs:
  - job_name: sprdflash
    scrape_interval: 15s
    static_configs:
      - targets: ["line-host-1:9184", "line-host-2:9184"]

rule_files:
  - sprdflash-alerts.yml
```

## 3. Import the dashboard

Grafana → Dashboards → **New → Import** → upload `sprdflash-dashboard.json`, then
pick your Prometheus datasource. The panels use `rate(...)` over 5–10 min
windows, so they populate once a run is producing units.

## Exposed metrics

| Metric | Type | Meaning |
|--------|------|---------|
| `sprdflash_units_total{result="pass"\|"fail"}` | counter | Units flashed, by result |
| `sprdflash_retries_total` | counter | Retry attempts beyond the first |
| `sprdflash_bytes_total` | counter | Payload bytes written |
| `sprdflash_flash_seconds_total` | counter | Sum of flash wall-clock seconds |
| `sprdflash_uptime_seconds` | gauge | Exporter uptime |

These are the same events that stream to the `--records` JSONL and land in the
[Postgres KPI schema](../schemas/mes-postgres.sql); Prometheus is the live view,
the warehouse is the historical one.
