#!/bin/sh
set -eu

# Manual test-oracle helper. DEVELOPER TOOL ONLY.
#
# This is the one place in the repository that runs an external `openscad`
# binary, and it does so purely as a reference oracle: it renders *our own*
# `.scad` sources (the Appa fixtures) and diffs the result against the
# checked-in STL goldens. Comparing output against a reference implementation
# is not deriving from it — no upstream code enters this project, and the
# sibling `openscad/` checkout is never built, linked, or shipped.
#
# It is NOT a cargo target (cargo only builds `.rs` files under `tests/`), it
# is excluded from the container image by `web/.dockerignore`, and it never
# runs during `cargo test` or in CI. The production server never invokes
# OpenSCAD.
#
# With no argument (or `check`), render references into a temporary directory
# and compare them byte-for-byte with the checked-in fixtures. `regenerate`
# replaces fixture STLs only after an explicit confirmation environment flag:
#
#   ALLOW_ORACLE_REGENERATION=1 tests/oracle.sh regenerate

mode=${1:-check}
case "$mode" in
  check|regenerate) ;;
  *) echo "usage: $0 [check|regenerate]" >&2; exit 2 ;;
esac

command -v openscad >/dev/null 2>&1 || {
  echo "openscad is required only for this manual oracle operation" >&2
  exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fixture_dir="$script_dir/fixtures/openappa"
temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/reopenscad-oracles.XXXXXX")
trap 'rm -rf "$temporary_dir"' EXIT HUP INT TERM

manifest="$temporary_dir/cases.tsv"
# Accept both literal tabs and the visible `\t` delimiters used by the
# committed manifest.
tab=$(printf '\t')
awk -v tab="$tab" '{ gsub(/\\t/, tab); print }' "$fixture_dir/cases.tsv" > "$manifest"
while IFS="$tab" read -r case_id source definitions reference; do
  case "$case_id" in ''|'#'*) continue ;; esac
  output="$temporary_dir/$reference"
  set -- openscad --export-format binstl -o "$output"
  old_ifs=$IFS
  IFS=';'
  for definition in $definitions; do
    [ -n "$definition" ] && set -- "$@" -D "$definition"
  done
  IFS=$old_ifs
  set -- "$@" "$fixture_dir/$source"
  echo "rendering $case_id" >&2
  "$@"
done < "$manifest"

if [ "$mode" = regenerate ]; then
  [ "${ALLOW_ORACLE_REGENERATION:-}" = 1 ] || {
    echo "set ALLOW_ORACLE_REGENERATION=1 to replace checked-in references" >&2
    exit 2
  }
  while IFS="$tab" read -r case_id _source _definitions reference; do
    case "$case_id" in ''|'#'*) continue ;; esac
    cp "$temporary_dir/$reference" "$fixture_dir/$reference"
  done < "$manifest"
  echo "regenerated OpenAPPA STL fixtures" >&2
  exit 0
fi

status=0
while IFS="$tab" read -r case_id _source _definitions reference; do
  case "$case_id" in ''|'#'*) continue ;; esac
  if cmp -s "$temporary_dir/$reference" "$fixture_dir/$reference"; then
    echo "ok: $case_id"
  else
    echo "different: $case_id (STL serialization or geometry changed)" >&2
    status=1
  fi
done < "$manifest"
exit "$status"
