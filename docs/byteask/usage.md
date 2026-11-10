# Usage

## Interactive sessions

```sh
cd your-project
byteask
```

Type a task in plain words. ByteAsk reads the code, runs commands, and shows each file change before applying it.
Useful keys and commands inside a session:

| | |
|---|---|
| `/model` | choose the model and reasoning effort |
| `/usage` | your plan's usage (ByteAsk account) |
| `/login` | switch how models are reached |
| `/review` | review the current changes |
| `/new` | start a new conversation |
| `esc` | stop the current turn, or close a pane |
| `ctrl+c` twice | quit |

Start with a prompt directly: `byteask "fix the failing test in tests/parser"`.
Continue an earlier session with `byteask resume`.

## Scripts and CI

`byteask exec` runs one task without the interface and prints the result:

```sh
byteask exec "add a unit test for parse_header and run it"
```

It runs non-interactively, so set the permissions you want explicitly. Read the Codex CLI documentation on
[non-interactive mode](https://github.com/openai/codex/blob/main/docs/exec.md) for options, which ByteAsk shares.

## C and C++ projects

ByteAsk recognises a C or C++ project from its build files and offers its C/C++ tools to the agent. You do not
call them yourself; ask for the outcome, for example:

- "the release build crashes on startup; find out why" (the agent can run the program under gdb or lldb, or
  open a core file);
- "check this change for data races" (ThreadSanitizer build and run);
- "is the new parser faster?" (benchmark both versions with a noise check);
- "why is this struct 80 bytes?" (layout and padding report).

A compilation database (`compile_commands.json`) gives the tools the exact flags your build uses. If your project
has none, ask ByteAsk to create one; for CMake projects it can generate it.

## Your own key or model

```sh
byteask byok set                      # paste an OpenAI, Anthropic or Gemini key
byteask models add local --ollama      # a model served by Ollama on this machine
byteask models add dgx --url http://your-host:8000/v1 --model your-model
```

Then pick the model in `/model`.

## Updating and removing

```sh
byteask --update
```

To uninstall, delete the `byteask` and `byteask-engine` files the installer reported, and `~/.byteask` if you also
want to remove your settings and sessions. If you installed with pip, run `pip uninstall byteask`.
