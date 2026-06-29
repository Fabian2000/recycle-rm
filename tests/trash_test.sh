#!/usr/bin/env bash
# Integration tests for the trash pipeline (not 1:1-with-GNU; that's difftest).
# Exercises: trash, list, size, restore (single/multi/all/merge), delete, clear,
# compress/decompress (+guards), prune/retention, orphans, blacklist, whitelist,
# self-containment guard. Each section runs against an isolated trash.

set -u
BIN="${BIN:-$(pwd)/target/debug/rm}"
PASS=0
FAIL=0
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

ok()   { PASS=$((PASS+1)); }
bad()  { FAIL=$((FAIL+1)); echo "FAIL: $1"; }
# eq <actual> <expected> <desc>
eq()   { if [ "$1" = "$2" ]; then ok; else bad "$3 (got '$1', want '$2')"; fi; }
yes()  { if [ "$1" = "0" ]; then ok; else bad "$2"; fi; }   # 1st arg = exit code

# Fresh isolated trash + workspace for a section; sets $T (trash root) and cds in.
fresh() {
  SEC="$work/$1"; rm -rf "$SEC"; mkdir -p "$SEC/home" "$SEC/play"
  export XDG_DATA_HOME="$SEC/home"
  T="$SEC/home/rm-trash"
  cd "$SEC/play"
}
db() { sqlite3 "$T/rm.db" "$1"; }

# ---- trash + list + restore (single) --------------------------------------
fresh basic
echo hello > f.txt
"$BIN" f.txt; yes $? "trash file"
eq "$(test -e f.txt && echo y || echo n)" "n" "file gone from source"
eq "$(db 'SELECT count(*) FROM trash')" "1" "one db row"
"$BIN" --trash restore 1 >/dev/null; yes $? "restore"
eq "$(cat f.txt 2>/dev/null)" "hello" "restored content"
eq "$(db 'SELECT count(*) FROM trash')" "0" "db row gone after restore"

# ---- recursive dir, restore multi & all -----------------------------------
fresh multi
mkdir d1 d2; echo a > d1/a; echo b > d2/b; echo c > c.txt
"$BIN" -r d1 d2; "$BIN" c.txt
eq "$(db 'SELECT count(*) FROM trash')" "3" "three entries"
"$BIN" --trash restore all >/dev/null; yes $? "restore all"
eq "$(cat d1/a)$(cat d2/b)$(cat c.txt)" "abc" "all restored"

# ---- delete (permanent, single + multi) -----------------------------------
fresh del
echo x>x; echo y>y; "$BIN" x y
printf 'y\n' | "$BIN" --trash delete 1 2 >/dev/null; yes $? "delete multi"
eq "$(db 'SELECT count(*) FROM trash')" "0" "entries deleted"

# ---- clear -----------------------------------------------------------------
fresh clr
echo x>x; echo y>y; "$BIN" x y
printf 'n\n' | "$BIN" --trash clear >/dev/null
eq "$(db 'SELECT count(*) FROM trash')" "2" "clear aborted on no"
printf 'y\n' | "$BIN" --trash clear >/dev/null
eq "$(db 'SELECT count(*) FROM trash')" "0" "clear emptied on yes"

# ---- compress / decompress round trip (metadata preserved) ----------------
fresh comp
echo content > big.txt; chmod 640 big.txt; touch -d '2019-01-02 03:04:05' big.txt
"$BIN" big.txt
"$BIN" --trash compress >/dev/null; yes $? "compress"
eq "$(db 'SELECT compressed FROM trash LIMIT 1')" "1" "marked compressed"
# double compress must fail
"$BIN" --trash compress 1 >/dev/null 2>&1; eq "$?" "1" "double compress errors"
"$BIN" --trash decompress >/dev/null; yes $? "decompress"
eq "$(db 'SELECT compressed FROM trash LIMIT 1')" "0" "marked decompressed"
# compress again then restore (decompress path)
"$BIN" --trash compress >/dev/null
"$BIN" --trash restore 1 >/dev/null; yes $? "restore compressed"
eq "$(cat big.txt)" "content" "compressed restore content"
eq "$(stat -c '%a' big.txt)" "640" "compressed restore mode"
eq "$(stat -c '%y' big.txt | cut -d. -f1)" "2019-01-02 03:04:05" "compressed restore mtime"

# ---- child-before-parent restore merge ------------------------------------
fresh merge
mkdir -p p/sub; echo child > p/sub/c; echo other > p/o
"$BIN" p/sub/c        # child first
"$BIN" -r p           # then parent
"$BIN" --trash restore 1 >/dev/null   # child -> recreates p/sub
eq "$(cat p/sub/c)" "child" "child restored, parent recreated"
"$BIN" --trash restore 2 >/dev/null   # parent -> merges into existing p
eq "$(cat p/o)$(cat p/sub/c)" "otherchild" "parent merged, child kept"

