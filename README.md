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

## Config file

wmux reads a tmux.conf-style file at `%WMUX_CONFIG%`, else `%USERPROFILE%\.wmux.conf`,
once when a session's server starts. One command per line; `#` starts a comment.
A full, commented sample is in [`wmux.conf.sample`](wmux.conf.sample).

```
set prefix C-a              # prefix key (default C-b); C-a, M-x, a bare char
set repeat-time 300         # ms an arrow repeat stays live after the prefix
set history-limit 50000     # scrollback lines per pane
set default-shell pwsh -NoLogo
set status-bg 4             # status bar colours: 0-255, colourN, or a name
set status-fg 7
set message-fg 7
set message-bg 1
set mouse on                # click panes, wheel-scroll, drag-select (default off)
bind | split-window -h      # rebind a prefix key to an action
bind _ split-window -v
unbind '"'
```

Bindable actions: `split-window -h|-v` (or `split-h`/`split-v`), `next-pane`,
`last-pane`, `kill-pane`, `kill-window`, `new-window`, `next-window`,
`previous-window`, `last-window`, `select-window N`, `zoom`, `copy-mode`,
`paste`, `rename-window`, `detach`, `choose-session`, `list-keys`.

### Using an existing `.tmux.conf`

Point `WMUX_CONFIG` at your `~/.tmux.conf` and it mostly works as a drop-in.
wmux applies what it understands — `prefix`, `mouse`, `history-limit`,
`repeat-time`, `status-style`/`message-style` (the `fg=`/`bg=` parts), and the
bindable actions above (including `bind` with `-r`, and `unbind -a`) — and
silently ignores the rest: `mode-keys`, `status-left`, plugins, `source-file`,
`run-shell`, directional `resize-pane` binds, non-prefix key tables (`-n`,
`-T`), and any option or command it does not recognise. `set-option`/`setw` and
a leading `-g`/`-a` are accepted. Only a malformed value for a setting wmux
*does* support (a bad key, number, or colour) is reported, in the status bar;
the rest of the file still applies.

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
| `s` | choose another session to attach to |
| Ctrl-b | send a literal Ctrl-b |
| `d` | detach |
| `?` | key help in the status bar |

The session ends when its last shell exits.

### Mouse (`set mouse on`)

Off by default, so the terminal's own selection keeps working. With it on:
click a pane to focus it, wheel up to enter copy mode and scroll, and drag to
select then release to copy.

### Choose a session (`Ctrl-b s`)

Lists the live sessions; arrows or `j`/`k` move, Enter attaches the client to
the chosen one, `q`/Esc cancels. The session you leave keeps running.

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
| `src/client.rs` | raw mode; keys/mouse -> socket; frames -> stdout; reconnect loop for session switching |
| `src/server.rs` | event loop, prefix state machine, windows, borders, status bar, clients, copy mode, mouse, chooser |
| `src/conpty.rs` | ConPTY: pipes, CreatePseudoConsole (flags 0), CreateProcessW |
| `src/pane.rs` | one shell: ConPTY + vt100 emulator + reader/waiter threads, answers DSR/DA |
| `src/layout.rs` | binary split tree, rectangle math |
| `src/keys.rs` | key events -> VT input bytes |
| `src/render.rs` | cell grid, per-client diff against the last frame |
| `src/clip.rs` | write text to the Windows clipboard (CF_UNICODETEXT) |
| `src/config.rs` | parse `~/.wmux.conf`: prefix, options, colours, key bindings |
| `examples/drive.rs` | end-to-end test: runs clients in pseudo consoles, types keys, checks screens |

## Not built yet

- Copy-mode block selection, searching the scrollback.
- Resizing panes or switching the active pane by mouse drag.
- Session persistence across a reboot.
