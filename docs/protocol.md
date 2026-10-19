# 同步协议与落盘格式 v1

本文是各实现（客户端、`cx-server`、WebDAV/S3 后端）之间的**冻结契约**。
改这里就是改协议，必须同步升级 `STORE_LAYOUT_VERSION`。

## 1. 为什么是"对象布局"而不是"协议"

WebDAV、S3 与自建服务端能力差异很大，能同时满足三者的最小公共集就是：
**列出前缀下的对象、按名字读写不可变对象**。因此同步语义全部编码进
对象的命名与内容里，传输层只负责"列目录 + 读写文件"。

三者的映射：

| 概念 | cx-server | WebDAV | S3 |
|---|---|---|---|
| 列目录 | `GET /v1/manifests` | `PROPFIND` + `Depth: 1` | `GET /?list-type=2&prefix=` |
| 写对象 | `PUT /v1/items/{name}` | `PUT {url}` | `PUT /{bucket}/{key}` |
| 读对象 | `GET /v1/items/{name}` | `GET {url}` | `GET /{bucket}/{key}` |
| 删除 | `DELETE /v1/items/{name}` | `DELETE {url}` | `DELETE /{bucket}/{key}` |
| 变更检测 | 响应 `ETag` | `getetag` 属性 | `ETag` + `LastModified` |
| 条件写 | `If-Match` / `If-None-Match` | `If-None-Match: *` | `If-None-Match: *`；阿里云 OSS 用等价的 `x-oss-forbid-overwrite: true`（409 = 已存在）。两者都不支持时**拒绝**降级为 HEAD 后 PUT |

> 条件写的两种写法是同一个语义（"只有对象不存在时才写"），必须保持原子：
> AWS / MinIO / R2 认 `If-None-Match: *`（已存在返回 412），阿里云 OSS **不认**这个头，
> 会回 `400 NotImplemented（A header you provided implies functionality that is not
> implemented）`，它的等价物是 `x-oss-forbid-overwrite: true`（已存在返回
> `409 FileAlreadyExists`）。端点认得出是 OSS 时直接用后者；认不出（CNAME / 自建域名）
> 就先按 AWS 风格试，撞上那个 NotImplemented 再换过来并记住——真实 OSS 验收
> （`scripts/e2e-oss.ps1`）覆盖了这条路径。

## 2. 目录布局

```
<root>/
├── cx-space.json                      # 空间元数据（明文，不含密钥）
└── v1/
    ├── devices/<device_id>.json       # 设备心跳：名字、系统、公钥、最后在线时间
    ├── manifests/<device_id>.json     # 每个设备自己的变更清单（只由该设备写）
    └── items/<yyyyMMdd>/<name>        # 不可变内容对象
```

- `<root>` = WebDAV 的集合路径 / S3 的 `bucket + prefix` / cx-server 的空间目录。
- `<device_id>` = 16 字节随机值的 base32（无填充、小写），全局唯一、不随机器名变化。
- `<yyyyMMdd>` = 由 `created_ms`（UTC）派生的日期，便于按天整段裁剪。
- `<name>` = `<created_ms:013>-<device8>-<hash8>.cxi`
  - `created_ms` 固定 13 位十进制（零填充）⇒ 字典序即时间序，可直接按名字比较先后
  - `device8` = 设备 id 前 8 字符，`hash8` = 内容哈希前 8 字符（十六进制）
  - 名字唯一且不可变：任何设备都不会覆盖别人的对象，**没有写竞争**
- 名字还作为载荷加密的 AAD，因此对象不能被改名（改名会导致解密失败）

### 2.1 为什么用 manifest 而不是直接列 items

轮询成本必须与内容量无关：

- 直接列 `items/`：对象数随历史线性增长，S3 LIST 费用与 WebDAV PROPFIND 响应体都会膨胀。
- 列 `manifests/`：对象数 = 设备数（通常 2–5 个），每次轮询一次 LIST + 少量 GET（ETag 未变则跳过）。

因此**每个设备只写自己的 manifest**：既避免了读-改-写竞争，也让拉取变成"对比已知 device 的 ETag"。

## 3. manifest 格式

`v1/manifests/<device_id>.json`，UTF-8 JSON：

```json
{
  "v": 1,
  "device_id": "k3v9…",
  "device_name": "workstation",
  "updated_ms": 1759300000000,
  "items": [
    {
      "name": "1759299998712-ab12cd34-9f3c1e77.cxi",
      "day": "20250930",
      "size": 348,
      "hash": "9f3c1e77…（hex，64 字符）",
      "kind": "text",
      "created_ms": 1759299998712,
      "mime": "text/plain; charset=utf-8"
    }
  ]
}
```

