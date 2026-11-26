# omp-sessions

Fast terminal UI for browsing, filtering, resuming, and cleaning up `omp` sessions, written in Rust with [Ratatui](https://github.com/ratatui/ratatui).

## Features

- **Fast cached loading**: parses `~/.omp/agent/sessions/*/*.jsonl` in a background thread with an atomic metadata cache at `~/.cache/omp-sessions-cache.json` (skips subagent transcripts with `parentSession` and fills missing titles from `history.db`).
- **Live session detection**: inspects `/proc` file descriptors and `~/.omp/agent/terminal-sessions/pts-*` mappings (excluding `__omp_worker_` processes) to flag active sessions (`●`) and prevent accidental deletion.
- **Instant multi-word filtering**: case-insensitive token matching across title, first prompt, project path, session ID, and `live` status.
- **Flexible sorting**: cycle between most recent activity (`date`), project name (`project`), and active-first (`status`).
- **Resume / Restore in place**: changes to the session's project directory and `exec`s `omp --resume <path>` or `omp --restore <path>`.
- **Safe single & batch deletion**: two-step confirmation (`d` twice) with live re-check; removes the `.jsonl` transcript, `.lock.os` file, sibling directory, and matching rows in `history.db` and `transcripts.db` (FTS5 triggers supported via bundled SQLite).
- **Responsive layout**: automatically shows the 8-char `ID` column at terminal width ≥ 120, with ellipsis truncation and scrollbar.

## Requirements

- Linux (uses `/proc` for live-session detection)
- Rust toolchain (Edition 2024, `rustc >= 1.88`)
- `omp` available in `PATH` (for resuming/restoring sessions)

## Build & Install

```bash
# Build and install to ~/bin/omp-sessions
make install

# Or install via Cargo (~/.cargo/bin/omp-sessions)
cargo install --path .
```

To install to a custom directory:

```bash
make install BINDIR=/usr/local/bin
```

## Keybindings

### Table mode

| Key | Action |
| --- | --- |
| `↑` / `k`, `↓` / `j` | Move cursor |
| `PgUp` / `PgDn`, `Ctrl+u` / `Ctrl+d` | Page / half-page scroll |
| `Home` / `g`, `End` / `G` | Jump to top / bottom |
| `/` | Focus filter input |
| `Enter` | Resume selected session (`omp --resume`) |
| `f` | Restore selected session (`omp --restore`) |
| `Space` | Toggle multi-select (`◆`) and advance cursor |
| `d` | Delete highlighted or selected sessions (press twice to confirm) |
| `s` | Cycle sort mode (`date` → `project` → `status`) |
| `r` | Reload sessions from disk |
| `Esc` | Clear filter, or clear selection |
| `q` / `Ctrl+c` | Quit |

### Filter mode

| Key | Action |
| --- | --- |
| `Enter` / `↓` | Return to table with filter applied |
| `Esc` | Clear filter and return to table |
| `←` / `→` / `Home` / `End` / `Ctrl+a` / `Ctrl+e` | Move cursor |
| `Ctrl+w` / `Ctrl+u` | Delete previous word / clear to start of line |
| `Ctrl+c` | Quit |

## Data Paths

- Sessions: `~/.omp/agent/sessions/<project>/*.jsonl`
- History DB: `~/.omp/agent/history.db`
- Transcripts DB: `~/.omp/agent/transcripts.db`
- Active PTY map: `~/.omp/agent/terminal-sessions/pts-*`
- Metadata cache: `${XDG_CACHE_HOME:-~/.cache}/omp-sessions-cache.json`

## Development

```bash
cargo test
cargo build --release
```

## License

[MIT](LICENSE)
