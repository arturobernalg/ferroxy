# Contributing to Conduit

Thank you for considering a contribution. Conduit operates under a written
[engineering charter](./CHARTER.md) that contributors are signing up for: one trait per real
concept, no speculative generality, hot-path discipline (no per-request allocation, no
`Arc<Mutex<…>>`, no async-fn boxing), and one concern per commit.

## Reporting bugs

File issues at https://github.com/arturobernalg/conduit/issues. Include:

- The Conduit version (`conduit --version`).
- The relevant configuration snippet, with secrets redacted.
- Exact steps to reproduce, including kernel version (`uname -r`) and CPU count for performance
  reports.

For security vulnerabilities, **do not file a public issue** — see the *Security* section of
the [README](./README.md).

## Development setup

### Linux (production target)

```bash
rustup toolchain install stable
rustup component add rustfmt clippy
cargo install --locked cargo-deny cargo-fuzz cargo-criterion cargo-show-asm
git clone https://github.com/arturobernalg/conduit.git
cd conduit
cargo build --workspace --all-targets
cargo test --workspace
```

The default backend on Linux is monoio (`io_uring`, thread-per-core).

### macOS (development only)

macOS is supported for development. Conduit auto-selects the `runtime-tokio` backend on
non-Linux targets — no extra flag is needed to build:

```bash
cargo build
cargo test
```

To explicitly invoke the tokio backend on Linux (e.g. for benchmark comparisons), pass the
`runtime-tokio` feature:

```bash
cargo build --features runtime-tokio
```

See the [Supported Platforms](./README.md#supported-platforms) section of the README for the
full policy. **Production deployments are Linux only.** No release artefacts are published for
macOS; do not run benchmark comparisons on macOS — performance characteristics differ from the
Linux production runtime.

## The quality gate

Every contribution must pass the full local gate before review. CI runs the same gate, with a
build matrix across all supported Linux targets plus a macOS check-only job to prevent the
portable path from rotting. A pull request is not eligible for review until CI is green.

```bash
cargo build   --workspace --all-targets --release
cargo test    --workspace
cargo clippy  --workspace --all-targets -- -D warnings
cargo fmt     --all -- --check
cargo deny    check
```

Hot-path changes additionally require a benchmark in `bench/micro/` showing no regression
against the previous milestone (or a commit message explaining the win). Parser changes require
a `cargo-fuzz` target that has run clean for at least one minute. Concurrent code requires a
`loom` test covering the new synchronisation.

## Pull request expectations

- One concern per commit. Mechanical refactors that incidentally make a behavioural change live
  in their own commit, separate from the behavioural change itself.
- Commit messages are written for the next person reading `git log`, not for the reviewer.
  State the invariant the commit establishes or the rule it enforces; do not narrate.
- Every public type and function has a doc comment. `#![deny(missing_docs)]` enforces this in
  most crates.
- New crates need an entry in `[[bans.deny]]` in `deny.toml` declaring which crates may depend
  on them. The layer-direction rule is mechanical, not editorial.

## What gets merged

Changes that **do** get merged:

- Bug fixes with a regression test.
- Performance improvements with measurements: which benchmark, before/after numbers, the
  hardware they ran on.
- Feature work that lands within a numbered phase of the charter's build plan.
- Documentation improvements, especially around correctness invariants.

Changes that **do not** get merged:

- Speculative abstractions (factories, trait objects, builder patterns) that do not remove
  demonstrable complexity in the same PR.
- Optimisations without numbers.
- Cross-platform fixes that compromise the Linux-first design.
- Style changes mixed with behavioural changes.

## License

By contributing, you agree that your contributions will be licensed under the project's dual
licence (Apache-2.0 OR MIT) as described in the [README](./README.md#license). No additional
agreement is required.
