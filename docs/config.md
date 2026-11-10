# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Accessibility

Two switches control how ByteAsk's own surfaces render. Neither is auto-detected.

**Reduced motion** — `tui.animations = false` in `config.toml`. Spinners and the shimmer
become static text; nothing repaints just to move.

```toml
[tui]
animations = false
```

**Screen reader mode** — ByteAsk's readouts print as linear, announce-once text instead of a
pane, spinners are static, and the `✓ ✗ › ▌ █ ░ ·` glyphs fall back to ASCII (`OK`, `x`, `>`,
`|`, `#`, `-`, `-`). Three ways to turn it on, highest precedence first:

| Source | How |
|---|---|
| Flag | `byteask --ax-screen-reader` |
| Environment | `BYTEASK_AX_SCREEN_READER=1` (`BYTEASK_PLAIN_MENU=1` is an older alias) |
| Config | `tui.screen_reader = true` |

```toml
[tui]
screen_reader = true
```

`BYTEASK_AX_SCREEN_READER=0` turns the mode off even when `tui.screen_reader = true`, so one
environment variable overrides a machine's settings for a single run. When the mode comes from
the flag or the environment, the first line ByteAsk prints is
`[Screen reader mode: on via flag]` (or `via env`), before the terminal is claimed.

ASCII glyphs are also used, on their own, whenever `LC_ALL`/`LC_CTYPE`/`LANG` name a non-UTF-8
locale.

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.
