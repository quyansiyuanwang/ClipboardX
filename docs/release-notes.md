# ClipboardX 0.2.1

极度轻量的三端剪贴板同步：单进程守护、默认加密、可自建服务端，也可用 WebDAV 或 S3。

## 本次更新（0.1.1）

- 终端界面：后端健康面板、内容摘要按需加载、`/` 过滤、预览滚动（`J`/`K`、`Ctrl+D/U`）、滚轮翻页、鼠标点行选中
- 顶部状态行在后端失败时直接给出下一步（按 `b` 看错误与退避时间），正常时不占版面
- 新增滚动 nightly 构建（draft），版本号改用构建序号而不是提交数

## 本次更新（0.2.0）

- 界面内配置后端：表单收集、保存前做真实连通性自测、失败保留草稿
- 后端管理面板：列出每个后端的健康、地址与错误，可停用/启用、可移除（移除只删本机配置与凭据）
- 本机历史里可删除单条（`cx delete --yes` / 界面里 `d`，破坏性操作先确认）
- 粘贴整段文本按字段处理并去掉末尾换行；秘密字段原样保留
- 后端名与整份配置在写盘前统一校验（拒绝能逃出 `secrets/` 的名字）
- 恢复 TLS 信任（改用原生根证书）、纠正 OSS 寻址：拒绝桶域名端点、按端点推断区域、显式 virtual-host
- 自建服务端：表单里填注册密钥时顺手完成设备注册
- 打包：允许覆盖/降级安装，发布版可以替换 nightly

## 本次更新（0.2.1）

- **加密门禁 fail-closed**：默认 `encrypt = true` 且没有空间密钥时，不同步到远端也不开局域网，而不是悄悄退化成明文
- S3 支持 STS 会话令牌（`--session-token-stdin` / `session_token_file`），临时凭据不再需要写进配置
- 保留策略清理失败在状态里可见（不再只写 debug 日志），且不触发退避/游标回滚
- macOS 卸载脚本同时移除 `cx` 与 `cx-server`，文档补齐三端卸载步骤
- 端到端脚本改用显式 `encrypt = false` 与非交互删除，CI 不再依赖终端交互

## 这个版本能做什么

- **三端同步**：Windows / macOS / Linux，同一份 `cx` 命令与控制协议；
- **三种后端**：自建 `cx-server`、WebDAV（Nextcloud 等）、S3 兼容对象存储（AWS/MinIO/R2）；
- **默认加密**：内容用 XChaCha20-Poly1305 加密后才上传，服务端与对象存储只见密文；
- **防乒乓**：三层防回环（剪贴板指纹去重、抑制表 TTL、已处理集合），不会 A→B→A 弹回；
- **轻**：`cx.exe` 约 3 MB、`cx-server` 约 0.7 MB，空闲时 CPU 近 0。

## 安装

| 平台 | 文件 | 安装 |
|---|---|---|
| Windows | `ClipboardX-<版本>-x64.msi` | 双击安装（per-user，不需要管理员） |
| macOS | `ClipboardX-<版本>.dmg` | 打开后运行其中的 `.pkg`；卸载见包内说明 |
| Linux | `clipboardx_<版本>_amd64.deb` | `sudo dpkg -i`（或 `apt install ./*.deb` 自动补依赖） |

未签名的产物会被系统拦一下：Windows 上点「更多信息 → 仍要运行」，macOS 上右键「打开」，
或 `xattr -d com.apple.quarantine /usr/local/bin/cx`。原因与补签名的方式见 `docs/packaging.md`。

## 校验下载

```bash
sha256sum -c SHA256SUMS        # Linux
shasum -a 256 -c SHA256SUMS    # macOS
Get-FileHash <文件> -Algorithm SHA256   # Windows PowerShell
```

## 上手

```bash
cx start                 # 后台运行（登录自启：cx service install）
cx status                # 查看状态与各后端
cx history               # 本机历史
cx push --text '你好'     # 手动发布一条
```

自建服务端：

```bash
cx-server space create 家庭空间 --quota 512MB     # 输出空间 id 与注册密钥
cx-server serve --listen 0.0.0.0:8688            # 前台跑（systemd/Docker 都按前台进程管理）
cx backend add server home http://<主机>:8688 --space <空间 id> --enroll-secret <注册密钥>
cx space init --passphrase-stdin && cx space publish home
```

## 已知限制

- 空间密钥的**密封下发**（用设备公钥加密 K）尚未实现，接口返回 501；当前用口令 + `cx space join`；
- 首版产物**未签名**（需要证书才能签，见 `docs/packaging.md` §4）；
- 部分设置仍需手改 `config.toml`（CLI 尚未提供全部子命令）。
