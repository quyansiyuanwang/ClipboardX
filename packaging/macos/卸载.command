#!/bin/sh
# 卸载 ClipboardX（命令行工具 + LaunchAgent）
# 双击本文件即可运行；需要输入管理员密码。
set -eu

PLIST="/Library/LaunchAgents/com.clipboardx.agent.plist"

echo "即将卸载 ClipboardX。"

if [ -f "$PLIST" ]; then
  CONSOLE_USER="$(stat -f%Su /dev/console 2>/dev/null || echo root)"
  CONSOLE_UID="$(id -u "$CONSOLE_USER" 2>/dev/null || echo '')"
  if [ -n "$CONSOLE_UID" ] && [ "$CONSOLE_USER" != "root" ] && [ "$CONSOLE_USER" != "loginwindow" ]; then
    sudo launchctl bootout "gui/$CONSOLE_UID" "$PLIST" 2>/dev/null || true
  fi
  sudo rm -f "$PLIST"
  echo "  已移除 LaunchAgent"
fi

if [ -f /usr/local/bin/cx ] || [ -f /usr/local/bin/cx-server ]; then
  # pkg 装了两个二进制；只删 cx 会把 cx-server 留成无人管理的残留文件
  sudo rm -f /usr/local/bin/cx /usr/local/bin/cx-server
  echo "  已移除 /usr/local/bin/cx 与 /usr/local/bin/cx-server"
fi

# 收据（可用 pkgutil --forget 清理）
sudo pkgutil --forget com.clipboardx.cli >/dev/null 2>&1 || true
sudo pkgutil --forget com.clipboardx.agent >/dev/null 2>&1 || true

echo
echo "配置、密钥与剪贴板历史仍保留在 ~/Library/Application Support/ClipboardX。"
printf "是否一并删除？[y/N] "
read -r answer
case "$answer" in
  y|Y|yes|YES)
    rm -rf "$HOME/Library/Application Support/ClipboardX"
    echo "  已删除用户数据"
    ;;
  *)
    echo "  已保留用户数据"
    ;;
esac

echo "卸载完成。"
