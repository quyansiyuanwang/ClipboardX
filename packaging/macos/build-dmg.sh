#!/usr/bin/env bash
# 构建 macOS 产物：通用二进制 -> 组件 .pkg -> .dmg
#
# 只能在 macOS 上运行（pkgbuild / hdiutil / lipo 都是系统自带，
# 且 macOS 目标无法从 Windows 交叉编译）。
#
# 设计说明（踩坑记录）：
#   最初用「两个组件包 + productbuild --distribution」以提供"是否自启"的可选界面，
#   但 productbuild 会把 pkg-ref 静默解析失败：产出的 product 归档只有 4 KB，
#   组件包的 480 KB payload 根本没被嵌入，installer 却报告"安装成功"、
#   实际什么都没装。
#   现在改为**单一组件包**（命令行 + LaunchAgent 一起装），不再依赖 productbuild。
#   自启的控制改为运行时命令 `cx service install|uninstall`（见安装说明）。
#
# 用法：
#   bash packaging/macos/build-dmg.sh
#   VERSION=0.1.0 SKIP_BUILD=1 CX_BIN=target/release/cx bash packaging/macos/build-dmg.sh
#   SIGN_ID="Developer ID Installer: Xxx (TEAMID)" NOTARIZE=1 bash packaging/macos/build-dmg.sh
#
# 环境变量：
#   VERSION     版本号，默认取 Cargo.toml 的 workspace.version
#   SKIP_BUILD  1 = 不编译，改用 CX_BIN 指定的现成二进制
#   CX_BIN      SKIP_BUILD=1 时的二进制路径（需已是通用二进制或本机架构）
#   SIGN_ID     可选：Developer ID Installer 证书名，用于给 .pkg 签名
#   NOTARIZE    1 = 使用 notarytool 公证并 stapler 装订
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "错误：DMG 只能在 macOS 上构建（当前 $(uname -s)）" >&2
  exit 1
fi

for tool in pkgbuild pkgutil hdiutil stat; do
  command -v "$tool" >/dev/null || { echo "错误：缺少 $tool" >&2; exit 1; }
done

if [ -z "${VERSION:-}" ]; then
  VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
fi
[ -n "$VERSION" ] || { echo "错误：无法确定版本号" >&2; exit 1; }

DIST="$REPO/dist"
mkdir -p "$DIST"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

ROOT="$STAGE/root"
SCRIPTS="$STAGE/scripts"
mkdir -p "$ROOT/usr/local/bin" "$ROOT/Library/LaunchAgents" "$SCRIPTS"

echo "==> ClipboardX $VERSION macOS 通用二进制"
if [ "${SKIP_BUILD:-0}" = "1" ]; then
  [ -n "${CX_BIN:-}" ] || { echo "错误：SKIP_BUILD=1 需要 CX_BIN" >&2; exit 1; }
  cp "$CX_BIN" "$ROOT/usr/local/bin/cx"
else
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --target aarch64-apple-darwin --bin cx
  cargo build --release --target x86_64-apple-darwin --bin cx
  if command -v lipo >/dev/null; then
    lipo -create -output "$ROOT/usr/local/bin/cx" \
      target/aarch64-apple-darwin/release/cx \
      target/x86_64-apple-darwin/release/cx
  else
    cp target/aarch64-apple-darwin/release/cx "$ROOT/usr/local/bin/cx"
  fi
fi
chmod 755 "$ROOT/usr/local/bin/cx"

# 硬断言：这一步曾经静默产出空 payload，导致安装"成功"但 /usr/local/bin/cx 不存在
BIN_SIZE="$(stat -f%z "$ROOT/usr/local/bin/cx" 2>/dev/null || echo 0)"
BIN_ARCHS="$(lipo -archs "$ROOT/usr/local/bin/cx" 2>/dev/null || echo 未知)"
echo "    cx 二进制：$BIN_SIZE 字节，架构 $BIN_ARCHS"
if [ "$BIN_SIZE" -lt 100000 ]; then
  echo "错误：cx 二进制仅 $BIN_SIZE 字节，构建产物异常，终止打包" >&2
  exit 1
