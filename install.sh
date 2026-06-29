#!/usr/bin/env bash
#
# Install recycle-rm so that `rm` runs it instead of the system rm, WITHOUT
# deleting the real rm. It works purely by name resolution: the binary is named
# `rm` and placed in a directory that is put in FRONT of your PATH. The real rm
# stays at /usr/bin/rm and is always reachable by its full path.
#
# Safe by design:
#   * Default install is per-user (~/.local/bin) and needs no sudo.
#   * Only YOUR shell start-up files are touched (idempotent, clearly marked,
#     reversible by --uninstall). No system-wide config (/etc) is changed, so
#     machines that intentionally order their PATH differently are left alone.
#
# Usage:
#   ./install.sh                 # per-user install into ~/.local/bin
#   ./install.sh --system        # system-wide into /usr/local/bin (needs root)
#   PREFIX=/opt/x ./install.sh   # install into $PREFIX/bin
#   ./install.sh --no-path       # install only, do not edit any PATH file
#   ./install.sh --dry-run       # show what would happen, change nothing
#   ./install.sh --uninstall     # remove the binary and the PATH lines again

set -euo pipefail

SYSTEM=0; DRY_RUN=0; UNINSTALL=0; NO_PATH=0
for arg in "$@"; do
  case "$arg" in
    --system)    SYSTEM=1 ;;
    --no-path)   NO_PATH=1 ;;
    --dry-run)   DRY_RUN=1 ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help)   sed -n '2,21p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

run() { if [ "$DRY_RUN" = 1 ]; then echo "[dry-run] $*"; else "$@"; fi; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

# --- choose the install directory -------------------------------------------
if [ -n "${PREFIX:-}" ]; then
  DEST="$PREFIX/bin"
elif [ "$SYSTEM" = 1 ]; then
  DEST="/usr/local/bin"
  if [ "$(id -u)" != 0 ]; then
    echo "error: --system installs into $DEST and must be run as root (try sudo)." >&2
    exit 1
  fi
else
  DEST="${XDG_BIN_HOME:-$HOME/.local/bin}"
fi
TARGET="$DEST/rm"

# Shell start-up files of the CURRENT user that we may prepend DEST to.
MARK_BEGIN="# >>> recycle-rm PATH >>>"
MARK_END="# <<< recycle-rm PATH <<<"
profile_files() {
  local files=("$HOME/.profile")
  case "$(basename "${SHELL:-}")" in
    bash) files+=("$HOME/.bashrc") ;;
    zsh)  files+=("$HOME/.zshrc" "$HOME/.zprofile") ;;
  esac
  printf '%s\n' "${files[@]}"
}

remove_path_block() {
  local f="$1"
  [ -f "$f" ] || return 0
  grep -qF "$MARK_BEGIN" "$f" || return 0
  run sed -i "\#$MARK_BEGIN#,\#$MARK_END#d" "$f"
  echo "Removed PATH lines from $f"
}

# --- uninstall --------------------------------------------------------------
if [ "$UNINSTALL" = 1 ]; then
  if [ -e "$TARGET" ]; then run rm -f "$TARGET"; echo "Removed $TARGET"; else echo "No binary at $TARGET"; fi
  while IFS= read -r f; do remove_path_block "$f"; done < <(profile_files)
  echo "Uninstalled. Open a new shell so the change takes effect."
  exit 0
fi

# --- build ------------------------------------------------------------------
echo "Building release binary (cargo build --release)..."
run cargo build --release
SRC="$ROOT/target/release/rm"
if [ "$DRY_RUN" = 0 ] && [ ! -x "$SRC" ]; then
  echo "error: build did not produce $SRC" >&2; exit 1
fi

# --- install ----------------------------------------------------------------
echo "Installing $SRC -> $TARGET"
run install -Dm755 "$SRC" "$TARGET"

# --- make sure OURS wins over /usr/bin/rm -----------------------------------
# True when $DEST appears before the directory of the current `rm` in PATH.
dest_is_preferred() {
  local d sys_rm sys_dir
  sys_rm="$(command -v rm 2>/dev/null || true)"
  sys_dir="$(dirname "${sys_rm:-/usr/bin/rm}")"
  [ "$sys_dir" = "$DEST" ] && return 0
  local IFS=:
  for d in $PATH; do
    [ "$d" = "$DEST" ] && return 0       # DEST comes first
    [ "$d" = "$sys_dir" ] && return 1    # system dir comes first
  done
  return 1
}

prepend_path() {
  local f="$1" created=0
  [ -e "$f" ] || { [ "$DRY_RUN" = 1 ] && { echo "[dry-run] create $f"; return 0; }; : > "$f"; created=1; }
  if grep -qsF "$MARK_BEGIN" "$f"; then echo "PATH already set in $f"; return 0; fi
  run sh -c "printf '\n%s\nexport PATH=\"%s:\$PATH\"\n%s\n' '$MARK_BEGIN' '$DEST' '$MARK_END' >> '$f'"
  echo "Prepended $DEST to PATH in $f"
}

echo
if dest_is_preferred; then
  echo "PATH ok: $DEST already comes before the system rm."
elif [ "$NO_PATH" = 1 ]; then
  echo "NOTE: $DEST is not ahead of the system rm in PATH, and --no-path was given."
  echo "      Add it yourself: export PATH=\"$DEST:\$PATH\""
elif [ "$SYSTEM" = 1 ]; then
  # Do NOT rewrite system-wide PATH: respect a machine that orders it on purpose.
  echo "NOTE: this system lists /usr/bin before $DEST in PATH."
  echo "      Not changing system configuration. To prefer recycle-rm system-wide,"
  echo "      ensure $DEST precedes /usr/bin in your PATH policy, or install per-user."
else
  # Per-user: prepend in the user's own start-up files (reversible, marked).
  while IFS= read -r f; do prepend_path "$f"; done < <(profile_files)
fi

# --- verify and report the truth --------------------------------------------
echo
echo "The system rm is untouched and still available as /usr/bin/rm."
if [ "$DRY_RUN" = 1 ]; then
  echo "(dry-run: nothing was changed.)"
elif dest_is_preferred; then
  hash -r 2>/dev/null || true
  echo "Active now: 'rm' resolves to $(command -v rm)"
  echo "Verify:     rm --version"
else
  echo "Done. Open a NEW shell (or: source ~/.profile), then check:  type rm"
  echo "It should print: $TARGET"
fi
