# sprdflash-rs

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
| `sprdflash-cli`      | ✅ `info`, `list-ports` | the `sprdflash` binary |
| `sprdflash-transport`| ⏳ next | `serialport` transport, port discovery, beacon-window connect, recovery |
| `sprdflash-flash`    | ⏳ next | device driver: PDL→BSL→partitions→format→reset, with `CHANGE_BAUD` |
| `sprdflash-line`     | ⏳ next | station pool, work queue, per-unit records, metrics           |

The core is validated byte-for-byte against the real V4035 PAC and the reference
Python implementation (CONNECT/CHANGE_BAUD frames, CRC-16-ARC, sprd-sum, the NV
sum32/CRC, and the erase/marker plan all match captured vendor ground truth).

## Build & test

```
cargo test            # 20 core tests, all against captured ground truth
cargo build --release
sprdflash info  path/to/firmware.pac
sprdflash list-ports
```

Targets: `x86_64-pc-windows-msvc` and `x86_64-unknown-linux-gnu` (WSL Ubuntu).

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