fi

echo "==> LaunchAgent 与安装后脚本"
cp packaging/macos/com.clipboardx.agent.plist "$ROOT/Library/LaunchAgents/"
cp packaging/macos/scripts/postinstall "$SCRIPTS/postinstall"
chmod 755 "$SCRIPTS/postinstall"

echo "==> pkgbuild（单一组件包：/usr/local/bin/cx + LaunchAgent）"
pkgbuild --root "$ROOT" \
  --identifier com.clipboardx.cli \
  --version "$VERSION" \
  --scripts "$SCRIPTS" \
  --install-location / \
  "$STAGE/ClipboardX.pkg"

PKG_SIZE="$(stat -f%z "$STAGE/ClipboardX.pkg")"
echo "    pkg 体积：$PKG_SIZE 字节"
PAYLOAD="$(pkgutil --payload-files "$STAGE/ClipboardX.pkg" 2>&1 || true)"
printf '%s\n' "$PAYLOAD" | sed 's/^/        /'
printf '%s' "$PAYLOAD" | grep -q 'usr/local/bin/cx' \
  || { echo "错误：pkg payload 中没有 usr/local/bin/cx" >&2; exit 1; }
printf '%s' "$PAYLOAD" | grep -q 'com.clipboardx.agent.plist' \
  || { echo "错误：pkg payload 中没有 LaunchAgent" >&2; exit 1; }
if [ "$PKG_SIZE" -lt 200000 ]; then
  echo "错误：pkg 仅 $PKG_SIZE 字节，明显不含二进制" >&2
  exit 1
fi

FINAL_PKG="$STAGE/ClipboardX.pkg"
if [ -n "${SIGN_ID:-}" ]; then
  echo "==> productsign（$SIGN_ID）"
  productsign --sign "$SIGN_ID" "$STAGE/ClipboardX.pkg" "$STAGE/ClipboardX-signed.pkg"
  FINAL_PKG="$STAGE/ClipboardX-signed.pkg"
else
  echo "==> 跳过签名（未提供 SIGN_ID）；用户首次打开需要右键「打开」或去除 quarantine 属性"
fi

echo "==> 组装 DMG 内容"
DMGROOT="$STAGE/dmg"
mkdir -p "$DMGROOT"
cp "$FINAL_PKG" "$DMGROOT/ClipboardX.pkg"
cp packaging/macos/安装说明.txt "$DMGROOT/安装说明.txt"
cp packaging/macos/卸载.command "$DMGROOT/卸载.command"
chmod 755 "$DMGROOT/卸载.command"

OUT="$DIST/ClipboardX-$VERSION-universal.dmg"
rm -f "$OUT"
echo "==> hdiutil create -> $OUT"
hdiutil create -volname "ClipboardX $VERSION" -srcfolder "$DMGROOT" -ov -format UDZO "$OUT"

OUT_SIZE="$(stat -f%z "$OUT")"
if [ "$OUT_SIZE" -lt 200000 ]; then
  echo "错误：DMG 仅 $OUT_SIZE 字节，说明 payload 为空（安装后会找不到 cx）" >&2
  exit 1
fi
echo "    DMG 体积：$OUT_SIZE 字节"

if [ "${NOTARIZE:-0}" = "1" ]; then
  : "${APPLE_ID:?公证需要 APPLE_ID}"
  : "${APPLE_TEAM_ID:?公证需要 APPLE_TEAM_ID}"
  : "${APPLE_PASSWORD:?公证需要 APPLE_PASSWORD（App 专用密码）}"
  echo "==> notarytool 公证（可能需要数分钟）"
  xcrun notarytool submit "$OUT" \
    --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_PASSWORD" --wait
  echo "==> stapler 装订"
  xcrun stapler staple "$OUT"
  xcrun stapler validate "$OUT"
fi

echo "==> 完成：$OUT"
ls -lh "$OUT"
