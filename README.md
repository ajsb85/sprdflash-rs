# SPRD Flash Tool

[![CI](https://github.com/ajsb85/sprdflash-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/ajsb85/sprdflash-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-2024_edition-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)
[![MSRV 1.85](https://img.shields.io/badge/MSRV-1.85-blue.svg)](https://blog.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/platforms-Windows_%7C_Linux-informational.svg)](#build--test)
[![Conventional Commits](https://img.shields.io/badge/Conventional%20Commits-1.0.0-yellow.svg)](https://www.conventionalcommits.org)
[![unsafe: forbidden in core](https://img.shields.io/badge/unsafe-forbidden_in_core-success.svg)](crates/sprdflash-core)

Pure-Rust, cross-platform (Windows 11 + Linux/WSL) flasher for SPRD/UNISOC
`.pac` firmware — RDA8910 / UIS8910 (Air724UG and friends). Built for a
**manufacturing line**: fast, robust, and observable enough to run thousands of
flashes per day across many fixtures.

It is a port of the hardware-verified Python
[`sprdflash`](https://github.com/ajsb85/sprdflash), which reverse-engineered the
full download protocol (PDL first stage → BSL/FDL2 → partitions → cross-SDK
format) byte-for-byte from the vendor tool. This project keeps that protocol in a
**sans-I/O core** so it stays deterministic and fully unit-testable, and layers a
fast, recoverable transport and a station orchestrator on top.

## Why Rust, and where the speed comes from

The wire, not the CPU, is the bottleneck. The levers, biggest first:

1. **`CHANGE_BAUD` to the FDL's maximum.** The reference flasher runs the whole
   6 MB transfer at 115200 (~42 s). FDL2 supports far higher rates; pushing to
   921600–3 M is an **8–25× wire speedup** — the single biggest win.
2. **One station per fixture.** Serial I/O is blocking and latency-sensitive, so
   each fixture gets its own OS thread; throughput scales linearly with fixtures.
3. **Bigger MIDST chunks + `DISABLE_TRANSCODE` (0x21)** on the bulk data phase —
   fewer round-trips, no HDLC escape expansion.
4. **`mmap` the PAC once**, `Arc`-share zero-copy payload slices to every station.

## Robustness (industrial)

- **Sans-I/O core** (`sprdflash-core`): `#![forbid(unsafe_code)]`, no hardware
  needed to test — the protocol is verified against captured vendor bytes.
- Per-phase **timeouts + bounded retries + automatic device recovery** (Windows
  `pnputil` re-enumerate; Linux/WSL `usbip` re-attach / `uhubctl` power-cycle).
- **Pre-flight PAC CRC** — a corrupt image is never flashed.
- **Per-unit JSON-lines records** for MES, `tracing` spans per station,
  Prometheus metrics (yield, throughput, phase timings).
- **Post-flash boot verify** (ATI/IMEI on the AT port), append-only audit log.
- `Cargo.lock` committed, `panic = "abort"` + `overflow-checks = on` in release.

## Workspace

| crate                | status | role                                                          |
|----------------------|--------|---------------------------------------------------------------|
| `sprdflash-core`     | ✅ done | sans-I/O protocol: PAC parse, PDL + BSL framing, checksums, plan |
| `sprdflash-cli`      | ✅ `info`, `list-ports`, `flash` | the `sprdflash` binary |
| `sprdflash-transport`| ✅ done | `serialport` transport, port discovery, beacon-window connect, recovery |
| `sprdflash-flash`    | ✅ done | device driver: PDL→BSL→partitions→format→reset, `CHANGE_BAUD` |
| `sprdflash-line`     | ✅ done | parallel stations, boot-verify (ATI/IMEI), JSON-lines records, metrics |

The core is validated byte-for-byte against the real V4035 PAC and the reference
Python implementation. The full driver is **hardware-verified on a real Air724UG
(RDA8910)**, both a same-SDK reflash and a cross-SDK `--format` change
(LuatOS V4035 ⇄ CSDK V302340) that boots directly on the soft reset with IMEI
and NV intact.

### Measured speed (single device, 6 MB)

| tool / config                | time   | notes                                       |
|------------------------------|--------|---------------------------------------------|
| Python `sprdflash` (528 B)   | ~42 s  | reference                                   |
| `sprdflash-rs` (2048 B)      | ~33 s  | **22% faster**; default                     |

The device is **flash-write-bound** (~186 KiB/s): MIDST chunks above ~2 KB
overflow the FDL2 receive buffer, and the per-frame overhead is already small at
2 KB — so single-device time is near its floor. **Line throughput comes from
running one station per fixture in parallel** (the `sprdflash-line` crate), not
from squeezing a single device.

## Build & test

```
cargo test            # 21 tests, all against captured ground truth
cargo build --release

# one device, auto mode-switch, boots into the new firmware
sprdflash flash --enter-download firmware.pac
# cross-SDK change: format FS + refresh NV (keeps IMEI)
sprdflash flash --enter-download --format firmware.pac
sprdflash info firmware.pac
sprdflash list-ports
```

Targets: `x86_64-pc-windows-msvc` and `x86_64-unknown-linux-gnu` (WSL Ubuntu).

## Running the line

Each `--station` runs on its own thread; they flash and boot-verify in parallel,
stream a JSON-lines record per unit, and report yield + throughput. Stations are
independent, so one bad unit never stalls the line. A wedged download agent is
recovered automatically (`pnputil` re-enumerate on Windows; wire a fixture
power-cycle on Linux) with bounded retries.

```
> sprdflash line firmware.pac \
    --station fixture-1:COM12 --station fixture-2:COM22 --station fixture-3:COM32 \
    --format --records /var/log/flash/units.jsonl

── results ──
  [PASS] fixture-1     41.0s  LuatOS-Air_V4035_...  IMEI 8634880509...
  [PASS] fixture-2     41.3s  LuatOS-Air_V4035_...  IMEI 8634880511...
  [PASS] fixture-3     40.8s  LuatOS-Air_V4035_...  IMEI 8634880514...

3/3 passed in 41.3s  →  ~261 good units/hour at this concurrency
```

Each unit is one JSON line for the MES / audit log:

```json
{"ts_ms":1700000000000,"station":"fixture-1","port":"COM12","product":"UIX8910_MODEM","result":"pass","attempts":1,"bytes":6071296,"flash_seconds":33.2,"total_seconds":41.0,"firmware":"LuatOS-Air_V4035_...","imei":"863488050987562","error":null}
```

A device is flash-write-bound at ~33 s, so throughput scales with fixtures
(~110/hour each) → **thousands/day across a modest bank of stations**.

## Protocol reference

See the Python project's README and `native.py` for the full reverse-engineering
writeup. Key facts encoded in `sprdflash-core`:

- **PDL** (`0525:a4a7`, first stage): `ae`-framed, header and payload are
  **separate writes**; loads `HOST_FDL`/`PDL1`, then hands over to BSL.
- **BSL**: HDLC `0x7e` frames, `type|size` big-endian, **Spreadtrum sum**
  checksum (not CRC) on RDA8910/UIS8910.
- **Cross-SDK `--format`**: erase `FMT_FSSYS "SYSF"` + `FLASH 0` (deduped by
  address), write the **NV template** with a refreshed **CRC-16-ARC** and a
  **sum32**-checked 12-byte `START_DATA`, then `PREPACK`. IMEI/RF-cal live in a
  separate `factorynv` region the format never touches.
