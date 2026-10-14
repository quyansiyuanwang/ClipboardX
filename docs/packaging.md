# 打包与分发

## 1. 产物矩阵

| 平台 | 产物 | 构建方式 | 本地（Windows）可验证 | CI 可验证 |
|---|---|---|---|---|
| Windows | `ClipboardX-<v>-x64.msi` | WiX v4（仓库内 dotnet tool 固定版本） | ✅ 可构建 + 真实安装/卸载验收 | ✅ `windows-latest` |
| macOS | `ClipboardX-<v>-universal.dmg`（内含 `ClipboardX.pkg`） | `pkgbuild` + `productbuild` + `hdiutil` + `lipo` | ❌ 只能由 macOS 构建 | ✅ `macos-latest` |
| Linux | `ClipboardX_<v>_<arch>.deb` | `dpkg-deb`（无额外工具） | ⚠️ 需 WSL/Docker 才能构建（本机 Docker daemon 未运行） | ✅ `ubuntu-latest` |

| 通用 | 校验和 `SHA256SUMS` | `sha256sum` | ✅ | ✅ |

**为什么 DMG 不能在本机（Windows）产出**：`pkgbuild`/`productbuild`/`hdiutil` 是 macOS 自带工具；
macOS 目标也无法从 Windows 交叉编译（需要 macOS SDK，且剪贴板代码本身是平台相关的）。
因此 macOS 产物只能在 macOS 主机或 `macos-*` runner 上构建。

## 2. 每平台安装包做了什么

### 2.1 Windows MSI（`packaging/wix/Package.wxs`）

- `Scope="perUser"`：装到 `%LOCALAPPDATA%\Programs\ClipboardX`，**不需要管理员权限**。
- 可选特性：`PathEnv`（用户 PATH，默认开）、`Autostart`（`HKCU\...\Run`，默认开）、
  `DesktopShortcut`（默认关）；基础特性 `Main` 必装。
- 开始菜单：`ClipboardX TUI` → `cx.exe tui`。
- 卸载：移除文件 / 快捷方式 / Run 键 / PATH 项；**保留** `%APPDATA%\ClipboardX` 用户数据，
  传 `REMOVEUSERDATA=1` 才清除。
- 不使用 Windows 服务：Session 0 没有交互式桌面，服务账户读不到剪贴板。

命令：

```powershell
pwsh scripts/build-msi.ps1                 # 产物在 dist\
pwsh scripts/verify-msi.ps1 -Msi dist\ClipboardX-0.1.0-x64.msi   # 安装→断言→卸载→断言
pwsh scripts/build-msi.ps1 -NoUi           # 不装 UI 扩展，产出无向导的 MSI（离线兜底）
```

### 2.2 macOS DMG（`packaging/macos/`）

- 同一 arm64 runner 上分别构建 `aarch64-apple-darwin` 与 `x86_64-apple-darwin`，
  用 `lipo -create` 合成通用二进制（若失败可退化为两个单架构 DMG）。
- 两个组件包：
  - `com.clipboardx.cli` → `/usr/local/bin/cx`
  - `com.clipboardx.agent` → `/Library/LaunchAgents/com.clipboardx.agent.plist` + 立即装载脚本
- `Distribution.xml` 提供"自定"步骤：是否安装登录自启（默认勾选）。
- 安装后脚本 `postinstall` 用 `stat -f%u /dev/console` 找出控制台用户，
  `launchctl bootstrap gui/<uid>` 立即装载；无图形会话时提示下次登录自动加载。
- DMG 内容：`ClipboardX.pkg` + `安装说明.txt` + `卸载.command`。
- 为什么不做成 `.app`：程序是 TUI，没有图形主窗口，"拖入 Applications"不适用。


```bash
bash packaging/macos/build-dmg.sh                                   # 未签名
SIGN_ID="Developer ID Installer: X (TEAMID)" bash packaging/macos/build-dmg.sh
SIGN_ID="…" NOTARIZE=1 APPLE_ID=… APPLE_TEAM_ID=… APPLE_PASSWORD=… bash packaging/macos/build-dmg.sh
sudo bash packaging/macos/verify-dmg.sh dist/ClipboardX-0.1.0-universal.dmg
```

### 2.3 Linux deb（`packaging/linux/`）

- 只依赖 `dpkg-deb`，不引入 `nfpm` / `cargo-deb` 等额外工具链。
- 安装内容：`/usr/bin/cx`、`/usr/lib/systemd/user/clipboardx.service`、
  `/usr/share/applications/clipboardx.desktop`、文档。
- `postinst` 执行 `systemctl --global enable clipboardx.service`（只写符号链接，不立即启动）。
- 用户单元的关键点：`PartOf/After=graphical-session.target`、
  `PassEnvironment=DISPLAY WAYLAND_DISPLAY XAUTHORITY XDG_RUNTIME_DIR …`、
  `Restart=on-failure`、`Nice=5`、`MemoryMax=256M`、`CPUQuota=25%`，
  以及 `ProtectSystem=strict` + `ProtectHome=read-only` + 仅可写自身配置/数据目录。

```bash
bash packaging/linux/build-deb.sh
sudo bash packaging/linux/verify-deb.sh dist/ClipboardX_0.1.0_amd64.deb
```

## 3. 三端自启机制对照

