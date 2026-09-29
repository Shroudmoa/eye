# eye — Terminal Session Recorder

`eye` records PTY sessions as structured, versioned, replayable event data
(not video). The recording is the source of truth; the terminal is just one
way to look at it. Replay, inspect, search, and export are all powered by the
same event stream.

## Milestone

```bash
# Record a session (default: record to ~/.local/share/eye/session-YYYY-MM-DD.eye)
eye

# Use terminal normally, then exit → recording stops

# Replay the exact session
eye replay session.eye
```

## Architecture

The project is split into clean layers:

| Layer | Responsibility |
|---|---|
| **CLI** | `src/cli.rs` — clap derive, argument parsing, default-subcommand indirection |
| **Session** | `src/session.rs` — `Session` value answering inspect/replay/export/search; `Item::{Output,Resize}` |
| **Recorder** | `src/recorder/` — PTY proxy loop, event emission order, signal handling |
| **Event Model** | `src/event/` — `Event` enum + `define_events!` macro, `Bytes`, `Stream`, `CommandSource` |
| **Storage** | `src/storage/` — crash-safe `.part` → `.eye` via `fsync` + `rename`; `SessionWriter`/`SessionReader` |
| **PTY Handling** | `src/pty.rs` — `posix_openpt`, `setsid`, `TIOCSCTTY`, raw `libc` only; no `nix`/`portable-pty` |
| **Replay Engine** | `src/replay/` — single-threaded `poll()` loop, `Clock`, `ReplayOptions`, `StopReason` |
| **Terminal Renderer** | `src/term/` — full VT model (grid/parser/attrs), `Screen` model feeding HTML export, `--screen` search, replay teardown |
| **Report** | `src/report/` — inspect/list/search/export layer, four non-terminal views sharing `Session` |
| **TTY** | `src/tty.rs` — `RawMode`, window size, `is_tty`, signalfd + blocked masks |

### Key Design Decisions

- **`.eye` format = JSONL**: `meta` header line + one JSON object per event. Streaming-friendly, crash-tolerant.
- **`data` payloads base64-encoded** — PTY traffic is arbitrary bytes; byte fidelity is the point.
- **Explicit `null`s** instead of `skip_serializing_if` — stable shape, avoids serde attribute conflicts.
- **Forward compatibility**: `Event` enum open with `Unknown(Box<RawValue>)` preserving unknown tags byte-exactly, generated from `define_events!`.
- **PTY via raw `libc`**: `posix_openpt`, `setsid`, `TIOCSCTTY`; no `nix`, no `portable-pty`.
- **`signalfd` + blocked signal mask** instead of async-signal handlers; child gets empty mask.
- **Single-threaded `poll()` loop, no async runtime** (both recorder and replay).
- **`OPOST` disabled** during record/replay so bytes written equal bytes recorded.
- **`ISIG` off on the real tty** so Ctrl-C reaches the child as byte 0x03.
- **Storage**: `.eye.part` written live, `fsync` + `rename` to `.eye` only on clean finish; default `session-YYYY-MM-DD.eye` auto-uniquified.
- **Session dir**: `$EYE_DIR` → `$XDG_DATA_HOME/eye` → `$HOME/.local/share/eye`.

## Commands

| Command | Description |
|---|---|
| `eye` (no args) | Default: record a session. Shows subtle banner (`watching terminal`, `recording: session-2026-09-29.eye`). |
| `eye record <file>` | Record a session to the given file (or `.eye` in the session dir). |
| `eye replay <file>` | Replay a recorded session. Controls: `Space` pause, `←/→` seek, `+`/`-` speed, `r` restart, `q` quit. Flags: `--speed`, `--start`, `--progress`. |
| `eye inspect <file>` | Structured timeline with `00:00.000` offsets; command/stderr/exit/resize rows; filters `--errors`, `--commands`, `--timeline`. |
| `eye list` | List recorded sessions in the session directory, with summaries (duration, commands, failures). |
| `eye export <file>` | Export to `--format json\|text\|html\|asciinema`; JSON/text/asciinema first, adding more straightforward. |
| `eye search` | Search byte stream or terminal screen for a query; `--screen` uses the VT model; `-c` case-sensitive; `-C` context lines. |

## Format

The `.eye` file is JSONL:

```
{"type":"meta","meta":{...}}
{"type":"output","t":0.0,"data":"base64...","stream":"tty"}
{"type":"command","t":1.0,"id":1,"text":"ls","source":"echo"}
{"type":"exit","t":1.5,"id":1,"code":Some(0),"signal":None}
{"type":"resize","t":3.0,"size":{"cols":80,"rows":24}}
{"type":"signal","t":4.0,"name":"SIGWINCH","number":28}
{"type":"mark","t":5.0,"label":"user","note":None}
{"type":"end","t":6.2,"reason":"shell_exit","code":Some(0)}
```

- **Versioned from day one**: new event types must not break old recordings (unknown types preserved via `Unknown(Box<RawValue>)`).
- **`data` payloads base64-encoded** (`Bytes` newtype) — PTY traffic is arbitrary bytes; byte fidelity is the point.
- **`CommandSource::Echo`/`Integration** — shell integration (OSC 133) is authoritative; echo reconstruction is the universal fallback.

## Installation

```bash
# From source (requires Rust 1.98.1+)
git clone https://github.com/...
cd eye
cargo build --release   # or `cargo build` for debug

# Or install binary if available
```

## Usage Examples

```bash
# 1. Record a session (default writes to ~/.local/share/eye/session-2026-09-29.eye)
eye

# 2. Record to a specific file
eye record ~/my-session.eye

# 3. Replay a session
eye replay ~/my-session.eye

# 4. Replay with speed control and start offset
eye replay ~/my-session.eye --speed 2.0 --start 30.0

# 5. Inspect a session (timeline view)
eye inspect ~/my-session.eye

# 6. Inspect with filters
eye inspect ~/my-session.eye --errors --timeline

# 7. List sessions
eye list

# 8. Export as JSON
eye export ~/my-session.eye -f json -o ~/my-session.json

# 9. Export as asciinema v2 cast
eye export ~/my-session.eye -f asciinema -o ~/my-session.cast

# 10. Search for "error" in raw byte stream
eye search "error"

# 11. Search on terminal screen only
eye search "error" --screen
```

## Development

```bash
# Run tests
cargo test --lib

# Run all tests (including integration)
cargo test

# Build in release mode
cargo build --release
```

### Adding a new event type

1. Add variant to `src/event/model.rs`.
2. Add `#[derive(PartialEq)]` and any new fields.
3. Run the `define_events!` macro (or update it) to emit the JSON tag.
4. Add serialization/deserialization logic in `src/event/serialize.rs` if needed.
5. Emit the event in the recorder (`src/recorder/mod.rs`).
6. Add handling in replay/inspect/export as needed.

## License

MIT OR Apache-2.0 (at your choice) — see `LICENSE` for details.