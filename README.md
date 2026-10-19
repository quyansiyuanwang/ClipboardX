# ClipboardX

极度轻量的三端（Windows / macOS / Linux）剪贴板同步工具：Rust 实现，守护进程常驻后台，
TUI 界面，支持自建服务端，也支持直接用 WebDAV 或 S3 作为同步后端。

## 设计要点

- **不用 async 运行时**：单线程状态机 + 阻塞线程 + channel，空闲 10 秒的 CPU 时间增量实测 0 ms、
  常驻内存约 6–11 MB。
- **内容不出明文**：载荷用 XChaCha20-Poly1305 端到端加密，服务端与对象存储只见密文。
- **后端即存储**：`cx-server`、WebDAV（Nextcloud 等）、S3（MinIO / R2 / AWS）共用同一套对象布局，
  换后端不用改引擎。
- **局域网直连**：同一网段内两台设备可以不经过任何服务端互推内容（Noise XX + PSK，
  PSK 由空间密钥派生，只有同一空间的设备能连上）。
- **前端可替换**：TUI 与 CLI 都只通过本地控制套接字访问守护进程，不直接碰剪贴板与网络。
- **两个二进制**：`cx`（守护进程 + CLI + TUI，约 3.9 MB）、`cx-server`（自建服务端，约 0.7 MB）。

## 安装

| 平台 | 文件 | 说明 |
|---|---|---|
| Windows | `ClipboardX-<版本>-x64.msi` | 双击安装（per-user，不需要管理员），可选加入 PATH、登录自启、桌面快捷方式 |
| macOS | `ClipboardX-<版本>-universal.dmg` | 打开后运行其中的 `.pkg`，安装到 `/usr/local/bin` 并注册 LaunchAgent |
| Linux | `clipboardx_<版本>_amd64.deb` | `sudo dpkg -i`（或 `apt install ./*.deb` 自动补依赖），系统级安装 + systemd 用户单元 |

自建服务端 `cx-server` 随包一起安装（它不依赖剪贴板与图形会话）。

### 卸载

| 平台 | 做法 |
|---|---|
| Windows | 「应用和功能」里卸载（或 `msiexec /x ClipboardX-<版本>-x64.msi`） |
| macOS | 挂载 DMG 后双击 `卸载.command`；DMG 丢了就手动删：`sudo rm -f /usr/local/bin/cx /usr/local/bin/cx-server /Library/LaunchAgents/com.clipboardx.agent.plist`，再 `sudo pkgutil --forget com.clipboardx.cli` |
| Linux | `sudo dpkg -r clipboardx` |

macOS 的 pkg **没有自带卸载器**，`卸载.command` 就是用来做这件事的（它还会问你是否
一并删除用户数据）。三个平台的卸载都只移除程序与自启项，**不动配置、空间密钥与剪贴板历史**：

- Windows：`%APPDATA%\ClipboardX`
- macOS：`~/Library/Application Support/ClipboardX`
- Linux：`~/.config/clipboardx` 与 `~/.local/share/clipboardx`

要彻底清干净，卸载后再手动删除上面这个目录。只想关掉开机自启、保留程序：`cx service uninstall`。

## Nightly 测试包

main 上的提交会产出滚动更新的 nightly：同一个 `nightly` 标签，**每次覆盖上一份**，
内容始终是 main 的最新提交，供人在真机上装来测试。它以 **draft** 发布，只对仓库协作者可见，
不等于正式发布。

- 每天 03:00（北京时间）自动构建一次（当天没有新提交就跳过）；也可以到
  Actions → Nightly → Run workflow 手动触发，手动触发一定按当前最新提交构建。
- 版本号用 `0.1.<一年中的第几天>.<构建序号>`：MSI / pkg / deb 的版本号都只接受数字段
  （semver 的预发布后缀喂不进 MSI），按天 + 构建序号递增既合法、又能区分每一天的每次构建，
  便于"今天的覆盖昨天的"这种升级测试。它大于正式版 `0.1.0`，因此从 nightly 装回正式版时
  apt 会提示降级，这是预期的。
