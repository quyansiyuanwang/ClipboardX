# ClipboardX

极度轻量的三端（Windows / macOS / Linux）剪贴板同步工具：Rust 实现，守护进程常驻后台，
TUI 界面，支持自建服务端，也支持直接用 WebDAV 或 S3 作为同步后端。

## 设计要点

- **不用 async 运行时**：单线程状态机 + 阻塞线程 + channel，空闲时 CPU 近 0、内存占用小。
- **内容不出明文**：载荷用 XChaCha20-Poly1305 端到端加密，服务端与对象存储只见密文。
- **后端即存储**：`cx-server`、WebDAV（Nextcloud 等）、S3（MinIO / R2 / AWS）共用同一套对象布局。
- **前端可替换**：TUI 与 CLI 都只通过本地控制套接字访问守护进程，不直接碰剪贴板与网络。
- **两个二进制**：`cx`（守护进程 + CLI + TUI）、`cx-server`（自建服务端）。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/protocol.md](docs/protocol.md) | 对象布局 v1、item 容器格式、同步算法、回声抑制、控制协议、服务端 REST |

| [docs/packaging.md](docs/packaging.md) | MSI / DMG / deb 构建方式、三端自启机制、签名公证、实测记录与踩坑 |


## 当前状态


|---|---|---|











## 开发

```powershell
cargo build --release --bin cx
cargo test
cargo clippy --workspace --all-targets -- -D warnings

.\scripts\check.ps1            # 提交前门禁：fmt + clippy + test（含跨平台 clippy）
.\scripts\e2e-local.ps1        # 端到端：守护进程 + CLI + 真实剪贴板（会临时改写剪贴板并恢复）
.\scripts\e2e-local.ps1 -NoClipboard   # 无头版本（CI 用）
.\scripts\e2e-webdav.ps1       # 端到端：两台守护进程经真实 WebDAV（wsgidav）互相同步
.\scripts\e2e-s3.ps1           # 端到端：两台守护进程经 S3 兼容存储（moto）互相同步
.\scripts\e2e-server.ps1       # 端到端：自建服务端 MVP（注册→建空间→加入→双设备同步→服务端只有密文）
.\scripts\e2e-lan.ps1          # 端到端：局域网直连（不配任何后端，两台守护进程互推；错配对码者收不到）
.\scripts\e2e-hardening.ps1    # 加固验收：体积预算、密钥 0600、明文不落盘、空闲 CPU/内存实测
.\scripts\build-msi.ps1        # 产出 dist\ClipboardX-<v>-x64.msi（WiX v4，版本锁定在仓库内）
.\scripts\verify-msi.ps1 -Msi dist\ClipboardX-0.1.0-x64.msi
```

日常用法：

```powershell
cx start            # 后台挂载守护进程（登录自启用 cx service install）
cx status           # 设备、后端、历史、计数
cx history --limit 20
cx copy 0           # 把最新一条写回剪贴板
cx pause / cx resume
cx doctor           # 守护进程起不来时也能跑（本地回退检查）
cx stop
```

```bash
bash packaging/macos/build-dmg.sh      # 仅 macOS
bash packaging/linux/build-deb.sh      # 需要 dpkg-deb
```

## 许可证

MIT OR Apache-2.0
