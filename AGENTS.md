# AGENTS.md

## Skills

- **rust-best-practices** — руководства по идиоматичному, эффективному и поддерживаемому Rust-коду.
  Всегда загружайте этот скилл (Skill tool, `name: "rust-best-practices"`) перед написанием, изменением или ревью любого Rust-кода в этом проекте: `src/`, `build.rs`, тесты.
  Расположение: `~/.agents/skills/rust-best-practices/SKILL.md`.

## Quality Gates

Перед завершением любой задачи по коду проверяйте:

```bash
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test
