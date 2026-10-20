# Contributing

Thanks for your interest in improving invocra!

## Development

The minimum supported Rust version is 1.88 (see `rust-version` in `Cargo.toml`).

```bash
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo package -p invocra-config --locked
cargo audit
```

## Code style

- **Do not run `cargo fmt`.** This repo is intentionally not rustfmt-clean;
  reformatting produces large, noisy diffs. The quality gate is **Clippy +
  tests**, not formatting. Match the style of the surrounding code.
- Keep changes focused; one logical change per pull request.

## Submitting changes

1. Create a branch off `main`.
2. Make your change with tests where it makes sense.
3. Run the checks above on the supported Rust version.
4. Open a pull request against `main` describing what changed and why.

## Reporting bugs

Open an issue with steps to reproduce, expected vs. actual behaviour, and the
invocra version (`invocra --version`). For security issues see
[SECURITY.md](SECURITY.md) — report those privately.
