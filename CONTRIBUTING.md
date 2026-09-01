# Contributing to Caudra

Caudra is an independent fork maintained at [github.com/caudra/caudra](https://github.com/caudra/caudra). Issues and pull requests belong in that repository.

Keep the project small. Before adding core behavior, consider whether a Lua plugin can provide it. If the plugin needs a missing API, open an issue to discuss the smallest useful surface first. Prefer APIs that match Neovim where that makes plugin code familiar.

Before opening an issue, search open and closed issues for the same problem. Pull requests that use AI should explain how it was used and include prompts when they help reviewers understand the change.

Useful commands live in `justfile`. Run the cheapest relevant checks while iterating, then `just ci` before requesting review.

Development builds omit dependency and vendored C debug information to keep linking practical. Caudra crates retain debug information. To step into one dependency, add `[profile.dev.package.<name>] debug = true` in `Cargo.toml`.

## Review standard

- Keep behavior explicit and code easy to inspect.
- Prefer a small fix over new abstraction.
- Add tests for behavior and failure modes, not implementation trivia.
- Explain non-obvious tradeoffs in the pull request.
- Do not hide permission, persistence, network, or compatibility changes in cleanup commits.

Write commit messages in the style of recent history. Keep them concise and explain why the change exists.
