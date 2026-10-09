#!/bin/sh
# Exit 0 when docs/design.md has at most 200 lines, 1 when it has more, 4
# when it cannot be read. An optional argument names another file.
file="${1:-docs/design.md}"
limit=200

[ -f "$file" ] && [ -r "$file" ] || {
  echo "design-length: cannot read $file" >&2
  exit 4
}
lines="$(wc -l < "$file")" || exit 4
lines=$((lines))
echo "design-length: $file has $lines lines (limit $limit)"
[ "$lines" -le "$limit" ]
