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

.\scripts\build-msi.ps1        # 产出 dist\ClipboardX-<v>-x64.msi（WiX v4，版本锁定在仓库内）
.\scripts\verify-msi.ps1 -Msi dist\ClipboardX-0.1.0-x64.msi
```

```bash
bash packaging/macos/build-dmg.sh      # 仅 macOS
bash packaging/linux/build-deb.sh      # 需要 dpkg-deb
```

## 许可证

MIT OR Apache-2.0
