# 打包与分发

## 1. 产物矩阵

| 平台 | 产物 | 构建方式 | 本地（Windows）可验证 | CI 可验证 |
|---|---|---|---|---|
| Windows | `ClipboardX-<v>-x64.msi` | WiX v4（仓库内 dotnet tool 固定版本） | ✅ 可构建 + 真实安装/卸载验收 | ✅ `windows-latest` |
| macOS | `ClipboardX-<v>-universal.dmg`（内含 `ClipboardX.pkg`） | `pkgbuild` + `productbuild` + `hdiutil` + `lipo` | ❌ 只能由 macOS 构建 | ✅ `macos-latest` |
| Linux | `ClipboardX_<v>_<arch>.deb` | `dpkg-deb`（无额外工具） | ⚠️ 需要 Linux 环境（WSL / Docker / Linux 主机） | ✅ `ubuntu-latest` |
| Linux | `.tar.gz`（可选） | `tar` | — | ✅ |
| 通用 | 校验和 `SHA256SUMS` | `sha256sum` | ✅ | ✅ |

**为什么 DMG 不能在 Windows 上产出**：`pkgbuild`/`productbuild`/`hdiutil` 是 macOS 自带工具；
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
- **单一组件包**（`pkgbuild`）：一个 `com.clipboardx.cli` 包里同时装
  - `/usr/local/bin/cx`
  - `/Library/LaunchAgents/com.clipboardx.agent.plist`
  - 安装后脚本 `postinstall`
- `postinstall` 用 `stat -f%u /dev/console` 找出控制台用户，
  `launchctl bootstrap gui/<uid>` 立即装载代理；无图形会话时提示下次登录自动加载。
- 自启开关不放在安装向导里，而是运行时命令 `cx service install|uninstall`
  （原因见 §6.1：productbuild 的产品归档会静默丢失 payload）。
- DMG 内容：`ClipboardX.pkg` + `安装说明.txt` + `卸载.command`。
- **卸载**（pkg 不自带卸载器，必须显式移除文件与收据）：
  - 推荐：挂载 DMG 后双击 `卸载.command`——bootout LaunchAgent、删除
    `/usr/local/bin/cx` 与 `/usr/local/bin/cx-server`、`pkgutil --forget` 两个收据，
    最后询问是否一并删除用户数据。
  - DMG 已丢失时手动执行等价命令：

    ```bash
    launchctl bootout "gui/$(stat -f%u /dev/console)" /Library/LaunchAgents/com.clipboardx.agent.plist 2>/dev/null || true
    sudo rm -f /Library/LaunchAgents/com.clipboardx.agent.plist /usr/local/bin/cx /usr/local/bin/cx-server
    sudo pkgutil --forget com.clipboardx.cli; sudo pkgutil --forget com.clipboardx.agent
    rm -rf "$HOME/Library/Application Support/ClipboardX"   # 可选：配置/空间密钥/历史一起删
    ```

  - 只想关掉开机自启、保留程序：`cx service uninstall`。
  - 注意 pkg 装了**两个**二进制（`cx` 与 `cx-server`）：只删 `cx` 会把 `cx-server`
    留成残留文件，而收据已被 forget，之后 `pkgutil` 再也查不到它；CI 验收现在会
    同时断言两者都已消失。
- 构建时硬断言三层：二进制 ≥ 100 KB、`pkgutil --payload-files` 必须列出
  `usr/local/bin/cx` 与 LaunchAgent、pkg 与 DMG 体积下限。任何一层不满足即失败，
  避免再出现"安装成功但什么都没有"的情况。
- 为什么不做成 `.app`：程序是 TUI，没有图形主窗口，"拖入 Applications"不适用。
  （后续可追加一个 `LSUIElement` 的 `.app` 作为可选启动器。）

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
| `e2e` | ubuntu / windows / macos 三平台矩阵 | `scripts/e2e-local.ps1 -NoClipboard`：`cx start` 分离进程 + start/stop/status/history/push/pause/resume/doctor/reload 全链路断言 |
| `e2e-webdav` | ubuntu / windows | `scripts/e2e-webdav.ps1`：用 wsgidav 起**真实 WebDAV 服务端**，两台守护进程互相同步（A 的剪贴板 → WebDAV → B 的历史，以及反向），并断言无回环放大 |
| `e2e-server` | ubuntu / windows / macos | `scripts/e2e-server.ps1`：真实 `cx-server` + 自助注册 + `cx space init/publish/join` + 两台守护进程同步 + 直接在服务端数据目录里断言明文没有落到任何对象里。不依赖任何外部服务 |
| `e2e-s3` | ubuntu / windows | `scripts/e2e-s3.ps1`：用 moto 起 **S3 兼容服务端**，两台守护进程经 S3（SigV4 签名 + 桶自动创建）互相同步；签名正确性另由 `cargo test` 里会重算签名的测试服务端保证 |
| `msi` | windows-latest | 构建 MSI → `msiexec /i /qn` → 断言文件/PATH/Run 键/`cx version` → `msiexec /x /qn` → 断言无残留 |
| `dmg` | macos-latest | 构建通用二进制 → pkg → dmg → 挂载 → `installer -pkg` → `lipo -archs` 校验 → 卸载 |
| `deb` | ubuntu-latest | 构建 deb → `dpkg -i` → 断言 → `dpkg -r` → Xvfb 下启动冒烟 |

