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
- Keep changes focused; one logical change per merge request.

## Submitting changes

1. Create a branch off `develop`.
2. Make your change with tests where it makes sense.
3. Run the checks above on the supported Rust version.
4. Open a merge request against `develop` describing what and why when remote publication is authorized.

## Releases

Prepare releases on `develop`. Bump the workspace version and internal dependency
versions in the crate manifests, then update `Cargo.lock` and run the checks above.

Publish only a reviewed, secret-scanned public export to GitHub `main`; never push
this checkout's internal history to the public repository. A push to public `main`
starts the release workflow. It reads the workspace version from `Cargo.toml` and
skips publication if `v<version>` already exists. For a new version, audit, tests,
Clippy, config package verification and the Linux build must pass before the
workflow creates the tag at the pushed commit and publishes the archive, checksums
and GitHub Release. Do not push a release tag manually or move an existing tag.

Crates.io publication is separate: authenticate with `cargo login`, then publish
`invocra-config`, `invocra-storage`, `invocra-gitlab`, `invocra-tui`, and `invocra`
in that order. `cargo publish` waits for each uploaded version to appear in the
index. Homebrew updates follow the verified GitHub release.

## Reporting bugs

Open an issue with steps to reproduce, expected vs. actual behaviour, and the
invocra version (`invocra --version`). For security issues see
[SECURITY.md](SECURITY.md) — report those privately.
