# Contributing

Thanks for your interest in doum. Bug reports, fixes and reports from hardware other than a GB10 are all welcome.

## Build and test

You need a recent stable Rust toolchain on Linux.

```sh
cargo build                                   # debug build: target/debug/doum
cargo fmt --all --check                       # formatting (rustfmt.toml: max_width = 140)
cargo clippy --all-targets -- -D warnings     # lints; warnings fail CI
cargo test --all                              # unit tests
```

CI runs the same three checks on x86_64 and aarch64 for every push to `main` and every pull request.

The unit tests need no GPU and no running daemon. To try a change by hand without touching a daemon you already run,
give the test daemon its own config and state directory, and a different port in that config:

```sh
mkdir -p /tmp/doum-dev && printf 'listen = "127.0.0.1:8499"\n' > /tmp/doum-dev/doum.toml
export DOUM_CONFIG=/tmp/doum-dev/doum.toml DOUM_STATE_DIR=/tmp/doum-dev/state
target/debug/doum daemon &
target/debug/doum run -s 4 -- sleep 5
```

`tests/llm_cycle.sh` is a manual end-to-end test against a real llama-swap and a real model. It loads a model on the
GPU, so it is not part of CI; run it only when you mean to.

The demo GIF in `docs/demo.gif` is recorded from a throwaway daemon with stand-in CPU jobs; `docs/demo/README.md`
explains how to record it again.

## Issues

Please include:

- `doum --version`, your distribution, GPU and driver version (and whether the GPU has unified memory),
- what you ran, what you expected and what happened,
- the relevant part of `doum status` (or `doum status --json`), the job's log (`doum logs <id>`), and the daemon's
  log (`journalctl --user -u doum`).

Remove anything private (commands, paths, environment) before posting. For a security problem, please do not open a
public issue; contact the maintainer through GitHub first.

## Pull requests

- Keep each pull request to one change, and say why it is needed.
- Scheduling rules belong in `crates/doum/src/sched.rs` as pure functions; add a unit test for any rule you add or
  change.
- Run the format, lint and test commands above before pushing.
- If you change behaviour, a flag, a config key or a default, update `docs/` (and `docs/config.example.toml`) and add
  a line to `CHANGELOG.md`.

By contributing you agree that your contribution is licensed under the Apache License 2.0, like the rest of doum.
