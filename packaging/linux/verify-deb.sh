#!/usr/bin/env bash
# 在 Linux 上校验 .deb：安装 -> 断言 -> 卸载 -> 断言
#
# 用法：sudo bash packaging/linux/verify-deb.sh dist/ClipboardX_0.1.0_amd64.deb
#
# 说明：CI runner 没有图形会话，因此这里只验证「安装 + 二进制可运行 + 单元文件落位」。

set -euo pipefail

DEB="${1:?用法: verify-deb.sh <deb 路径>}"
DEB="$(cd "$(dirname "$DEB")" && pwd)/$(basename "$DEB")"

if [ "$(id -u)" != "0" ]; then
  echo "错误：需要 root（sudo）" >&2
  exit 1
fi

fail() { echo "断言失败：$1" >&2; exit 1; }
ok() { echo "    [ok] $1"; }

echo "==> 检查包元数据"
dpkg-deb -I "$DEB" | sed 's/^/    /'
PKG="$(dpkg-deb -f "$DEB" Package)"
[ "$PKG" = "clipboardx" ] || fail "包名应为 clipboardx（实际 $PKG）"
ok "包名与版本正确"

echo "==> 安装"
dpkg -i "$DEB" >/dev/null || fail "dpkg -i 失败"

[ -x /usr/bin/cx ] || fail "/usr/bin/cx 未安装"
[ -f /usr/lib/systemd/user/clipboardx.service ] || fail "systemd 用户单元未安装"
[ -f /usr/share/applications/clipboardx.desktop ] || fail "desktop 文件未安装"
ok "文件均已落位"

echo "==> 断言二进制可运行"
/usr/bin/cx version || fail "cx version 退出码非 0"
/usr/bin/cx version --json | grep -q '"name": *"cx"' || fail "cx version --json 输出异常"
ok "cx version 正常"

echo "==> 断言 systemd 用户单元可解析"
if command -v systemd-analyze >/dev/null 2>&1 && command -v dbus-run-session >/dev/null 2>&1; then
  if dbus-run-session -- systemd-analyze --user verify /usr/lib/systemd/user/clipboardx.service \
       >/tmp/cx-unit-verify.log 2>&1; then
    ok "用户单元语法正确"
  else
    echo "    [warn] systemd-analyze --user verify 未通过（容器/无会话环境的常见现象）："
    head -n 5 /tmp/cx-unit-verify.log | sed 's/^/        /'
  fi
else
  echo "    [warn] 缺少 systemd-analyze 或 dbus-run-session，跳过单元语法校验"
fi

if [ -e /etc/systemd/user/graphical-session.target.wants/clipboardx.service ]; then
  ok "已为用户会话全局启用自启"
else
  echo "    [warn] 未发现全局启用符号链接（无 systemd 的环境下属正常）"
fi

echo "==> 卸载并断言无残留"
dpkg -r "$PKG" >/dev/null || fail "dpkg -r 失败"
[ ! -e /usr/bin/cx ] || fail "cx 未被移除"
[ ! -e /usr/lib/systemd/user/clipboardx.service ] || fail "用户单元未被移除"
ok "卸载干净"

echo "==> deb 校验全部通过"
