# 安装说明（Windows MSI）

ClipboardX 是**用户级**安装的剪贴板同步工具，安装过程不需要管理员权限。
之所以不做成系统服务：Windows 服务运行在 Session 0，没有交互式桌面，
读不到剪贴板。

## 安装

双击 `ClipboardX-<版本>-x64.msi`，或静默安装：

```powershell
msiexec /i ClipboardX-0.1.0-x64.msi /qn
# 自选特性：PATH / 自启 / 桌面快捷方式
msiexec /i ClipboardX-0.1.0-x64.msi /qn ADDLOCAL=Main,PathEnv,Autostart
```

安装位置：`%LOCALAPPDATA%\Programs\ClipboardX`。

## 覆盖安装（升级）

直接双击新版本的 MSI 即可，**不需要先手动关掉后台的 cx**：安装包会先停掉正在运行的
`cx.exe`（守护进程，以及你开着的 `cx tui`），再用新文件覆盖，最后自动把守护进程
重新放回后台。所以：

- 升级期间剪贴板同步会中断几秒，之后自动恢复；
- 你开着的 `cx tui` 会被关闭，升级完成后重新打开即可；
- 如果你在安装向导里点了「取消」，后台守护进程不会自动回来，手动跑一次
  `cx start` 或重新登录即可（自启项没被改动）。

## 安装后可做的事

```powershell
cx version          # 确认安装成功
cx status           # 查看守护进程与后端状态
cx tui              # 打开终端界面（也可用开始菜单里的 ClipboardX TUI）
```

## 卸载

```powershell
msiexec /x ClipboardX-0.1.0-x64.msi /qn
# 连同配置、密钥与历史一起清除：
msiexec /x ClipboardX-0.1.0-x64.msi /qn REMOVEUSERDATA=1
```

默认保留 `%APPDATA%\ClipboardX`（配置、空间密钥、剪贴板历史），
以便重装后无需重新配对。

## SmartScreen 提示

未签名的安装包首次运行会弹出「Windows 已保护你的电脑」：
点击「更多信息」→「仍要运行」即可。要消除该提示需要为 MSI 购买代码签名证书。