- `items` 只保留该设备的最近 N 条（默认 200），按 `created_ms` 升序。
- 裁剪 manifest 时不删除"其他设备尚未确认看到"的条目：见 §6。
- `kind` ∈ `text` | `image`（后续可扩展 `html`、`files`）。

## 4. item 容器格式（`*.cxi`）

```
偏移  长度  内容
0     4     魔数 "CXI1"
4     1     容器版本 = 1
5     1     kind（0=text, 1=image）
6     1     cipher（0=明文, 1=XChaCha20-Poly1305）
7     1     保留（必须为 0）
8     4     meta_len（小端 u32）
12    meta_len   meta（postcard 编码）
12+meta_len …  payload（可能已加密）
```

`meta` 字段（**明文但无内容**，因此可以被任何人读取而不泄漏剪贴板内容）：

| 字段 | 说明 |
|---|---|
| `id` | 16 字节 item id（随机） |
| `origin_device` | 产生该内容的设备 id |
| `origin_name` | 设备名（仅用于展示"来自哪台机器"） |
| `created_ms` | 产生时间（毫秒，UTC） |
| `seq` | 该设备内单调递增序号（用于时钟回拨时保持顺序） |
| `kind` | 与容器头一致 |
| `mime` | 可选，如 `text/plain; charset=utf-8` |
| `plain_hash` | `blake3(payload 明文)`，32 字节 |
| `plain_len` | payload 明文长度 |
| `nonce` | 加密时为 24 字节随机 nonce，明文时全 0 |

约定：

- **哈希口径**：`plain_hash = blake3(payload 明文)`。文本 payload = UTF-8 原文（默认不做换行归一化）；
  图片 payload = 由 RGBA 转码出的 PNG 字节。同机同内容哈希稳定，跨机同一张图若 RGBA 完全一致也稳定，
  否则会被视为两条内容（不会造成回环，只是多一条历史）。
- **加密**：XChaCha20-Poly1305，密钥 = 空间密钥 `K`（32 字节），AAD = `<name>`（对象文件名）。
  解密时先校验 `plain_hash`，不一致即丢弃并计数。
- **大小上限**：默认 4 MiB（`max_item_bytes`）。超限内容跳过并计入 `cx status` 的跳过计数，不阻塞其他条目。

## 5. 空间密钥与加入空间

`cx-space.json`（**明文**，可以放在公共桶里）：

```json
{
  "v": 1,
  "space_id": "…（16 字节 base32）",
  "cipher": "xchacha20poly1305",
  "kdf": { "algo": "argon2id", "salt": "…base64…", "m_cost": 19456, "t_cost": 2, "p_cost": 1 },
  "wrapped_key": "…base64（AEAD 封装的 K）"
}
```

- `K` 由 `argon2id(口令, salt, 参数)` 派生的密钥包裹后存放 ⇒ 只有知道口令的设备能解出 `K`。
- 服务端与对象存储**永远拿不到口令**，因此也就拿不到 `K`（零知识）。
- 加入空间：`cx space join --backend <name>`，读取 `cx-space.json`，提示输入口令，解出 `K` 后写入本机 `space.key`（0600）。
- `cx space key export` 导出口令备份；`cx space init` 创建空间（随机 `K`，要求设置口令）。
- `space.encrypt = false` 可显式关闭加密（对象目录里就是可读文本），仅用于调试或需要第三方工具直读的场景。

`cx-server` 场景额外有 `enroll_secret`：服务端只保存它的哈希，用于设备注册；注册成功后服务端用
设备上报的 x25519 公钥密封下发 `K`（`GET /v1/spaces/{id}/key`），设备随后自行校验 `K` 能否解开已有 item。

## 6. 同步算法

每设备的本地状态：`seen`（已处理 item 名字的 LRU，容量 1024）、`suppress`（回声抑制表，见 §7）、
每后端游标 `{etag_of_manifest[device_id], items_ms}`。

**发布**（本机剪贴板变化后，去抖 200 ms）：

1. 读剪贴板 → 归一化（文本取 UTF-8；图片转 PNG）→ 计算 `plain_hash`。
2. 命中 `suppress` ⇒ 丢弃（这是回声，不是新内容）。
3. 命中 `seen` 或与"本机上次发布的内容"相同 ⇒ 丢弃（去重）。
4. 写本地历史，生成 item 对象名，按 `space.encrypt` 加密后写入各启用的后端（`If-None-Match: *`）。
5. 更新自己的 manifest 并 PUT（`If-Match` 失败则重读合并后重试）。

**拉取**（自适应轮询：近 5 分钟有活动时 5 s，空闲时 30 s，失败指数退避 1→300 s ±20%）：

