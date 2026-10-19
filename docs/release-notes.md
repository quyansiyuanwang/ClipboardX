极度轻量的三端剪贴板同步：单进程守护、默认加密、可自建服务端，也可用 WebDAV 或 S3。

<!--
维护约定：本文件只放**不随版本变化**的内容（能力概览、安装、校验、上手、已知限制）。
「本次更新」由 .github/workflows/release.yml 按「上一个 tag..本次 tag」的提交自动生成并
拼在最前面，因此这里绝不能手写某个版本的更新条目——一旦写进去，下一版就会把上一版的段落
一起带上，Release 正文里就会出现一串重复的「本次更新」（v0.1.1–v0.3.0 踩过这个坑）。
-->

## 这个版本能做什么

- **三端同步**：Windows / macOS / Linux，同一份 `cx` 命令与控制协议；
- **三种后端**：自建 `cx-server`、WebDAV（Nextcloud 等）、S3 兼容对象存储（AWS/MinIO/R2/阿里云 OSS）；
- **默认加密**：内容用 XChaCha20-Poly1305 加密后才上传，服务端与对象存储只见密文；
- **防乒乓**：三层防回环（剪贴板指纹去重、抑制表 TTL、已处理集合），不会 A→B→A 弹回；
- **轻**：`cx.exe` 约 4 MB、`cx-server` 约 0.7 MB，空闲时 CPU 近 0。

## 安装

| 平台 | 文件 | 安装 |
|---|---|---|
| Windows | `ClipboardX-<版本>-x64.msi` | 双击安装（per-user，不需要管理员）；覆盖安装会自动停掉后台 `cx` 再装 |
| macOS | `ClipboardX-<版本>-universal.dmg` | 打开后运行其中的 `.pkg`；卸载见包内说明 |
| Linux | `ClipboardX_<版本>_amd64.deb` | `sudo dpkg -i`（或 `apt install ./*.deb` 自动补依赖） |

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
- 建好空间之前产生的历史**不会回溯上传**，只有之后的新条目会同步；
- 产物**未签名**（需要证书才能签，见 `docs/packaging.md` §4）。