# ---- retention: manual prune + lazy prune ---------------------------------
fresh ret
echo a>a; echo b>b; "$BIN" a b
old=$(( ($(date +%s) - 40*86400) * 1000000 ))
db "UPDATE trash SET deleted_at=$old WHERE original_path LIKE '%/a'"
"$BIN" --trash prune >/dev/null
eq "$(db 'SELECT count(*) FROM trash')" "1" "manual prune removed expired"
db "UPDATE trash SET deleted_at=$old WHERE original_path LIKE '%/b'"
echo c>c; "$BIN" c    # lazy prune on next trashing rm
eq "$(db "SELECT count(*) FROM trash WHERE original_path LIKE '%/b'")" "0" "lazy prune removed expired b"

# ---- orphans ---------------------------------------------------------------
fresh orph
echo a>a; "$BIN" a
echo junk > "$T/files/deadbeef-orphan"
eq "$("$BIN" --trash orphans | grep -c deadbeef)" "1" "orphan detected"
printf 'y\n' | "$BIN" --trash orphans clear >/dev/null
eq "$("$BIN" --trash orphans | grep -c deadbeef)" "0" "orphan cleared"

# ---- blacklist (program) ---------------------------------------------------
fresh bl
cat > caller.sh <<EOF
#!/bin/bash
"$BIN" "\$1"
EOF
chmod +x caller.sh
"$BIN" --trash blacklist add caller.sh >/dev/null
echo data > viaA; ./caller.sh viaA
eq "$(test -e viaA && echo y || echo n)" "n" "blacklisted script deletes"
eq "$(db 'SELECT count(*) FROM trash')" "0" "blacklisted not trashed"

# ---- whitelist (path) ------------------------------------------------------
fresh wl
mkdir proj other; echo p>proj/f; echo o>other/f
"$BIN" --trash whitelist add proj >/dev/null
"$BIN" proj/f
eq "$(db "SELECT count(*) FROM trash WHERE original_path LIKE '%proj/f'")" "1" "whitelisted path trashed"
"$BIN" other/f
eq "$(test -e other/f && echo y || echo n)" "n" "non-whitelisted deleted"
eq "$(db "SELECT count(*) FROM trash WHERE original_path LIKE '%other/f'")" "0" "non-whitelisted not trashed"

# ---- self-containment guard ------------------------------------------------
fresh selfc
echo a>a; "$BIN" a >/dev/null   # init trash under XDG home
"$BIN" -r "$XDG_DATA_HOME" >/dev/null 2>&1; eq "$?" "1" "refuse to trash dir containing trash"
eq "$(test -d "$T" && echo y || echo n)" "y" "trash intact after guard"

# ---- conflict policy: refuse, touch nothing --------------------------------
fresh conflict
echo orig > f.txt
"$BIN" f.txt                      # trash it (id 1)
echo NEW > f.txt                  # a new file now occupies the original path
"$BIN" --trash restore 1 >/dev/null 2>&1; eq "$?" "1" "restore into existing file refused"
eq "$(cat f.txt)" "NEW" "existing target untouched on refused restore"
eq "$(db 'SELECT count(*) FROM trash')" "1" "entry kept after refused restore"
# orphan recover into an existing path is refused too
echo junk > "$T/files/orph1"
echo EXIST > occupied
"$BIN" --trash orphans recover orph1 occupied >/dev/null 2>&1; eq "$?" "1" "orphan recover into existing refused"
eq "$(cat occupied)" "EXIST" "existing target untouched on refused recover"

# ---- --one-file-system (needs a real mount; skipped if unavailable) --------
fresh ofs
mkdir -p top/sub top/mnt; echo a > top/sub/a
if mount -t tmpfs none top/mnt 2>/dev/null; then
  echo inside > top/mnt/inside
  out="$("$BIN" -rv --one-file-system top 2>&1)"; code=$?
  eq "$code" "1" "one-file-system exits 1 (skipped a mount)"
  eq "$(echo "$out" | grep -c "skipping 'top/mnt'")" "1" "prints skip message"
  eq "$(test -e top/mnt/inside && echo y || echo n)" "y" "mount left intact"
  eq "$(test -e top/sub/a && echo y || echo n)" "n" "same-device part removed"
  eq "$(db "SELECT count(*) FROM trash WHERE original_path LIKE '%/top'")" "1" "partial entry trashed"
  umount top/mnt 2>/dev/null
else
  echo "(skipped --one-file-system test: no mount privilege)"
fi

echo "-----------------------------"
echo "trash integration: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ]
