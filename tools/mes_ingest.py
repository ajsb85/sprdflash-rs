#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>
"""Reference MES ingester for SPRD Flash Tool unit records.

Reads the JSON-lines the flasher writes (``sprdflash line --records``) and emits
idempotent UPSERT SQL for the ``flash_event`` landing table (see
``docs/schemas/mes-postgres.sql``). Stdlib only -- pipe the output to psql:

    python tools/mes_ingest.py units.jsonl | psql "$DATABASE_URL"
    tail -f units.jsonl | python tools/mes_ingest.py | psql "$DATABASE_URL"

Idempotent on (station, ts_ms), so replaying a log never double-counts.
"""
from __future__ import annotations

import json
import sys

COLUMNS = [
    "station", "ts_ms", "work_order", "operator", "port", "product", "pac",
    "result", "attempts", "bytes", "flash_seconds", "total_seconds", "phases",
    "firmware", "imei", "error",
]
# Columns updated on conflict (everything except the (station, ts_ms) key).
UPDATE_COLS = [c for c in COLUMNS if c not in ("station", "ts_ms")]


def sql_literal(col: str, rec: dict) -> str:
    """Render one record field as a SQL literal."""
    if col == "phases":
        return f"{quote(json.dumps(rec.get('phases', [])))}::jsonb"
    val = rec.get(col)
    if val is None:
        return "NULL"
    if isinstance(val, bool):
        return "TRUE" if val else "FALSE"
    if isinstance(val, (int, float)):
        return repr(val)
    return quote(str(val))


def quote(s: str) -> str:
    """Single-quote a string literal, doubling embedded quotes."""
    return "'" + s.replace("'", "''") + "'"


def statement(rec: dict) -> str:
    vals = ", ".join(sql_literal(c, rec) for c in COLUMNS)
    sets = ", ".join(f"{c} = EXCLUDED.{c}" for c in UPDATE_COLS)
    return (
        f"INSERT INTO flash_event ({', '.join(COLUMNS)}) VALUES ({vals}) "
        f"ON CONFLICT (station, ts_ms) DO UPDATE SET {sets};"
    )


def main(argv: list[str]) -> int:
    src = open(argv[1], encoding="utf-8") if len(argv) > 1 else sys.stdin
    ok = bad = 0
    print("BEGIN;")
    with src:
        for lineno, line in enumerate(src, 1):
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
                if "station" not in rec or "ts_ms" not in rec:
                    raise ValueError("missing station/ts_ms")
                print(statement(rec))
                ok += 1
            except (json.JSONDecodeError, ValueError) as e:
                bad += 1
                print(f"-- skip line {lineno}: {e}", file=sys.stderr)
    print("COMMIT;")
    print(f"-- ingested {ok} record(s), skipped {bad}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
