# Contributing to Nera Cleaner

Thanks for helping. Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) first: it holds the hard rules, and
[docs/DESIGN.md](docs/DESIGN.md) holds the layout rules every screen follows.

## Build and check

```sh
cargo run -p nera-app                       # run the app
cargo test --workspace --exclude nera-app   # unit tests; none touch your real files
cargo clippy --workspace --all-targets
cargo fmt --all
```

CI runs the same checks on macOS and Windows for every pull request. The macOS code cannot be compiled on
Windows and the other way round, so CI is the only proof a change works on the platform you don't have.

## Rules that are not negotiable

1. **Deleting files** goes only through `nera_core::fs_util::{remove_item, trash_item, remove_or_trash}`.
   Never call `std::fs::remove_*` in feature code.
2. **A new cleanup target** needs a reason it is safe: what creates the data, and how the app recreates it.
   If losing it costs the user anything (sign-in, history, downloads), mark it `.review()` so it is never
   pre-selected. Add it to the tests that list what must never be cleaned if it is close to user data.
3. **Numbers must be verifiable.** Say in the pull request which system tool the value was compared with
   (`du`, Finder, Activity Monitor, Task Manager...). Each feature has a `dump_<name>` example for this.
4. **No blocking work on the UI thread**, and layout only from `nera_ui`.
5. **Tests** use temporary folders, never real user data.

## Pull requests

- Keep one change per pull request, with a description of what you tested and on which OS.
- Screens: add a screenshot (light or dark) when the change is visible.
- By contributing you agree that your contribution is licensed under the [Apache License 2.0](LICENSE).

## Reporting problems

- Bugs and ideas: [open an issue](https://github.com/fluxvane/nera-cleaner/issues/new/choose).
- If the app removed something it should not have, or you found a way to make it do so, follow
  [SECURITY.md](SECURITY.md) instead of opening a public issue.
