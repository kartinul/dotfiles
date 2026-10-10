---
name: developer-workflows
description: Build, test, lint, and debug xvpn commands and supervisor workflows.
---

## Common Tasks

### Adding a new CLI command
1. Add variant to `Command` enum in `src/cli.rs`
2. Add handler in `src/commands.rs`
3. Add dispatch arm in `cli.rs::dispatch()`

### Modifying sing-box config generation
Edit `config::singbox_config()` — generates base config for `Scope::Global` or `Scope::Selective`. User rules are carried by `migrate_singbox()`.

### Testing vless parsing
Add test cases to `src/lib.rs` `tests` module covering `parse_link()`.

### Debugging supervisor
Run `make supervise` in foreground — logs to stderr with `[xvpn]` prefix. Or `make logs-supervisor`.