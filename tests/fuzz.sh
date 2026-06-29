#!/usr/bin/env bash
# Randomized differential fuzzer: throw random option/operand combinations at
# both the real GNU rm and our clone on identical fresh fixtures, and compare
# stdout, stderr and exit code. Any divergence is a 1:1 violation.
#
# Deletion is stubbed in our binary, so we avoid scenarios whose *output*
# depends on a removal actually having happened (e.g. interactive runs that
# answer "no" to some children and then hit "Directory not empty"). We achieve
# this by only feeding all-yes or all-no answer streams.

set -u
OURS="${OURS:-$(pwd)/target/debug/rm}"
REAL="${REAL:-rm}"
N="${1:-400}"
SEED="${SEED:-1}"
PASS=0
FAIL=0
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

RANDOM=$SEED

OPTS=( -f -i -I -r -R -d -v --recursive --force --verbose --dir
       --interactive=always --interactive=never --interactive=once
       --one-file-system --no-preserve-root --rec --ver --r -rf -rv -ri -rfv )
NAMES=( file.txt nonempty.txt emptydir full link fifo notadir nope.xyz
        full/a notadir/x -dashfile . dir/deep )

make_fixture() {
  local d="$1"; mkdir -p "$d"
  ( cd "$d"
    : > file.txt; echo x > nonempty.txt; : > notadir
    mkdir -p emptydir full/sub dir/deep
    : > full/a; : > full/sub/b; : > dir/deep/leaf
    ln -s file.txt link
    mkfifo fifo 2>/dev/null || true
    : > -- -dashfile 2>/dev/null || true
  )
}

run() {
  local bin="$1"; local input="$2"; shift 2
  local fx; fx="$(mktemp -d "$work/fx.XXXXXX")"
  make_fixture "$fx"
  local out code
  out="$( cd "$fx" && printf '%b' "$input" | XDG_DATA_HOME="$fx/.xdg" "$bin" "$@" 2>"$fx/.e" )"
  code=$?
  printf '%s\n--C--\n%s\n--E--\n%s' "$out" "$code" "$(cat "$fx/.e")"
}

top_of() { printf '%s' "${1#./}" | cut -d/ -f1; }  # top-level component

for ((i=0; i<N; i++)); do
  # 1-3 random options
  args=()
  nopt=$(( RANDOM % 3 ))
  for ((o=0; o<=nopt; o++)); do args+=( "${OPTS[$((RANDOM % ${#OPTS[@]}))]}" ); done
  # 1-3 random operands, but keep them in DISJOINT subtrees and unique, so no
  # scenario's output depends on an actual deletion having happened (the hook
  # is a no-op). '.' is allowed: GNU and we both skip it identically.
  nop=$(( RANDOM % 3 ))
  used=" "
  for ((p=0; p<=nop; p++)); do
    cand="${NAMES[$((RANDOM % ${#NAMES[@]}))]}"
    t="$(top_of "$cand")"
    case "$used" in *" $t "*) continue;; esac
    used="$used$t "
    args+=( "$cand" )
  done
  # all-yes or all-no answer stream (avoids deletion-dependent divergence)
  if (( RANDOM % 2 )); then input="y\ny\ny\ny\ny\ny\ny\ny\n"; else input="n\nn\nn\nn\nn\nn\nn\n"; fi

  a="$(run "$REAL" "$input" "${args[@]}")"
  b="$(run "$OURS" "$input" "${args[@]}")"
  if [ "$a" = "$b" ]; then
    PASS=$((PASS+1))
  else
    FAIL=$((FAIL+1))
    echo "FAIL #$i  args: ${args[*]}   input: ${input//\\n/ }"
    diff <(printf '%s' "$a") <(printf '%s' "$b") | sed 's/^/    /'
    [ "$FAIL" -ge 15 ] && { echo "...stopping after 15 failures"; break; }
  fi
done

echo "-----------------------------"
echo "fuzz: PASS=$PASS FAIL=$FAIL (N=$N seed=$SEED)"
[ "$FAIL" -eq 0 ]
