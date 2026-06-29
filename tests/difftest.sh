#!/usr/bin/env bash
# Differential test: compare our safe-rm against the system GNU rm.
#
# For each scenario we run BOTH binaries on an identical freshly-created
# fixture and compare stdout, stderr and exit code.
#
# Clean-room: this only observes the real rm's external behaviour.

set -u
# Resolve to an absolute path: scenarios run with cwd inside a fixture dir.
OURS="${OURS:-$(pwd)/target/debug/rm}"
# Invoke the real rm by basename so its program-name prefix is "rm:".
REAL="${REAL:-rm}"
PASS=0
FAIL=0

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# make_fixture <dir>: populate a directory with a known layout.
make_fixture() {
  local d="$1"
  mkdir -p "$d"
  ( cd "$d"
    : > file.txt
    echo content > nonempty.txt
    : > another.log
    : > -dashfile          2>/dev/null || true
    mkdir -p emptydir
    mkdir -p full/sub
    : > full/a
    : > full/sub/b
    ln -s file.txt link
    mkfifo fifo          2>/dev/null || true
    : > notadir
    # exotic names: newline, tab, single quote, non-UTF8 byte
    : > "$(printf 'wn\nl')"   2>/dev/null || true
    : > "$(printf 'wt\tb')"   2>/dev/null || true
    : > "wq'q"                2>/dev/null || true
    : > "$(printf 'wu\377x')" 2>/dev/null || true
  )
}

# INPUT is fed to stdin (always a pipe -> is_terminal() is false for both,
# matching the script/CI use case). Set it before calling check_in.
INPUT=""

# run <binary> <args...>  (args run inside a fresh fixture, cwd = fixture)
run() {
  local bin="$1"; shift
  local fx; fx="$(mktemp -d "$work/fx.XXXXXX")"
  make_fixture "$fx"
  local out err code
  # Isolate our trash into the throwaway fixture (real rm ignores XDG_DATA_HOME).
  out="$( cd "$fx" && XDG_DATA_HOME="$fx/.xdg" printf '%b' "$INPUT" | XDG_DATA_HOME="$fx/.xdg" "$bin" "$@" 2>"$fx/.stderr" )"
  code=$?
  err="$(cat "$fx/.stderr")"
  printf '%s\n---CODE---\n%s\n---ERR---\n%s' "$out" "$code" "$err"
}

# check_in <input> <name> <args...>
check_in() {
  INPUT="$1"; shift
  check "$@"
  INPUT=""
}

check() {
  local name="$1"; shift
  local a b
  a="$(run "$REAL" "$@")"
  b="$(run "$OURS" "$@")"
  if [ "$a" = "$b" ]; then
    PASS=$((PASS+1))
    # echo "PASS: $name"
  else
    FAIL=$((FAIL+1))
    echo "FAIL: $name  [args: $*]"
    diff <(printf '%s' "$a") <(printf '%s' "$b") | sed 's/^/    /'
  fi
}

# ---- scenarios -------------------------------------------------------------
check "simple file"            file.txt
check "verbose"            -v  file.txt
check "two files"              file.txt another.log
check "missing operand"
check "nonexistent"            nope.xyz
check "force nonexistent"  -f  nope.xyz
check "dir without -r"         emptydir
check "empty dir -d"       -d  emptydir
check "nonempty dir -d"    -d  full
check "recursive"          -r  full
check "recursive verbose"  -rv full
check "bundled -rf"        -rf full
check "dash file via --"   --  -dashfile
check "symlink"                link
check "invalid short"      -Z  file.txt
check "unrecognized long"  --nope file.txt
check "bad interactive"    --interactive=foo file.txt
check "preserve root -r"   -r  /
check "root no -r"             /
check "root -d"            -d  /
check "double recursive"   -r -r full
check "force no operand"   -f

# --- option abbreviation (getopt_long prefix matching) ----------------------
check "abbrev --rec"       --rec full
check "abbrev --r"         --r full
check "abbrev --ver"       --ver
check "ambiguous --v"      --v file.txt
check "ambiguous --ve"     --ve file.txt
check "abbrev npr --no"    --no -r full
check "abbrev npr --n"     --n -r full
check "unrecognized=val"   --nope=1 file.txt
check "interactive abbrev" --i=foo file.txt

# --- error wording ----------------------------------------------------------
check "not a directory"        notadir/x
check "force not a directory" -f notadir/x
check "force isdir"        -f  emptydir

# --- interactive prompts (descriptors, descend, rpmatch) --------------------
check_in "y\n"   "-i empty"        -i file.txt
check_in "y\n"   "-i nonempty"     -i nonempty.txt
check_in "y\n"   "-i fifo"         -i fifo
check_in "y\n"   "-i symlink"      -i link
check_in "n\n"   "-i no"           -i file.txt
check_in "yeah\n" "-i yeah=yes"    -i file.txt
check_in "  y\n" "-i space=no"     -i file.txt
check_in "y\ny\ny\ny\ny\ny\n" "-ri all yes" -ri full
check_in "n\n"   "-ri descend no"  -ri full
check_in "y\n"   "-di empty dir"   -di emptydir
check_in "y\n"   "-I once recursive" -I -r full
check_in "n\n"   "-I once no"      -I -r full

# --- exotic filenames (GNU shell-escape quoting) ----------------------------
check "verbose newline name" -v "$(printf 'wn\nl')"
check "verbose tab name"     -v "$(printf 'wt\tb')"
check "verbose quote name"   -v "wq'q"
check "verbose nonutf8 name" -v "$(printf 'wu\377x')"
check "error nonexistent newline" "$(printf 'no\nsuch')"

echo "-----------------------------"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ]
