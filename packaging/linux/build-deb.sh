#!/usr/bin/env bash
# 构建 Debian/Ubuntu 安装包（不依赖 nfpm/cargo-deb，只用 dpkg-deb）
#
# 用法：
#   bash packaging/linux/build-deb.sh
#   VERSION=0.1.0 SKIP_BUILD=1 CX_BIN=target/release/cx bash packaging/linux/build-deb.sh
#   ARCH=arm64 bash packaging/linux/build-deb.sh
#
# 环境变量：
#   VERSION     版本号，默认取 Cargo.toml 的 workspace.version
#   ARCH        dpkg 架构名（amd64 | arm64），默认 amd64
#   SKIP_BUILD  1 = 不编译，改用 CX_BIN
#   CX_BIN      SKIP_BUILD=1 时的二进制路径
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

command -v dpkg-deb >/dev/null || { echo "错误：缺少 dpkg-deb（apt install dpkg-dev）" >&2; exit 1; }

if [ -z "${VERSION:-}" ]; then
  VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
fi
[ -n "$VERSION" ] || { echo "错误：无法确定版本号" >&2; exit 1; }

ARCH="${ARCH:-amd64}"
case "$ARCH" in
  amd64) TARGET="x86_64-unknown-linux-gnu" ;;
  arm64) TARGET="aarch64-unknown-linux-gnu" ;;
  *) echo "错误：不支持的 ARCH=$ARCH" >&2; exit 1 ;;
esac

DIST="$REPO/dist"
mkdir -p "$DIST"
PKGROOT="$(mktemp -d)"
trap 'rm -rf "$PKGROOT"' EXIT

echo "==> ClipboardX $VERSION ($ARCH / $TARGET)"
if [ "${SKIP_BUILD:-0}" = "1" ]; then
  [ -n "${CX_BIN:-}" ] || { echo "错误：SKIP_BUILD=1 需要 CX_BIN" >&2; exit 1; }
  [ -n "${CX_SERVER_BIN:-}" ] || { echo "错误：SKIP_BUILD=1 需要 CX_SERVER_BIN" >&2; exit 1; }
  BIN="$CX_BIN"
  SERVER_BIN="$CX_SERVER_BIN"
else
  cargo build --release --target "$TARGET" --bin cx --bin cx-server
  BIN="target/$TARGET/release/cx"
  SERVER_BIN="target/$TARGET/release/cx-server"
fi
[ -f "$BIN" ] || { echo "错误：找不到二进制 $BIN" >&2; exit 1; }
[ -f "$SERVER_BIN" ] || { echo "错误：找不到二进制 $SERVER_BIN" >&2; exit 1; }

echo "==> 组装包内容"
mkdir -p "$PKGROOT/DEBIAN" \
         "$PKGROOT/usr/bin" \
         "$PKGROOT/usr/lib/systemd/user" \
         "$PKGROOT/lib/systemd/system" \
         "$PKGROOT/usr/share/applications" \
         "$PKGROOT/usr/share/doc/clipboardx"

install -m 755 "$BIN" "$PKGROOT/usr/bin/cx"
install -m 644 packaging/linux/clipboardx.service "$PKGROOT/usr/lib/systemd/user/clipboardx.service"
install -m 755 "$SERVER_BIN" "$PKGROOT/usr/bin/cx-server"
# 系统级 unit：**默认不启用**。自建服务端是可选部署（通常也装在服务器上，
# 而不是桌面机上），装了就地自动起服务才是更糟的默认。
install -m 644 packaging/linux/clipboardx-server.service "$PKGROOT/lib/systemd/system/clipboardx-server.service"
install -m 644 packaging/linux/clipboardx.desktop "$PKGROOT/usr/share/applications/clipboardx.desktop"
install -m 644 packaging/linux/README-install.zh-CN.txt "$PKGROOT/usr/share/doc/clipboardx/README.zh-CN.txt"
install -m 644 README.md "$PKGROOT/usr/share/doc/clipboardx/README.md"

SIZE_KB="$(du -sk "$PKGROOT/usr" | cut -f1)"

cat > "$PKGROOT/DEBIAN/control" <<EOF
Package: clipboardx
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Maintainer: ClipboardX contributors <noreply@example.com>
Homepage: https://github.com/quyansiyuanwang/ClipboardX
Installed-Size: $SIZE_KB
Depends: libc6 (>= 2.31)
Description: 极度轻量的三端剪贴板同步工具（守护进程 + TUI）
 ClipboardX 在一台设备复制后，其他设备可直接粘贴，无需注册账号、
 无需公网中继。
 .
 同步后端可选自建服务端、WebDAV（Nextcloud 等）或 S3（MinIO / R2 / AWS），
 内容端到端加密，服务端与对象存储只见密文。
 .
 剪贴板访问需要图形会话（X11 或 Wayland）；无会话时可运行
  cx daemon --no-clipboard 作为纯中继。自启为 systemd 用户单元
  （graphical-session.target），不使用系统服务。
EOF

cat > "$PKGROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
# 为用户启用自启（写入 /etc/systemd/user/*.wants，不启动任何东西）
if command -v systemctl >/dev/null 2>&1; then
  systemctl --global enable clipboardx.service >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q || true
fi
# 自建服务端要用的系统账号与数据目录（幂等）。
# 服务本身**不自动启用**：它通常装在服务器上，装完就监听端口是更糟的默认。
if ! getent group clipboardx >/dev/null 2>&1; then
  addgroup --system clipboardx >/dev/null 2>&1 || true
fi
if ! getent passwd clipboardx >/dev/null 2>&1; then
  adduser --system --ingroup clipboardx --home /var/lib/clipboardx \
          --no-create-home --disabled-login clipboardx >/dev/null 2>&1 || true
fi
install -d -o clipboardx -g clipboardx -m 0700 /var/lib/clipboardx 2>/dev/null || true

echo "ClipboardX 已安装。新开终端后运行：cx version / cx status / cx tui"
if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
  echo "如需立即启动：systemctl --user start clipboardx.service"
else
  echo "当前无图形会话，桌面登录后守护进程会随 graphical-session.target 启动。"
fi
echo
echo "自建服务端（可选）：cx-server 已就位，默认未启用。"
echo "  启用一个本地服务端：sudo systemctl enable --now clipboardx-server"
echo "  或前台试跑：        cx-server serve --data ~/.local/share/cx-server"
exit 0
EOF

cat > "$PKGROOT/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
  if command -v systemctl >/dev/null 2>&1; then
    systemctl --global disable clipboardx.service >/dev/null 2>&1 || true
  fi
fi
exit 0
EOF

cat > "$PKGROOT/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "purge" ]; then
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q || true
  fi
fi
# 用户数据（~/.config/clipboardx、~/.local/share/clipboardx）保留，便于重装免重新配对
exit 0
EOF

chmod 755 "$PKGROOT/DEBIAN/postinst" "$PKGROOT/DEBIAN/prerm" "$PKGROOT/DEBIAN/postrm"

OUT="$DIST/ClipboardX_${VERSION}_${ARCH}.deb"
rm -f "$OUT"
echo "==> dpkg-deb --build -> $OUT"
dpkg-deb --root-owner-group --build "$PKGROOT" "$OUT"

echo "==> 完成：$OUT ($(du -h "$OUT" | cut -f1))"
