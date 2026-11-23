# Contributing to fdt

Thanks for your interest. fdt is in early development, so the most useful help right now is:

- **Bug reports** with steps to reproduce, especially from Windows and Linux, which get less daily
  use than macOS.
- **Measurements** on big apps: where fdt feels slow, with the size of the session (log lines,
  requests, frames).
- **Pull requests** for open issues.

## Before you start

For anything bigger than a small fix, open an issue first and describe what you want to change.
The design and the milestone plan are in
[`docs/superpowers/specs/2026-11-18-flutter-devtools-rs-design.md`](docs/superpowers/specs/2026-11-18-flutter-devtools-rs-design.md),
and some work is already planned in `docs/superpowers/plans/`. Agreeing on the approach first saves
you from rewriting a finished pull request.

## Set up

1. Install Rust with [rustup](https://rustup.rs). The pinned version in `rust-toolchain.toml`
   installs itself on the first build.
2. On Linux, install the build dependencies listed in the [README](README.md#install).
3. Build and run: `cargo run -p fdt_app`.
4. For something to connect to, run the test app in `fixtures/jank_lab` with `flutter run`. It has
   deliberate jank, leaks, failing requests and log bursts.

## Checks

CI runs these on macOS, Windows and Linux, and a pull request has to pass all of them:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Clippy treats warnings as errors. Code that only runs on some systems needs `#[cfg(...)]` gates so
it doesn't leave unused-code warnings on the others; CI catches this, but only after you push.

## Writing code

- Add tests with every change. Most crates test their logic without a window; `fdt_app` has
  test helpers for the UI.
- Keep the UI fast. The performance budgets are in section 10 of the design doc, and the release
  budget tests (`cargo test --workspace --release -- budget`) print what they measure.
- New dependencies must use a licence that `deny.toml` allows. CI checks this with `cargo deny`.
- Text in the UI is short and plain: say what happened and what to do next.

## Commits and pull requests

- Commit messages follow the style already in the history: `feat(vm): …`, `fix(app): …`,
  `docs: …`, with the crate as the scope.
- Keep a pull request to one change, and say in its description how you tested it. For UI
  changes, add a screenshot or a short recording.

## Licence

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE), the same as the rest of the project.