1. LIST `v1/manifests/`，对每个 manifest 比较 `ETag`；未变则跳过。
2. 变化的 manifest：GET 并解析，取其中名字 ∉ `seen` 的条目。
3. 按 `(created_ms, device_id)` 升序排序，逐个 GET item → 校验魔数/版本/长度/`plain_hash` → 解密。
4. 最新一条按 `apply_remote` 策略写入本机剪贴板（`latest`｜`only_if_idle`｜`manual`），并写入 `suppress`。
5. 全部条目写入 `seen` 与本地历史；更新游标。
6. **不回传**：拉取到的内容不会再次发布到后端（默认不 reflect），这是防止 A→B→A 乒乓的关键。

**裁剪**（每设备只删自己的东西，每 6 小时一次）：

- 自己的 manifest 超过上限（默认 200 条 / 30 天）时裁掉最旧的条目，并删除对应 item 对象。
- 绝不删除 `created_ms` 比自己"最新已见 item"更新的对象（避免与并发写入竞争）。
- `cx-server` 侧另有空间级配额与保留策略，超出时返回 `507`，客户端据此暂停上传并提示。

## 7. 回声抑制与防乒乓

| 场景 | 处理 |
|---|---|
| 远端内容写入本机剪贴板后，watcher 立即触发 | 内容哈希进入 `suppress`（默认 8 条 / 30 s），命中即丢弃 |
| A 复制 → B 应用 → B 再发布 | 默认不 reflect，B 收到后不回传 |
| 连续两次相同内容 | `plain_hash` 相同 ⇒ 去重，不产生新 item |
| 本机主动复制了与远端相同的内容 | 同上去重；若 `suppress` 已过期则视为本机新复制，正常发布（幂等，对象名相同） |

## 8. 本地控制协议（CLI/TUI ↔ 守护进程）

- 监听 `127.0.0.1` 上 `daemon.json` 记录的端口（仅回环）。
- 首行认证：`{"auth":"<control.token 内容>","proto":1}`；令牌文件权限 0600，校验失败立即断开并按 IP 限速。
- 之后为 JSON Lines 请求/响应：

```json
{"id":1,"cmd":"status"}
{"id":1,"ok":true,"data":{"paused":false,"backends":[…]}}
{"id":2,"cmd":"history.list","args":{"limit":20}}
{"id":2,"ok":false,"error":{"code":"not_running","msg":"守护进程未运行"}}
```

- `cmd=subscribe` 后服务端持续推送 `{"event":"clip.new","data":{…}}`，供 TUI 实时刷新。
- `cmd=history.delete`（参数 `position`，0 = 最新）**只删本机历史**：不删后端对象、
  不改已发布清单、不通知其他设备——别处的副本仍然存在。删除后该对象名会留在
  "已处理"集合里，因此即使某个后端的游标落后、把它重新列出来，也不会再被应用回来。
  真正的多端删除需要墓碑与冲突规则（"我删了、他又发上来"算谁赢），尚未实现。
- TUI使用 `history.show.checked`、`copy.checked`、`history.delete.checked`，参数含
  `position` 与 `expected_name`。引擎在同一线程核对对象名后才执行；位置漂移返回
  `history_changed`。老守护进程不支持这些新命令时必须拒绝，不能回退到未校验操作。
  手动复制成功响应在实际剪贴板写入完成后产生，纯中继模式/写入失败返回错误。
- 远端游标使用 `progress_v=1` 和逐对象 `completed_names`；只有完整消费manifest才提交ETag。
  旧时间戳游标升级时重新扫描，缺失/坏对象保留待重试且不阻塞健康设备；运行时等待引擎
  成功解密、入库并保存已处理状态后才接受进度，暂停期间不消费。
- 每个后端的发件箱位于数据目录的 `outbox/<target+space fingerprint>/`，先落盘再唤醒worker，
  成功上传对象及manifest后确认移除。目标/前缀/空间改变不会自动转发旧积压；失败按真实退避重试。
- CLI 与 TUI 都不直接读写剪贴板；后端管理/只读连通测试可直接访问网络，日常历史操作通过守护进程。

## 9. cx-server REST 接口
### 9.1 两个"空间 id"是不同的东西（容易混淆，这里特意写清楚）

| 名字 | 形式 | 属于谁 | 作用 |
|---|---|---|---|
| **服务端空间 id** | 32 位十六进制 | `cx-server` | 存储租户：令牌、配额、对象目录都挂在它下面 |
| **客户端空间 id** | 16 字节 base32 | `cx-space.json` | 加密空间：决定数据加密密钥 K 与 AAD |

设备令牌标识服务端空间，而 `cx-space.json` 里的客户端空间 id 决定"能不能解开对方的密文"。
两台设备要能同步，**两者都必须一致**：前者由 `--space` 配置，后者由 `cx space join` 拉取。