**注意**：CI 的 macOS/Linux runner 没有图形会话，所以 CI 能验证"安装、文件落位、二进制可运行"，
**不能**验证剪贴板行为；那部分需要在真机上验收。

> 首次推送前 CI 不会运行；仓库需要在 GitHub 上创建（`gh auth status` 已登录 `quyansiyuanwang`）。
> 私有仓库的 macOS runner 计费为 Linux 的 10 倍（一次 DMG 构建约 30–60 计费分钟），公开仓库免费。

## 6. 打包踩坑记录

事实与坑（都已解决，写下来避免重踩）：

1. **`cargo-dist` 的 msi 走 WiX v3**，且明确不支持 WiX v4+。若开发机装的是 WiX v7
   （只有 `wix.exe`，没有 `candle`/`light`），这条路同样走不通。结论：MSI 用自写的 WiX v4 源 +
   仓库内 dotnet tool 固定版本（`.config/dotnet-tools.json`）。
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
6. **安装后会发生什么**（`scripts/verify-msi.ps1` 逐条断言）：文件落到
   `%LOCALAPPDATA%\Programs\ClipboardX`、用户 PATH 追加该目录、`HKCU\...\Run` 写入自启项、
   `cx version` 可运行；卸载后文件、Run 键与 PATH 全部还原。
### 6.1 macOS 打包踩坑（productbuild 产品归档）

首个 DMG 版本用"两个组件包 + `productbuild --distribution`"来提供"是否安装自启"的可选界面，
结果 CI 上出现一串极具误导性的现象：

1. `pkgbuild` 正常：`cli.pkg` 480 KB，`pkgutil --payload-files cli.pkg` 能列出 `./usr/local/bin/cx`；
2. `productbuild` 无任何告警，只是打印 "Wrote product to .../ClipboardX.pkg"；
3. 产出的 `ClipboardX.pkg` **只有 4 KB**——完整 payload 没被嵌入；
4. `installer -pkg ClipboardX.pkg -target /` 报告 "The install was successful."；
5. 但 `/usr/local/bin/cx` 与 `/Library/LaunchAgents/...plist` **都不存在**。

也就是说 Distribution.xml 里的 `<pkg-ref id="com.clipboardx.cli" />` 静默解析失败，
installer 装了一个空产品。最终处理方式：**放弃 productbuild**，改为单一组件包，
自启开关交给 `cx service install|uninstall`；并在构建脚本里加了三层硬断言
（二进制体积、payload 文件清单、pkg/DMG 体积下限），让这类问题在构建期就炸出来。

教训：**"安装成功"不等于"装上了"**，
打包流水线必须在断言里检查"该出现的文件确实出现了"。

### 6.2 守护进程与句柄继承（Windows 上会让脚本挂起）

`cx start` 需要在后台挂载守护进程，而"分离进程"在 Windows 上有一个隐蔽的坑：

**Rust 标准库的 `Command` 始终允许句柄继承（`bInheritHandles = TRUE`）。**
如果调用方的 stdout 是管道或重定向文件（shell 脚本、CI runner、编辑器任务都在这么干），
守护进程会把这个句柄一起继承走；只要守护进程活着，那个管道就不会关闭，
于是**调用方会一直挂住**。实测三种写法全部复现：

1. `cx start`（PowerShell 管道捕获输出）→ 挂起；
2. `cx start *> file`（重定向到文件）→ 挂起；
3. `Start-Process -Wait`（连 `-Wait` 都等不到返回）→ 挂起。

注意 `cx start` 自己其实**已经返回了**（`daemon.json` 已写出、守护进程已就绪），
挂住的是调用方对管道 EOF 的等待——这正是它难以排查的原因。

**修法**：Windows 下不用 `Command`，直接 `CreateProcessW(..., bInheritHandles = FALSE, CREATE_NO_WINDOW)`，
不给守护进程任何可继承句柄（见 `crates/cx-daemon/src/runtime.rs::spawn_detached_windows`）。
Unix 侧用 `setsid` 新建会话，fd 由调用方的 CLOEXEC 语义保证不被带过去。

顺带一提：`--quiet` 是后台启动时固定加上的，因为"无句柄环境"里写 stderr 没有意义；
守护进程的日志一律进 `logs/daemon.log`。

