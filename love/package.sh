#!/usr/bin/env sh
# Build the LÖVE front-end into a portable .love plus per-platform bundles.
#
#   ./package.sh [version] [outdir] [service-binary]
#
# Bundles embed the official LÖVE 11.5 runtime (zlib/libpng licensed). The
# runtime binaries are downloaded unmodified; only the game archive is added on
# top (appended to love.exe on Windows, copied into Resources on macOS).
set -eu

version=${1:-dev}
out=${2:-dist}
service=${3:-}
root=$(cd "$(dirname "$0")" && pwd)
out_abs=$(mkdir -p "$out" && cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

base=https://github.com/love2d/love/releases/download/11.5
fetch() {
  if [ ! -f "$work/$1" ]; then
    curl -fsSL "$base/$1" -o "$work/$1"
  fi
}

# --- portable game archive ---------------------------------------------------
name=bluetooth-input-bridge-love-$version
mkdir -p "$work/game"
cp -R "$root/main.lua" "$root/conf.lua" "$root/src" "$root/assets" "$work/game/"
(cd "$work/game" && zip -9 -q -r "$out_abs/$name.love" main.lua conf.lua src assets)
cp "$out_abs/$name.love" "$work/game.love"
echo "built $name.love"

readme() {
  cat > "$1" <<'EOF'
BlueBridgeIme — LÖVE 前端
=========================

这是「蓝牙输入桥」的轻量前端，界面与 Tauri 版一致但更小、更省资源，
适合低配置 / 低分辨率电脑。它通过本地 socket 连接无界面 Rust 服务，
不包含转发逻辑本身；电脑上仍需运行 bluetooth-input-bridge-service。

运行方式见本目录下的说明，或直接看 love/README.md。

Linux 包里带有 bluetooth-input-bridge-service，界面连接不上时会尝试
在 .love 旁边启动它；也可以自己用 systemd 管理（见 desktop/README.md）。

刷新率上限 30 FPS，并且只在界面内容变化时重绘。

LÖVE 运行时按 zlib/libpng 许可证分发（见 LICENSE-love.txt），未做修改。
EOF
}

# --- Linux (AppImage + game) -------------------------------------------------
fetch love-11.5-x86_64.AppImage
mkdir -p "$work/linux"
cp "$work/love-11.5-x86_64.AppImage" "$work/linux/love.AppImage"
chmod +x "$work/linux/love.AppImage"
cp "$work/game.love" "$work/linux/bluebridge.love"
cp "$root/LICENSE-love.txt" "$work/linux/"
if [ -n "$service" ] && [ -f "$service" ]; then
  cp "$service" "$work/linux/bluetooth-input-bridge-service"
  chmod +x "$work/linux/bluetooth-input-bridge-service"
fi
readme "$work/linux/README.txt"
cat > "$work/linux/run.sh" <<'EOF'
#!/bin/sh
# 在低配置机器上不需要解包 AppImage 也能跑；若系统缺少 FUSE：
#   ./love.AppImage --appimage-extract-and-run bluebridge.love
cd "$(dirname "$0")"
exec ./love.AppImage bluebridge.love
EOF
chmod +x "$work/linux/run.sh"
(cd "$work/linux" && tar -czf "$out_abs/$name-linux-x86_64.tar.gz" .)
echo "built $name-linux-x86_64.tar.gz"

# --- Windows (fused executable) ---------------------------------------------
fetch love-11.5-win64.zip
mkdir -p "$work/win"
unzip -q "$work/love-11.5-win64.zip" -d "$work/win"
win=$(dirname "$(find "$work/win" -name love.exe -type f | head -1)")
# LÖVE 会读取附加在 exe 末尾的 zip，因此一个 exe 就够，无需拖动。
cat "$win/love.exe" "$work/game.love" > "$win/BlueBridgeIme.exe"
cp "$root/LICENSE-love.txt" "$win/"
readme "$win/README.txt"
(cd "$win" && zip -9 -q -r "$out_abs/$name-windows-x86_64.zip" .)
echo "built $name-windows-x86_64.zip"

# --- macOS (game inside the app bundle) -------------------------------------
fetch love-11.5-macos.zip
mkdir -p "$work/mac"
unzip -q "$work/love-11.5-macos.zip" -d "$work/mac"
app=$(find "$work/mac" -type d -name love.app | head -1)
cp "$work/game.love" "$app/Contents/Resources/game.love"
cp "$root/LICENSE-love.txt" "$work/mac/"
readme "$work/mac/README.txt"
# -y 保留符号链接，.app 里 Frameworks 用的是链接。
(cd "$work/mac" && zip -9 -q -r -y "$out_abs/$name-macos.zip" .)
echo "built $name-macos.zip"