- 三个产物都跑过与正式发布相同的安装验收（装得上、能运行、卸得干净），但未签名、
  且未经过完整人工验收。

## 上手

```bash
cx start                 # 后台挂载守护进程（登录自启：cx service install）
cx status                # 设备、后端、历史、各后端计数
cx tui                   # 全屏终端界面（历史、行内内容摘要、预览滚动、过滤、写回剪贴板）
cx history --limit 20
cx copy 0                # 把最新一条写回剪贴板
cx delete 0 --yes        # 删除最新一条（只删本机历史）；交互终端里省略 --yes 会先确认
cx pause / cx resume
cx doctor                # 守护进程起不来时也能跑（本地回退检查，含密钥权限检查）
cx stop
```

改设置不必手改文件（键名与范围都有校验，写盘前会整体检查一遍）：

```bash
cx config get                     # 列出全部可设置项与当前值（含写法提示）
cx config get lan.port            # 读一项
cx config set sync.poll_active_ms 1200   # 改一项（这类热重载：cx reload 即生效）
cx config set lan.port 8699               # 端口这类要重启：cx restart
cx config show                    # 想看整份 config.toml 就用它
```

可设置的键：`device.name`、`sync.{watch_interval_ms,debounce_ms,poll_active_ms,poll_idle_ms,
max_item_bytes,history_max_items,history_max_days,apply_remote,normalize_newlines,
ignore_blank_text,ignore_globs}`、`lan.{enabled,port,use_mdns}`。
`space.encrypt` 只能读不能从命令行关掉——关掉就是明文上传，要改请自己编辑文件并自负后果。

终端界面（`cx tui`）的按键：

| 按键 | 作用 |
|---|---|
| `j` / `k`、`↑` / `↓` | 上下一行 |
| `PgUp` / `PgDn` | 上下翻页 |
| `g` / `G`、`Home` / `End` | 跳到最新 / 最旧 |
| `回车` | 把选中项写回剪贴板 |
| `/` | 过滤（回车确定、`Esc` 取消）：匹配类型、来源、对象名、哈希、已加载的内容摘要 |
| `J` / `K`、`Ctrl+D` / `Ctrl+U`、`0` | 预览下滚 / 上滚一行、翻页、回到开头 |
| `空格` | 暂停 / 恢复同步 |
| `p` / `r` / `R` | 立即发布本机剪贴板 / 刷新 / 重新读取配置 |
| `d` | 删除选中项（会先确认）——只删本机历史，见下面的「已知限制」 |
| `a` | 添加同步后端（表单：类型 / 字段 / 密钥；密钥密文输入，保存前做真实连通性自测）。提交后**留在界面里**：成功回面板并选中新行，失败把草稿留在表单里（改一个字段就能重试）。S3 的「模板」是选择题（`←/→`：阿里云 OSS 北京/杭州、MinIO、Cloudflare R2、AWS S3），选中即填好端点 / 区域 / 寻址三样；「寻址」也是选择题，不用记 `virtual-host` 这种字面值。终端窄于 46 列时字段改成"标签一行、值一行" |
| `b` | **后端管理面板**：一张表（名称 / 类型 / 目标 / 发布·接收 / 状态 / 开关）＋ 连接与安全诊断。面板内一次性做完所有管理动作，不用退出界面：`↑↓` 选后端、`空格`或`t` 停用·启用、`x` 移除（先确认）、`a` 添加、`r` 重读诊断。窄终端（<62 列）自动收起"类型/目标/收发"三列，只留名称、状态与开关 |
| `?` / `q` | 帮助 / 退出 |

鼠标：滚轮翻列表，左键点某一行即选中它（点空白处不动光标）。头部第二行会在**没有配置后端**或**有后端在失败**时给出提示
（包括下一步该敲什么命令），正常时显示"后端 N 个正常｜最近成功 …"，不用专门去查。

