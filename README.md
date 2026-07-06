# ByteAsk CLI

**ByteAsk** — an AI coding agent for your terminal. Interactive, tool-using, and fast.

## Install

**macOS / Linux**

```sh
curl -fsSL https://code.byteask.ai/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://code.byteask.ai/install.ps1 | iex
```

Also on package managers:

```sh
pip install byteask         # or:  npx @byteask/cli
```

Then run **`byteask`** to start, or sign in first:

```sh
byteask login --email you@company.com
```

## What's in this repo

This is the public distribution home for the ByteAsk CLI:

- **`install.sh` / `install.ps1`** — the installers (also served from `code.byteask.ai`).
- **`byteask` / `byteask.ps1`** — the CLI launcher.
- **Releases** — each [GitHub Release](https://github.com/ByteAsk/cli/releases) ships the
  `byteask-engine` binaries for every supported platform:

  | Platform | Asset |
  |---|---|
  | Linux x86_64 | `byteask-engine-linux-x86_64.gz` |
  | Linux arm64 | `byteask-engine-linux-arm64.gz` |
  | macOS arm64 | `byteask-engine-darwin-arm64.gz` |
  | Windows x86_64 | `byteask-engine-windows-x86_64.exe.gz` |

The installers always fetch the newest published release
(`releases/latest/download/…`), so `byteask --update` and fresh installs get the
latest engine automatically.

## Updating

```sh
byteask --update
```

## License

Proprietary — © ByteAsk. All rights reserved. See [`LICENSE`](LICENSE).
The engine binary bundles third-party open-source software; see
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