回归覆盖：`scripts/e2e-local.ps1` 与 CI 的 `e2e` job 都直接跑 `cx start`，
再断言进程活着、`cx status` 能连上、`cx stop` 能停掉——挂起会立刻表现为 CI 超时。

## 7. 常见故障排查

| 现象 | 原因与处理 |
|---|---|
| `WIX0144` 找不到扩展 | 扩展版本与 WiX 工具版本不匹配；按 §6.2 处理，或 `-NoUi` 构建无向导 MSI |
| `WIX0311` 字符不在代码页 | 源文件里有中文但未设 `Codepage`；设 `936` |
| `msiexec` 返回 1603 | 查看 `dist\msi-logs\install.log`；常见是已存在同版本且 `AllowSameVersionUpgrades` 未生效 |
| pkg 安装后 `cx` 命令找不到 | 新开终端（PATH 由 pkg 写入 `/usr/local/bin`）；确认 `/usr/local/bin` 在 PATH 中 |
| LaunchAgent 未生效 | `launchctl print gui/$(id -u)/com.clipboardx.agent`；确认是 agent 在用户会话内而非 daemon |
| Linux 守护进程报会话缺失 | 无 DISPLAY/WAYLAND_DISPLAY；桌面登录后再启动，或用 `cx daemon --no-clipboard` 做纯中继 |

## 8. 发布流程

打一个与 `Cargo.toml` 版本一致的 tag 即可：

```bash
# 先把版本号改好（Cargo.toml）并提交
git tag v0.1.0 && git push origin v0.1.0
```

`Release` 流水线（`.github/workflows/release.yml`）会：

1. **校验 tag 与 Cargo.toml 的版本一致**，且 tag 指向的提交在 `main` 上（防止从分支随手发版）；
2. 三端各自构建产物并**先验收再上传**：MSI 走 安装→断言→卸载，DMG 走 挂载→断言→卸载，
   deb 走 安装→冒烟→卸载；Windows 上额外跑一遍**升级验收**（装旧版→装新版→只留一份→卸干净）；
3. 配了证书就自动签名/公证（Windows 用 `signtool`，macOS 用 `codesign`+`notarytool`），
   没配就跳过并在日志里写明——两种状态都不需要改流程；
4. 汇总产物、生成 `SHA256SUMS`，用它建 GitHub Release（说明取 `docs/release-notes.md`）。

本地对应命令：

```powershell
pwsh scripts/verify-msi.ps1 -Msi dist\ClipboardX-0.1.0-x64.msi
pwsh scripts/verify-msi-upgrade.ps1            # 升级验收（自动构建 0.0.9 与 0.1.0）
bash packaging/linux/verify-deb.sh
bash packaging/macos/verify-dmg.sh
```
## 9. Windows 安装失败排查

安装/卸载反复失败时，最常见的是 Windows Installer 自己的临时目录被搞脏，而不是包有问题。
以下两种现象都在真机上复现过。

### 9.1 `Could not set file security for file … Error: 1307`

`1307` 是 `ERROR_INVALID_SID`：Installer 无法给它的**回滚脚本**设置安全描述符。
这个回滚脚本位于安装盘根目录的 `Config.Msi`（隐藏+系统目录），常见成因：

- 上一次安装被中断/被杀，留下了残缺的 `Config.Msi`（**重复安装时最容易遇到**）；
- 杀毒软件锁住或改写了 `Config.Msi` 里的 `.rbf` 文件；
- 该目录里的文件属于已不存在的账户（换过用户、清理过账户）。

处理顺序（每步都需要管理员权限）：

1. 先卸掉旧产品：`msiexec /x <ProductCode或MSI路径> /qn`（从"应用和功能"里卸载也行）；
2. 停掉 Installer 服务：`net stop msiserver`；
3. 清掉残留目录（两个盘都要看，报错里写的是哪个盘就是哪个）：
   `attrib -h -s C:\Config.Msi` / `attrib -h -s D:\Config.Msi`，然后 `rd /s /q C:\Config.Msi`；
   删不动就重命名为 `Config.Msi.old` 后重启一次；
4. 重新安装。失败时带日志重试，日志里会直接写出是哪一步失败：
   `msiexec /i ClipboardX-<版本>-x64.msi /l*v %TEMP%\clipboardx-msi.log`

### 9.2 `已安装更高版本的 ClipboardX`

nightly 的版本号是 `0.1.<一年中的第几天>.<构建序号>`，**数字上大于正式版 `0.1.0`**，
因此"装过 nightly 之后再装正式版"曾会被直接拒绝。包已改为允许降级
（`MajorUpgrade AllowDowngrades="yes"`）：这种情况会先移除旧版本再安装你指定的版本。