### 9.2 设备注册与密钥获取的现状

- **自助注册**：`cx backend add server <名> <地址> --space <id> --enroll-secret <密钥>`
  会调用 `POST /v1/enroll` 换取设备令牌并写入 `secrets/<名>.token`（服务端只保存令牌哈希）。
  管理员也可以跳过自助注册，用 `cx-server token issue` 直接签发。
- **空间密钥**：当前走**口令**路径——`cx space init` 在有口令时把 K 用 argon2id 派生的密钥
  包裹后写进 `cx-space.json`，`cx space publish` 上传，`cx space join` 下载并用口令解出 K。
- `GET /v1/spaces/{id}/key`（用设备公钥密封下发 K）**尚未实现**，接口明确返回 `501`。
  宁可给出一个清晰的"未实现"，也不发布一个半成品的安全承诺。

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/v1/enroll` | `{space_id, enroll_secret, device_name, device_pubkey}` → `{device_token}` |
| GET | `/v1/spaces/{id}/key` | 用设备公钥密封的空间密钥（需 Bearer 令牌） |
| GET | `/v1/manifests` | 列出 `{name, etag, last_modified, size}` |
| GET | `/v1/manifests/{device_id}` | 取 manifest（支持 `If-None-Match`） |
| PUT | `/v1/manifests/{device_id}` | 写 manifest（可用 `If-Match` 乐观锁） |
| GET/PUT/DELETE | `/v1/items/{name}` | 不可变对象读写删 |
| GET | `/v1/items?since=<cursor>&wait=25` | 长轮询：有新对象时立即返回，超时返回空 |
| GET | `/healthz` | 存活探针 |
| GET | `/v1/version` | 服务端版本与协议版本 |
| GET | `/metrics` | 可选的 Prometheus 文本指标（无第三方依赖） |

状态码约定：`401` 令牌无效（客户端应提示重新注册）、`403` 空间不匹配、`404` 对象不存在、
`409` 条件写冲突（客户端重试合并）、`413` 对象超限、`507` 空间配额用尽。

TLS：默认由反向代理终止（Caddy / Nginx 配置见 `docs/packaging.md`）；
服务端自带 TLS 通过 `tls` feature 启用（`rustls` + PEM 证书）。

## 10. 局域网直连协议

同一个网段内的两台设备不经任何服务端直接互推内容。它是**可选**通路：后端负责
"跨网络、离线可达"，局域网负责"同一个 Wi-Fi 下更快、不经过服务器"，两者可同时启用
（同一份内容可能走两条路，由已处理集合去重）。

**发现**：mDNS 服务类型 `_clipboardx._tcp.local`，TXT 记录含
`v`（协议版本）、`name`（设备名）、`device_id`、`space`（**空间 id 的哈希前缀**，
不直接暴露空间 id）。发现是尽力而为，`[lan].peers` 的显式 `host:port` 永远可用。

**传输**：TCP，帧格式 `[u32 大端长度][Noise 密文]`，单帧上限 17 MiB
（不设上限的话，对端只要发 4 字节长度就能让我们分配 4 GB）。

**握手**：Noise `XXpsk3_25519_ChaChaPoly_BLAKE2s`

- `XX`：双方交换并认证静态公钥（事先不需要知道对方公钥）；
- `psk3`：第三条消息混入 PSK，**PSK 由空间密钥派生**（`blake3::derive_key`，
  上下文串 `ClipboardX LAN psk v1`）⇒ 只有同一空间内的设备能完成握手；
  未加密空间则必须显式配置 `pairing_code`，由它派生 PSK。

**消息**（postcard 编码）：

| 消息 | 方向 | 说明 |
|---|---|---|
| `Hello{device_id, device_name, space_id}` | 双向 | 握手后交换身份；**空间不一致即断开** |
| `Manifest{device_id, entries}` | 双向 | "我有哪些内容"，用于双向补齐 |
| `Want{names}` | 请求方 | 我要这些对象 |
| `Item{name, bytes}` | 提供方 | 一条内容（**本身已是密文**） |
| `Ping` / `Pong` | 双向 | 保活（10 秒无发送则发 Ping） |
| `Bye` | 双向 | 主动断开 |

**信任**：首次见到某 `device_id` 时记录其静态公钥（`lan-peers.json`）；此后公钥变化则
拒绝连接——防中间人与"冒充已知设备"。用户确认无害后可删除该条目重新信任。

**安全层次**：① Noise 提供传输加密与对端认证；② item 本身在客户端就用空间密钥加密过，
因此即使传输层被攻破，内容仍是密文；③ 对端空间校验与 TOFU 固定。