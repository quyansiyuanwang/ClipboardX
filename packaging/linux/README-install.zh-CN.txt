ClipboardX 安装说明（Linux）
============================

安装
----
  sudo apt install ./ClipboardX_<版本>_<架构>.deb

安装内容：
  /usr/bin/cx                                     命令行工具（守护进程 + CLI + TUI）
  /usr/lib/systemd/user/clipboardx.service        systemd 用户单元（自启）
  /usr/share/applications/clipboardx.desktop      桌面菜单项（在终端中打开 TUI）

首次使用
--------
  cx version       确认安装成功
  cx status        查看守护进程与后端状态
  cx tui           打开终端界面
  cx backend add   配置同步后端（server / webdav / s3 三选一）

后台运行
--------
安装时已经为用户启用了自启（systemctl --global enable），桌面登录后随
graphical-session.target 自动启动。手动控制：

  systemctl --user status  clipboardx.service
  systemctl --user restart clipboardx.service
  systemctl --user stop    clipboardx.service
  journalctl --user -u clipboardx -f     查看日志

为什么是用户单元而不是系统服务
------------------------------
剪贴板属于图形会话：X11 需要 DISPLAY，Wayland 需要 WAYLAND_DISPLAY 与
会话总线。系统级服务没有这些环境，读不到剪贴板。

无图形会话（SSH / 服务器）
--------------------------
默认会在启动时报错提示会话缺失。只做中继（不读写本机剪贴板）时：

  cx daemon --foreground --no-clipboard

卸载
----
  sudo apt remove clipboardx     # 保留用户数据
  sudo apt purge  clipboardx     # 同上，收据与桌面库一并清理

用户数据位于 ~/.config/clipboardx 与 ~/.local/share/clipboardx，
卸载时保留，方便重装后免重新配对。
