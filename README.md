# claude-md-stream

Renders a running Claude Code session as a Markdown stream, so a terminal pane
can show what the agent is doing without showing the agent's TUI. Input goes the
other way, from any editor buffer into the same session, so the pane you read
and the pane you type in are separate windows onto one agent.

It reads the transcript Claude Code already writes. No API traffic, no second
client, no automation of the CLI: the session stays an ordinary interactive
agent you can drop into at any time.

Requires [herdr](https://herdr.dev) to find the session behind a pane and to
deliver prompts to it.

## Install

```sh
nix run github:tophcodes/claude-md-stream -- tail <agent>
```

Or as a flake input, which gives you both `claude-md-stream` and `claude-send`:

```nix
inputs.claude-md-stream.url = "github:tophcodes/claude-md-stream";
```

## Reading

```sh
claude-md-stream tail <agent|session-id> [--no-follow] [--no-sidechains] [--max-result-lines N]
```

The target is whatever `herdr agent list` knows the pane as: its name, `pane_id`,
`tab_id` or `workspace_id`. A session UUID works too, for a session with no pane.

Output is Markdown on stdout, appended forever. `--no-follow` renders what exists
and exits.

## Writing

```sh
echo "what is in flake.nix" | claude-send --stdin ~/.local/state/claude-md-stream/<agent>/input.md
```

`claude-send` takes the text on stdin and reads the agent's name from the
directory the buffer lives in. Pipe rather than point it at the file: an editor
whose save completes asynchronously will still be writing when the reader looks.

Every accepted send is appended to `sent.jsonl` beside the buffer. A queued
prompt reaches the transcript only when the turn that consumes it begins, which
can be minutes later, so the viewer tails that log and shows what you sent the
moment it goes out.

### From an editor

Helix, bound so that a stray `:w` cannot fire a prompt:

```toml
[keys.normal.space]
ret = [
  "select_all",
  ":pipe-to claude-send --stdin '%{buffer_name}'",
  "delete_selection",
  ":write!",
]
```

Any editor that can pipe a buffer to a command works the same way. Nothing about
Helix reaches into the tool.

## The format

Assistant text is plain Markdown and your prompts are blockquotes. Everything
else is a fenced block whose info string names the component:

````markdown
---
session: 2e0e8f4d-10bc-4648-a03d-e4816afb411b
cwd: /home/toph/infra
model: claude-opus-5
---

<!-- claude at=2026-09-12T09:14:02Z uuid=f54c8823 thread=main kind=prompt -->
> render the session as markdown

<!-- claude at=2026-09-12T09:14:12Z uuid=697896a7 thread=main kind=tool -->
```claude:tool id=toolu_01AB name=Bash
{
  "command": "herdr agent list"
}
```

<!-- claude at=2026-09-12T09:14:13Z uuid=e5059efb thread=main kind=result -->
```claude:result for=toolu_01AB status=ok lines=42
{"agents": [ … ]}
```
````

Components: `claude:thinking`, `claude:tool`, `claude:result`, `claude:sent`,
`claude:meta`. A renderer that knows none of them shows code blocks, which is
why the stream stays readable with `glow`, `bat`, on GitHub, or in a bare pane.

Three rules make it safe to build on:

- **Fence length is derived from the body**, one backtick longer than the longest
  run inside it. CommonMark closes a fence only with a run at least as long, so
  no content can break out of its block.
- **The anchor comment carries the machine-readable fields**, invisible in a
  rendered view. `thread` is `main` or a subagent id, on every unit: several
  subagents run at once and their lines interleave, so a frontend that wants
  them apart reconciles by that field rather than by position.
- **An unknown transcript line type is dropped, not an error.** The envelope
  gains types between Claude Code releases. Content is read from
  `message.content`, which is the public API message shape; an unknown block
  type there surfaces as `claude:meta kind=unknown-block` instead of vanishing.

`claude:meta kind=status` reports `working` and `idle` as herdr sees them. The
transcript says nothing while a message is being written, so without it a pane
looks dead for as long as the agent thinks.

## Not here

Token-level streaming. Partial messages exist only in the terminal, behind TUI
chrome with no stable inner layer under it, and scraping that would put a parser
on a layout nobody holds still. `--include-partial-messages` gives real token
streaming, but only for a headless `-p` session, which is no longer an
interactive agent you can take over.

## Licence

MIT
