<!--
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>
-->

# Contributing to SPRD Flash Tool

Thanks for helping build a manufacturing-grade flasher. This project targets
production reliability, so the bar for merges is high but the rules are simple.

## Workflow: Trunk-Based Development (tbdflow)

We use [Trunk-Based Development](https://trunkbaseddevelopment.com/) via
[`tbdflow`](https://crates.io/crates/tbdflow). `main` is always releasable; work
lands in small, frequent, signed commits.

- **Small change, straight to trunk:**

  ```
  tbdflow commit -t feat -s flash --message "add CHANGE_BAUD lever"
  ```

- **Larger change, short-lived branch (< 1 day):**

  ```
  tbdflow branch --name quick-fix
  # ... work ...
  tbdflow complete --name quick-fix
  ```

- Sync before you start: `tbdflow sync`. Check for conflicting work: `tbdflow radar`.
- The trunk must stay green: every push runs `fmt` + `clippy -D warnings` + tests
  on Windows and Linux.

## Commit messages: Conventional Commits

Follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org).
`tbdflow commit -t <type> -s <scope> -m <subject>` builds them for you.

- **Types:** `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`,
  `chore`.
- **Scopes** map to crates: `core`, `transport`, `flash`, `line`, `cli`, or a
  cross-cutting area (`ci`, `docs`, `release`).
- **Subject:** imperative mood, ≤ 50 characters, no trailing period.
- **Body:** wrap at 72 characters; explain *why*, not *what*.
- Breaking changes: `tbdflow commit --breaking --breaking-description "..."`.

## Signed commits and tags (required)

All commits and release tags are **SSH-signed**. Configure once:

```
git config gpg.format ssh
git config user.signingkey ~/.ssh/id_ed25519.pub
git config commit.gpgsign true
git config tag.gpgsign true
```

Add your public key to your GitHub account (Settings → SSH and GPG keys →
*Signing key*) so commits show **Verified**.

## Code standards

- **Rust 2024 edition**, MSRV **1.85** (pinned in `rust-toolchain.toml`).
- **`cargo fmt --all`** must be clean (`rustfmt.toml` is the source of truth).
- **`cargo clippy --all-targets -D warnings`** must pass — no `#[allow(...)]`
  without a comment justifying it.
- **`cargo test --all`** must pass. Protocol changes need a test against captured
  ground-truth bytes (see `sprdflash-core/src/*.rs` `#[cfg(test)]` modules).
- `sprdflash-core` is **`#![forbid(unsafe_code)]`** — keep all `unsafe` (mmap,
  FFI) in the outer crates and document each block.
- Every source file carries the SPDX header:

  ```rust
  // SPDX-License-Identifier: MIT
  // SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>
  ```

## Pre-flight

```
cargo fmt --all --check
cargo clippy --all-targets --all-features   # with -D warnings
cargo test --all
cargo build --release
```

## Hardware changes

Protocol/driver changes must be verified on a real RDA8910/UIS8910 module (a
same-SDK reflash **and** a cross-SDK `--format` change that boots with IMEI
intact). Note the device, firmware, and measured timing in the PR/commit body.

## Releasing

Maintainers cut releases from `main`:

```
tbdflow commit -t chore -s release --message "release v0.2.0" --tag v0.2.0
```

The tag is SSH-signed; a signed GitHub release is published with a changelog
generated from Conventional Commits (`tbdflow changelog`).
