# recycle-rm

The project is named **recycle-rm**; the installed command stays **`rm`** (a drop-in replacement).

> **"Introducing `rm`."**
> **"But `rm` already exists?"**
> **"Exactly. And today, we're reinventing it."**

A **safe, drop-in replacement for `rm`**: the command-line interface is 1:1
compatible with GNU coreutils `rm` (every flag, exit code and error message;
so existing scripts keep working), but removed files are not destroyed. They go
to a **recoverable trash** you can list, restore, compress, and prune.

## Why

`rm` is famously unforgiving: one typo and the data is gone. This tool keeps
the exact behaviour scripts depend on, while giving you an undo.

## Compatibility

> **This is not a port.** No GNU source code is used. This is an independent
> reimplementation that only *reproduces the observable behaviour* of `rm` so
> scripts keep working. The compatibility below is by black-box matching, not
> by sharing code. (See [Status & licensing](#status--licensing).)

- **Drop-in:** all of GNU `rm`'s options (`-f -i -I -r -R -d -v`,
  `--interactive[=WHEN]`, `--one-file-system`, `--preserve-root`/
  `--no-preserve-root`, `--`), the same exit codes, and byte-for-byte identical
  diagnostics (including GNU's shell-escape quoting of odd file names).
- Verified by a differential test that compares output against the real GNU
  `rm`, plus a randomized fuzzer.

## Install

The build produces an executable literally named `rm`. The installer puts it in
front of your PATH so typing `rm` runs this tool, while the real `rm` stays at
`/usr/bin/rm`.

```bash
./install.sh            # per-user (~/.local/bin), no sudo
./install.sh --system   # system-wide (/usr/local/bin), needs root
./install.sh --dry-run  # preview without changing anything
./uninstall.sh          # undo (pass --system if you installed that way)
```

The installer only edits your own shell start-up files (idempotent and
reversible) and never touches system-wide configuration, so machines that
order their PATH on purpose are left as they are. After installing, open a new
shell and check with `type rm` and `rm --version`. Build manually instead with
`cargo build --release` (binary at `target/release/rm`).

## The trash

```bash
rm file.txt              # moved to the trash, not destroyed
rm --trash list          # see what's there (short hex IDs)
rm --trash restore 1     # put it back at its original path
rm --trash help          # all trash subcommands
```

Features: restore (single/multi/`all`, merges directories), permanent
`delete`/`clear`, on-demand `compress`/`decompress` (tar + zstd, metadata
preserved), time-based `retention` (default 30 days, lazy + `prune`), soft
`max-size` warning, a `blacklist` (programs/scripts that delete permanently),
a path `whitelist` (trash only certain paths), `orphans` recovery, a
configurable storage `target` (e.g. a mounted box), and a global `on`/`off`
switch. Removed files stay private (trash dir is `0700`).

### Not the desktop trash

This is an **independent** trash (`$XDG_DATA_HOME/rm-trash`, SQLite metadata).
It is **not** the freedesktop.org/desktop trash: items removed here do **not**
appear in your file manager's Trash, and items in the desktop Trash are not
managed by this tool.

## Status & licensing

An **independent, clean-room implementation** that is command-line compatible with
GNU coreutils `rm`, but **not derived from** GNU coreutils (no GPL source was
copied; behaviour was reproduced from documentation and black-box observation).

Licensed under **GPL-3.0-or-later** (see [LICENSE](LICENSE)). This is a
deliberate choice, not an obligation: the implementation is independent, so
the copyleft is chosen to keep the tool and its derivatives free, rather than
inherited from GNU coreutils.