列表左侧的 `↑` 表示本机发出、`↓` 表示来自别的设备、`·` 表示还没拿到状态；每行右侧的
内容摘要**按需加载**（只为当前可见的窗口向守护进程要），所以长历史也不会拖慢界面。
窄于 84 列的终端会自动改成上下布局。

配置同步后端（三种任选，也可同时用）：

```bash
# 自建服务端
cx-server space create 家庭空间 --quota 512MB    # 输出空间 id 与注册密钥
cx-server serve --listen 0.0.0.0:8688           # 前台跑（systemd / Docker 都按前台进程管理）
cx backend add server home http://<主机>:8688 --space <空间 id> --enroll-secret <注册密钥>
cx space init --passphrase-stdin && cx space publish home
# 另一台设备：cx space join home --passphrase-stdin

# WebDAV（Nextcloud 等）
cx backend add webdav dav https://cloud.example.com/remote.php/dav/files/<用户>/clipboardx --username <用户> --password-stdin

# S3 兼容（AWS / MinIO / R2 / 阿里云 OSS）
cx backend add s3 oss --bucket clipboardx --endpoint http://127.0.0.1:9000 --access-key-id <AK> --secret-stdin --path-style
# 阿里云 OSS：先在控制台创建桶并授权该前缀，自动采用 virtual-host（也可显式 --virtual-host）
# 运行 --secret-stdin 后从交互标准输入提供密钥，不把 Secret 字面量写入 shell 命令/历史。
cx backend add s3 oss --bucket <桶> --endpoint https://oss-cn-hangzhou.aliyuncs.com --region cn-hangzhou --prefix clipboardx --access-key-id <AccessKeyId> --secret-stdin --virtual-host

# STS 临时凭据：把会话令牌也走 stdin（长期 RAM Key 不需要这一步）
cx backend add s3 oss --bucket <桶> --endpoint https://oss-cn-hangzhou.aliyuncs.com --region cn-hangzhou --access-key-id <AccessKeyId> --secret-stdin --session-token-stdin --virtual-host

# 连上真实 OSS 之后，想验证"两台设备经真实对象存储互相同步"（加密空间、会自动清空专用前缀）：
#   $env:CX_OSS_ENDPOINT='https://oss-cn-beijing.aliyuncs.com'; $env:CX_OSS_BUCKET='clipboard-sync'
#   $env:CX_OSS_PREFIX='clip/_diag/sync'; $env:CX_OSS_REGION='cn-beijing'
#   $env:CX_OSS_ACCESS_KEY='<AccessKeyId>'; $env:CX_OSS_SECRET_KEY='<Secret>'
#   pwsh scripts/e2e-oss.ps1

cx backend list          # 查看已配置后端（可配置多个，会同时生效）
cx backend test <名字>    # 只读列举/元数据检查；不建桶、不验证写入或删除权限
```

默认要求加密：首次使用还需通过 `cx space init --passphrase-stdin` 创建空间并
`cx space publish <后端名>` 发布元数据，其他设备执行 `cx space join <后端名> --passphrase-stdin`。
未加入空间或密钥不可读时不会静默转为明文外发；界面仍可查看本机历史与诊断。
空间初始化/加入后需要重启守护进程以加载密钥和后端 worker。

上传任务先写入每个目标/空间绑定的本机发件箱，断网或重启后继续重试；暂停期间不上传积压。
修改后端地址、桶、前缀或空间后不会将旧目标的积压转发给新目标；旧队列保留供人工恢复，
不要为了排障直接清空历史或游标。此版本会对旧时间戳游标做有界重新扫描以补齐漏项。
零云后端不代表仅本机：启用 LAN 后仍可能与局域网设备同步。

> **注意**：新增或删除后端后需要**重启守护进程**（`cx stop && cx start`）才能生效——
> `cx reload` 目前只重载引擎侧配置，后端 worker 线程是启动时创建的（这一项正在改进）。

局域网直连（不经过服务端）：

