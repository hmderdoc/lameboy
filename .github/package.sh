#!/usr/bin/env bash
# Package one built target into dist/ for the release.
# Usage: package.sh <label> <rust-target> <version>
set -eu

LABEL="$1"
TARGET="$2"
VERSION="$3"

BIN="terminal_gameboy"
EXT=""
case "$TARGET" in
  *windows*) EXT=".exe" ;;
esac

SRC="target/${TARGET}/release/${BIN}${EXT}"
if [ ! -f "$SRC" ]; then
  echo "build artifact not found: $SRC" >&2
  exit 1
fi

NAME="lameboy_${VERSION}_${LABEL}"
OUT="dist/${NAME}"
mkdir -p "$OUT"

cp "$SRC" "$OUT/"
# Docs sysops need; ignore any that are absent.
for f in README.md LICENSE PATCH-NOTES.md xtrn.ini.example; do
  [ -f "$f" ] && cp "$f" "$OUT/" || true
done

cd dist
case "$TARGET" in
  *windows*) zip -r "${NAME}.zip" "$NAME" ;;
  *)         tar czf "${NAME}.tar.gz" "$NAME" ;;
esac
echo "packaged dist/${NAME}.*"
