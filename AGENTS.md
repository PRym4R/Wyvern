# AGENTS.md — Wyvern

Lightweight Discord client in Rust + egui 0.31.1 (user tokens). Root:
`/home/hackerman/wyvern`.

## Commands
- Build: `cargo build --release && cp -f target/release/wyvern ./wyvern`.
  The root `wyvern` binary is tracked; a stale copy means the user runs old code.
- Tests: `cargo test` (all green, keep their count in sync with README).
- 0 warnings required.

## Git
- Identity is unset: `git -c user.name='hackerman' -c user.email='hackerman@local' commit ...`.
- Only `main`; no side branches. Never push — the user pushes manually.
- Style: `feat(scope): ...`, `fix(scope): ...`, `docs(...)`.

## Conventions
- Code comments: English, brief ("why" first). User-facing strings: English.
- Test messages (`assert!` / `eprintln!`) and debug logs (`push_debug`): Russian.
- Docs are Russian; TODO uses `[x]` done, `[ ]` left, `[!]` priority, `(S/M/L)` effort.

## Backups
After the user says "работает" (it works):

```
tar czf /home/hackerman/backups/wyvern-src-<YYYYMMDD>-<shortsha>.tar.gz \
  --exclude=./target --exclude=./wyvern --exclude=./.git -C /home/hackerman/wyvern .
```

## Map
- `src/app.rs` app state; `src/gateway.rs` + `src/gateway/rest.rs` Discord;
  `src/models.rs` structs; `src/ui/*` screens; `src/paths.rs` data/cache paths;
  `src/memcheck.rs` memory test (`--ignored`).
- State files (all overridable) live in the platform data dir
  (`dirs::data_dir()/wyvern`, e.g. `~/.local/share/wyvern`): `settings.json`,
  `deleted.json`, `accounts.json` (vault), `accounts.lock`; logs go to
  `dirs::cache_dir()/wyvern`. Legacy `~/.wyvern_*` files migrate on startup.
- Any change to message height must update `estimate_msg_height` and the geometry
  tests (`src/ui/chat.rs`, `mod geometry_tests`).