```toml
[space]
encrypt = false                # 明文空间必须显式声明；默认 true 且没有空间密钥时局域网会被禁用

[lan]
enabled = true
pairing_code = "自定的配对码"   # 明文空间必须配；有空间密钥时由密钥派生 PSK
peers = ["192.168.1.23:8699"]   # 可选：mDNS 被禁的网络里用显式地址
```

## 已知限制

- **安装包未签名**：首次安装会被系统拦一下（Windows 点「更多信息 → 仍要运行」，macOS 右键「打开」）。
  补上证书后 CI 会自动签名/公证，流程不变，见 [docs/packaging.md](docs/packaging.md)。
- **空间密钥的密封下发未实现**：`GET /v1/spaces/{id}/key` 明确返回 501，当前用"口令 + `cx space join`"。
- **建好空间之前的历史不会回头上传**：只有之后的新剪贴板条目才会同步出去（想补齐需要单独的"重发历史"能力）。
- **STS 临时凭据只支持静态配置**：可以通过 `--session-token-stdin`（TUI 表单里是"会话令牌"）
  或 `session_token_file` / `session_token_env` 提供，令牌会参与签名；但**不会自动续期**，
  过期后需要重新配置并用 `cx restart` 重载，且报错只提示凭据问题，不会替你刷新。
- **删除只作用于本机历史**（`cx delete <序号>` / TUI 里按 `d`）：后端对象与其他设备上的副本不受影响，
  已同步出去的内容也不会被追回。多端删除同步需要墓碑与冲突规则，尚未实现。
- macOS 与 Linux 的行为只在 CI 上验证过，未在真机长期使用。

## 开发

```powershell
cargo build --release --bin cx --bin cx-server
cargo test
cargo clippy --workspace --all-targets -- -D warnings

.\scripts\check.ps1            # 提交前门禁：fmt + clippy + test（含跨平台 clippy）

# 端到端脚本（每条都是"起真实进程/真实服务端"的验收，不依赖 mock）
.\scripts\e2e-local.ps1        # 守护进程 + CLI + 真实剪贴板（会临时改写剪贴板并恢复）
.\scripts\e2e-local.ps1 -NoClipboard   # 无头版本（CI 用）
.\scripts\e2e-webdav.ps1       # 两台守护进程经真实 WebDAV（wsgidav）互相同步
.\scripts\e2e-s3.ps1           # 两台守护进程经 S3 兼容存储（moto）互相同步
.\scripts\e2e-server.ps1       # 自建服务端（注册 → 建空间 → 加入 → 双设备同步 → 服务端只有密文）
.\scripts\e2e-lan.ps1          # 局域网直连（不配任何后端，两台守护进程互推；错配对码者收不到）
.\scripts\e2e-hardening.ps1    # 体积预算、密钥权限、明文不落盘、空闲 CPU/内存实测

.\scripts\build-msi.ps1        # 产出 dist\ClipboardX-<v>-x64.msi（WiX v4，版本锁定在仓库内）
.\scripts\verify-msi.ps1 -Msi dist\ClipboardX-0.1.0-x64.msi
.\scripts\verify-msi-upgrade.ps1   # 装旧版 → 装新版 → 只留一份 → 卸干净

bash packaging/macos/build-dmg.sh      # 仅 macOS
bash packaging/linux/build-deb.sh      # 需要 dpkg-deb
```

## 文档

| 文档 | 内容 |
|---|---|
| [docs/protocol.md](docs/protocol.md) | 对象布局 v1、item 容器格式、同步算法、回声抑制、控制协议、服务端 REST、局域网协议 |
| [docs/packaging.md](docs/packaging.md) | MSI / DMG / deb 构建方式、三端自启机制、签名与公证、发布流程 |

## 许可证

MIT OR Apache-2.0（二选一，各取所需：[MIT](LICENSE-MIT) 更简单且兼容 GPLv2，
[Apache-2.0](LICENSE-APACHE) 含明确的专利授权）。
