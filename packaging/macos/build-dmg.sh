#!/usr/bin/env bash
# 构建 macOS 产物：通用二进制 -> .pkg -> .dmg
#
# 只能在 macOS 上运行（pkgbuild / productbuild / hdiutil / lipo 都是系统自带，
# 且 macOS 目标无法从 Windows 交叉编译）。
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
#   NOTARIZE    1 = 使用 notarytool 公证并 stapler 装订（需 APPLE_ID / APPLE_TEAM_ID / APPLE_PASSWORD）
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "错误：DMG 只能在 macOS 上构建（当前 $(uname -s)）" >&2
  exit 1
fi

for tool in pkgbuild productbuild hdiutil; do
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

CLI_ROOT="$STAGE/cli-root"
AGENT_ROOT="$STAGE/agent-root"
AGENT_SCRIPTS="$STAGE/agent-scripts"
mkdir -p "$CLI_ROOT/usr/local/bin" "$AGENT_ROOT/Library/LaunchAgents" "$AGENT_SCRIPTS"

echo "==> ClipboardX $VERSION macOS 通用二进制"
if [ "${SKIP_BUILD:-0}" = "1" ]; then
  [ -n "${CX_BIN:-}" ] || { echo "错误：SKIP_BUILD=1 需要 CX_BIN" >&2; exit 1; }
  cp "$CX_BIN" "$CLI_ROOT/usr/local/bin/cx"
else
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --target aarch64-apple-darwin --bin cx
  cargo build --release --target x86_64-apple-darwin --bin cx
  if command -v lipo >/dev/null; then
    lipo -create -output "$CLI_ROOT/usr/local/bin/cx" \
      target/aarch64-apple-darwin/release/cx \
      target/x86_64-apple-darwin/release/cx
  else
    cp target/aarch64-apple-darwin/release/cx "$CLI_ROOT/usr/local/bin/cx"
  fi
fi
chmod 755 "$CLI_ROOT/usr/local/bin/cx"

echo "==> LaunchAgent 与安装脚本"
cp packaging/macos/com.clipboardx.agent.plist "$AGENT_ROOT/Library/LaunchAgents/"
cp packaging/macos/scripts/postinstall "$AGENT_SCRIPTS/postinstall"
chmod 755 "$AGENT_SCRIPTS/postinstall"

echo "==> pkgbuild（两个组件包：命令行 + 登录自启）"
pkgbuild --root "$CLI_ROOT" \
  --identifier com.clipboardx.cli \
  --version "$VERSION" \
  --install-location / \
  "$STAGE/cli.pkg"

pkgbuild --root "$AGENT_ROOT" \
  --identifier com.clipboardx.agent \
  --version "$VERSION" \
  --scripts "$AGENT_SCRIPTS" \
  --install-location / \
  "$STAGE/agent.pkg"

echo "==> productbuild（可选组件：是否安装自启）"
productbuild --distribution packaging/macos/Distribution.xml \
  --package-path "$STAGE" \
  --resources packaging/macos/resources \
  "$STAGE/ClipboardX.pkg"

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
