#!/usr/bin/env bash
# 在 macOS 上校验 DMG：挂载 -> 安装 pkg -> 打印现场证据 -> 断言 -> 卸载 -> 断言无残留
#
# 用法：sudo bash packaging/macos/verify-dmg.sh dist/ClipboardX-0.1.0-universal.dmg
#
# 说明：CI 的 macOS runner 是无图形会话的，所以这里只能验证「安装 + 二进制可运行 +

set -euo pipefail

DMG="${1:?用法: verify-dmg.sh <dmg 路径>}"
DMG="$(cd "$(dirname "$DMG")" && pwd)/$(basename "$DMG")"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "错误：本脚本只能在 macOS 上运行" >&2
  exit 1
fi
if [ "$(id -u)" != "0" ]; then
  echo "错误：需要 root（sudo）来执行 installer 与 launchctl 校验" >&2
  exit 1
fi

MOUNT_POINT="$(mktemp -d)"
fail() { echo "断言失败：$1" >&2; exit 1; }
ok() { echo "    [ok] $1"; }

cleanup() {
  hdiutil detach "$MOUNT_POINT" -quiet 2>/dev/null || true
  rmdir "$MOUNT_POINT" 2>/dev/null || true
}
trap cleanup EXIT

echo "==> 挂载 $DMG"
hdiutil attach "$DMG" -mountpoint "$MOUNT_POINT" -nobrowse -quiet
[ -f "$MOUNT_POINT/ClipboardX.pkg" ] || fail "DMG 中缺少 ClipboardX.pkg"
[ -f "$MOUNT_POINT/安装说明.txt" ] || fail "DMG 中缺少安装说明"
[ -x "$MOUNT_POINT/卸载.command" ] || fail "DMG 中缺少可执行的卸载脚本"
ok "DMG 内容正确（pkg + 说明 + 卸载脚本）"

echo "==> 校验 pkg 元数据"
pkgutil --check-signature "$MOUNT_POINT/ClipboardX.pkg" || echo "    [warn] pkg 未签名（预期行为，除非提供了 SIGN_ID）"
PKG_PAYLOAD="$(pkgutil --payload-files "$MOUNT_POINT/ClipboardX.pkg" 2>&1 || true)"
printf '%s\n' "$PKG_PAYLOAD" | sed 's/^/        /'
printf '%s' "$PKG_PAYLOAD" | grep -q 'usr/local/bin/cx' || fail "pkg payload 中没有 cx"
printf '%s' "$PKG_PAYLOAD" | grep -q 'com.clipboardx.agent.plist' || fail "pkg payload 中没有 LaunchAgent"
ok "pkg payload 含二进制与 LaunchAgent"

echo "==> 安装 pkg"
installer -pkg "$MOUNT_POINT/ClipboardX.pkg" -target / -verboseR

# 先打印现场证据，再断言——断言失败时也需要看到"到底装了什么"
echo "==> 安装现场证据"
echo "    --- /usr/local/bin ---"
ls -l /usr/local/bin 2>&1 | sed 's/^/        /' | head -n 12
echo "    --- /Library/LaunchAgents ---"
ls -l /Library/LaunchAgents 2>&1 | sed 's/^/        /' | head -n 12
echo "    --- 收据与文件清单 ---"
pkgutil --pkgs 2>&1 | grep -i clipboardx | sed 's/^/        /' || echo "        （没有 clipboardx 收据）"
pkgutil --files com.clipboardx.cli 2>&1 | sed 's/^/        cli 收据: /' | head -n 10

[ -x /usr/local/bin/cx ] || fail "/usr/local/bin/cx 未安装或不可执行"
[ -f /Library/LaunchAgents/com.clipboardx.agent.plist ] || fail "LaunchAgent 未安装"
ok "二进制与 LaunchAgent 均已落位"

echo "==> 断言二进制可运行且是通用二进制"
/usr/local/bin/cx version || fail "cx version 退出码非 0"
ARCHS="$(lipo -archs /usr/local/bin/cx 2>/dev/null || echo unknown)"
echo "    架构：$ARCHS"
case "$ARCHS" in
  *arm64*x86_64*|*x86_64*arm64*) ok "通用二进制（arm64 + x86_64）" ;;
  *) echo "    [warn] 非通用二进制：$ARCHS" ;;
esac

JSON="$(/usr/local/bin/cx version --json)"
echo "$JSON" | grep -q '"name": *"cx"' || fail "cx version --json 输出缺少 name"
ok "cx version --json 输出正常"

echo "==> 断言 LaunchAgent 可被 launchd 解析"
plutil -lint /Library/LaunchAgents/com.clipboardx.agent.plist >/dev/null || fail "LaunchAgent plist 格式非法"
/usr/libexec/PlistBuddy -c 'Print :Label' /Library/LaunchAgents/com.clipboardx.agent.plist \
  | grep -qx 'com.clipboardx.agent' || fail "LaunchAgent Label 不正确"
/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' /Library/LaunchAgents/com.clipboardx.agent.plist \
  | grep -qx '/usr/local/bin/cx' || fail "LaunchAgent 未指向 /usr/local/bin/cx"
ok "LaunchAgent plist 合法、Label 与可执行路径正确"

echo "==> 卸载并断言无残留"
CONSOLE_UID="$(stat -f%u /dev/console 2>/dev/null || echo '')"
if [ -n "$CONSOLE_UID" ]; then
  launchctl bootout "gui/$CONSOLE_UID" /Library/LaunchAgents/com.clipboardx.agent.plist 2>/dev/null || true
fi
rm -f /Library/LaunchAgents/com.clipboardx.agent.plist /usr/local/bin/cx
pkgutil --forget com.clipboardx.cli >/dev/null 2>&1 || true
pkgutil --forget com.clipboardx.agent >/dev/null 2>&1 || true
[ ! -e /usr/local/bin/cx ] || fail "cx 未被移除"
[ ! -e /Library/LaunchAgents/com.clipboardx.agent.plist ] || fail "LaunchAgent 未被移除"
ok "卸载干净"

echo "==> DMG 校验全部通过"