| 平台 | 机制 | 由谁写入 | 为什么不用系统级 |
|---|---|---|---|
| Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` → `cx.exe start --silent` | MSI 特性 / `cx service install` | Session 0 服务读不到剪贴板 |
| macOS | `~/Library/LaunchAgents/com.clipboardx.agent.plist`（系统级放 `/Library/LaunchAgents`，仍在用户会话内运行） | pkg postinstall / `cx service install` | LaunchDaemon 在系统会话，无 NSPasteboard |
| Linux | systemd **user** unit，`WantedBy=graphical-session.target` | deb postinst / `cx service install` | system 服务没有 DISPLAY/WAYLAND_DISPLAY |

## 4. 签名与公证

| 产物 | 未签名的后果 | 需要的证书 | 启用方式 |
|---|---|---|---|
| MSI | SmartScreen 提示「Windows 已保护你的电脑」→「更多信息」→「仍要运行」 | 代码签名证书（OV/EV） | CI secrets：`WINDOWS_CERT_PFX` / `WINDOWS_CERT_PASSWORD`（`signtool` 步骤） |
| DMG / pkg | Gatekeeper 提示「未识别开发者」→ 右键「打开」或 `xattr -dr com.apple.quarantine` | Apple Developer ID（$99/年） | CI secrets：`APPLE_CERT_P12` / `APPLE_ID` / `APPLE_TEAM_ID` / `APPLE_APP_PASSWORD` |

首版默认**不签名**，安装说明里写明绕过步骤；补上证书后只需在 CI 增加 secrets，流程不变。

## 5. CI 流水线（`.github/workflows/ci.yml`）

| Job | Runner | 内容 |
|---|---|---|
| `test` | ubuntu / windows / macos 三平台矩阵 | `cargo fmt --check`、`clippy -D warnings`、`cargo test`、release 构建、**二进制体积预算断言** |
| `msi` | windows-latest | 构建 MSI → `msiexec /i /qn` → 断言文件/PATH/Run 键/`cx version` → `msiexec /x /qn` → 断言无残留 |
| `dmg` | macos-latest | 构建通用二进制 → pkg → dmg → 挂载 → `installer -pkg` → `lipo -archs` 校验 → 卸载 |
| `deb` | ubuntu-latest | 构建 deb → `dpkg -i` → 断言 → `dpkg -r` → Xvfb 下启动冒烟 |

**注意**：CI 的 macOS/Linux runner 没有图形会话，所以 CI 能验证"安装、文件落位、二进制可运行"，


> 首次推送前 CI 不会运行；仓库需要在 GitHub 上创建（`gh auth status` 已登录 `quyansiyuanwang`）。
> 私有仓库的 macOS runner 计费为 Linux 的 10 倍（一次 DMG 构建约 30–60 计费分钟），公开仓库免费。



事实与坑（都已解决，写下来避免重踩）：


   （无 `candle`/`light`），`windows-latest` 预装 WiX v3。结论：MSI 走自写 WiX v4 源 + 仓库内 dotnet tool。
2. **WiX 扩展的 flavor 目录与版本强绑定**：4.x 落在 `wixext4`，7.x 落在 `wixext7`。
   用 `-ext WixToolset.UI.wixext`（不带版本）会解析到扩展缓存里的**最高版本**（7.0.0），
   再用当前工具的 flavor 去找目录 ⇒ `WIX0144 The extension could not be found`。
   解法：按工具版本安装（`wix extension add --global WixToolset.UI.wixext/4.0.6`）
   并**显式传入 DLL 路径**（见 `scripts/build-msi.ps1`）。
3. **`WIX0104`**：XML 注释里不能出现连续两个连字符（`--`），源文件里写 `start --silent` 会直接编译失败。
4. **`WIX0311` / `WIX0349`**：中文字符串需要设置数据库代码页。最终采用 `Package@Codepage="936"` +
   `SummaryInformation@Codepage="936"`（摘要流只接受 ANSI 代码页，`65001` 会被拒绝）。
5. **`msiexec` 与 PATH 环境变量**：`<Environment System="no">` 写的是用户 PATH（`HKCU\Environment`），
   卸载会自动还原，验证脚本据此断言。
6. **产物与验收结果**：`ClipboardX-0.1.0-x64.msi` 约 476 KB；本机
   安装 → 文件落位、PATH 生效、Run 键写入、`cx version` 可运行 → 卸载 → 文件/Run 键/PATH 全部还原，全部通过。

  仓库内 dotnet tool 固定的 WiX 4.0.6、Docker 已装但 daemon 未运行、WSL 有 Debian（用于 `bash -n` 校验脚本）。

## 7. 常见故障排查

| 现象 | 原因与处理 |
|---|---|
| `WIX0144` 找不到扩展 | 扩展版本与 WiX 工具版本不匹配；按 §6.2 处理，或 `-NoUi` 构建无向导 MSI |
| `WIX0311` 字符不在代码页 | 源文件里有中文但未设 `Codepage`；设 `936` |
| `msiexec` 返回 1603 | 查看 `dist\msi-logs\install.log`；常见是已存在同版本且 `AllowSameVersionUpgrades` 未生效 |
| pkg 安装后 `cx` 命令找不到 | 新开终端（PATH 由 pkg 写入 `/usr/local/bin`）；确认 `/usr/local/bin` 在 PATH 中 |
| LaunchAgent 未生效 | `launchctl print gui/$(id -u)/com.clipboardx.agent`；确认是 agent 在用户会话内而非 daemon |
| Linux 守护进程报会话缺失 | 无 DISPLAY/WAYLAND_DISPLAY；桌面登录后再启动，或用 `cx daemon --no-clipboard` 做纯中继 |
