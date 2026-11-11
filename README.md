# wmux

A small tmux-style terminal multiplexer for Windows, built on ConPTY.

## Build and run

```
cargo build --release
cargo install --path .                 # put wmux.exe in ~/.cargo/bin (on PATH)

wmux                                   # new session (named 0, 1, ...) and attach
wmux new -s work                       # new session called "work"
wmux new -s logs -d pwsh -NoLogo       # start detached, with a specific command
wmux ls                                # list sessions
wmux attach -t work                    # attach (aliases: a, at; no -t = newest)
wmux kill-session -t work
wmux kill-server                       # stop all sessions
```

Set `WMUX_SHELL` to change the default shell (default: pwsh, else powershell, else cmd).
Set `WMUX_LOG=<file>` for a debug trace from client and server.
Run `wmux kill-server` before you rebuild: a running server locks `wmux.exe`.

## How sessions work

`wmux new` starts `wmux __server <name>` as a background process with no console
(`DETACHED_PROCESS`), so it lives on when you close the terminal. The server owns
the shells and listens on an AF_UNIX socket in `%LOCALAPPDATA%\wmux\<name>.sock`
(the same idea as tmux's `/tmp/tmux-UID/`). The client sends keys and resizes and
writes the frames it gets back. With more than one client, the session takes the
size of the client that typed last (tmux `window-size latest`).

## Keys (prefix = Ctrl-b, same as tmux defaults)

| Key | Action |
|---|---|
| `%` or `\|` | split left/right |
| `"` or `-` | split top/bottom |
| arrows | move to the pane in that direction (repeatable for 500 ms) |
| Ctrl+arrows | resize pane (repeatable) |
| `o` / `;` | next pane / last pane |
| `z` | zoom pane |
| `x` | kill pane |
| `c` | new window |
| `n` / `p` / `l` / `0-9` | next / previous / last / numbered window |
| `,` | rename window |
| `&` | kill window |
| `[` or PgUp | copy mode (see below) |
| `]` | paste the last copied text |
| Ctrl-b | send a literal Ctrl-b |
| `d` | detach |
| `?` | key help in the status bar |

The session ends when its last shell exits.

### Copy mode (`Ctrl-b [`)

Move a cursor through the pane and its scrollback, select text, and copy it.

| Key | Action |
|---|---|
| arrows / `h` `j` `k` `l` | move the cursor (scrolls the view at the edge) |
| `0` / `$` / Home / End | start / end of line |
| `g` / `G` | oldest line / live view |
| PgUp `b` / PgDn `f` / Ctrl-u / Ctrl-d | page / half-page |
| `Space` or `v` | start or clear a selection |
| `y` or Enter | copy (selection, or the cursor line) and leave |
| `q` or Esc | leave |

A copy goes to the Windows clipboard and to an internal buffer that `Ctrl-b ]`
pastes. Paste from another program uses the terminal's own paste (bracketed
paste is passed through).

## Layout

| File | Job |
|---|---|
| `src/main.rs` | CLI: new, attach, ls, kill-session, kill-server |
| `src/session.rs` | socket paths, session list, stale socket cleanup, start server |
| `src/proto.rs` | wire format `[tag][len][payload]`, key serialization |
| `src/client.rs` | raw mode; keys -> socket; frames -> stdout |
| `src/server.rs` | event loop, prefix state machine, windows, borders, status bar, clients |
| `src/conpty.rs` | ConPTY: pipes, CreatePseudoConsole (flags 0), CreateProcessW |
| `src/pane.rs` | one shell: ConPTY + vt100 emulator + reader/waiter threads, answers DSR/DA |
| `src/layout.rs` | binary split tree, rectangle math |
| `src/keys.rs` | key events -> VT input bytes |
| `src/render.rs` | cell grid, per-client diff against the last frame |
| `src/clip.rs` | write text to the Windows clipboard (CF_UNICODETEXT) |
| `examples/drive.rs` | end-to-end test: runs clients in pseudo consoles, types keys, checks screens |

## Not built yet

- Mouse support, config file.
- Choose-tree / switch session from inside (`Ctrl-b s`).
