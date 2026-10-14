#!/bin/sh
# FileHunter из исходников рядом с установленной версией (macOS): своя папка данных,
# свои вкладки и история, своё имя в сети. Установленную копию не трогает.
# При первом запуске копирует индекс установленной версии, чтобы не сканировать диски заново.
set -e
cd "$(dirname "$0")"
BASE="$HOME/Library/Application Support"
export FILECAT_DATA_DIR="$BASE/FileHunter (dev)"
export FILECAT_NAME="$(scutil --get ComputerName 2>/dev/null || hostname) (dev)"
SRC="$BASE/FileHunter"
# интерфейс из папки проекта: правки HTML/CSS/JS видны по Ctrl+R/F5 в окне, без пересборки
export FILEHUNTER_UI_DIR="$(pwd)/app/ui"

if [ ! -f "$FILECAT_DATA_DIR/index.bin" ] && [ -f "$SRC/index.bin" ]; then
  echo "Копирую индекс установленной версии..."
  mkdir -p "$FILECAT_DATA_DIR"
  for f in index.bin drives.json s3.json sources.json hashes.db; do
    if [ -f "$SRC/$f" ]; then cp "$SRC/$f" "$FILECAT_DATA_DIR/"; fi
  done
  if [ -d "$SRC/etags" ]; then cp -R "$SRC/etags" "$FILECAT_DATA_DIR/"; fi
fi

echo "Собираю..."
nice -n 10 cargo build -p filehunter --profile fast
./target/fast/filehunter &
