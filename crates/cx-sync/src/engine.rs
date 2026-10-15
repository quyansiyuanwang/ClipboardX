//! 同步引擎：单线程状态机。
//!
//! 为什么是"纯函数式"的：引擎**唯一持有可变状态**（抑制表、已处理集合、游标、
//! 历史索引、身份序号），外部世界的一切都以事件传入、以 [`Action`] 传回。
//! 因此没有锁，也没有"哪个线程改了状态"的问题；测试可以直接喂事件、断言动作序列，
//! 不需要线程、定时器或真实剪贴板。
//!
//! 三层防回环/防重复（见 `docs/protocol.md` §7）：
//! 1. 本机指纹去重：剪贴板内容与上次所见相同 ⇒ 不处理；
//! 2. 抑制表（TTL）：把远端内容写进本机剪贴板后，watcher 立刻触发的那次变化被丢弃；
//! 3. 已处理集合 + 内容哈希：同名对象不重复入库，同内容不重复上传。

use cx_clip::{ClipContent, ClipKind};
use cx_core::config::ApplyRemoteMode;
use cx_core::state::{Cursor, SEEN_CAPACITY};
use cx_core::{
    Config, CoreError, DaemonState, History, Identity, Paths, SeenSet, SpaceCipher, TtlSet,
};
use cx_proto::control::{
    DoctorCheck, DoctorReport, ErrorBody, Event as ControlEvent, HistoryPreview, HistorySummary,
    StatusReport, errcode, event,
};
use cx_proto::{Item, ItemMeta, ItemName, ManifestEntry, content_hash, content_hash_hex};
use serde_json::json;
use std::collections::BTreeMap;

/// 抑制表容量。
pub const DEFAULT_SUPPRESS_CAPACITY: usize = 8;

/// 抑制表条目存活时间（毫秒）。
pub const DEFAULT_SUPPRESS_TTL_MS: u64 = 30_000;

/// 输入事件。
#[derive(Debug)]
pub enum Event {
    /// watcher 读到的剪贴板内容（`None` 表示剪贴板为空或格式不受支持）。
    ClipboardChanged(Option<ClipContent>),
    /// 启动预热：只记录"当前剪贴板是什么"，**不发布**。
    ///
    /// 用途：重启后剪贴板里可能还是上次从别的设备同步过来的内容，若不预热，
    /// 首次读取会被当成"本机新内容"再上传一次（同名不同源，产生冗余对象）。
    Prime(Option<ClipContent>),
    /// 来自控制套接字的命令。
    Command(Command),
    /// 周期心跳：用于状态落盘与后端轮询。
    Tick,
}

/// 控制命令（都带请求 id，便于把结果原样回给发起者）。
#[derive(Debug)]
pub enum Command {
    /// 按稳定对象名核对历史请求；位置漂移时拒绝，不操作另一条内容。
    HistoryChecked {
        /// 请求 id。
        id: u64,
        /// 原位置。
        position: usize,
        /// 用户选择的稳定对象名。
        expected_name: String,
        /// show / copy / delete。
        operation: String,
        /// 预览字符上限。
        max_chars: usize,
    },
    /// 暂停同步。
    Pause {
        /// 请求 id。
        id: u64,
    },
    /// 恢复同步。
    Resume {
        /// 请求 id。
        id: u64,
    },
    /// 查询状态。
    Status {
        /// 请求 id。
        id: u64,
    },
    /// 列出历史。
    HistoryList {
        /// 请求 id。
        id: u64,
        /// 条数上限。
        limit: usize,
    },
    /// 查看某条历史的内容。
    HistoryShow {
        /// 请求 id。
        id: u64,
        /// 位置（0 = 最新）。
        position: usize,
        /// 文本预览的最大字符数。
        max_chars: usize,
    },
    /// 删除一条本机历史。只动本机：不删后端对象、不改已发布清单、不通知其他设备。
    HistoryDelete {
        /// 请求 id。
        id: u64,
        /// 位置（0 = 最新）。
        position: usize,
    },
    /// 把历史内容写回本机剪贴板。
    Copy {
        /// 请求 id。
        id: u64,
        /// 位置（0 = 最新）。
        position: usize,
    },
    /// 立即发布当前（或指定的）内容。
    Push {
        /// 请求 id。
        id: u64,
        /// 指定内容；`None` 表示由守护进程重新读取剪贴板。
        content: Option<ClipContent>,
        /// 是否忽略"与上次相同"的判断。
        force: bool,
    },
    /// 立即拉取一次。
    Pull {
        /// 请求 id。
        id: u64,
    },
    /// 本机诊断。
    Doctor {
        /// 请求 id。
        id: u64,
    },
    /// 重载配置。
    Reload {
        /// 请求 id。
        id: u64,
        /// 新配置。
        config: Box<Config>,
    },
    /// 收到远端对象（由后端 worker 线程投递）。
    ApplyRemote {
        /// 来自哪个后端（用于分别统计与退避）。
        backend: String,
        /// 对象名。
        name: String,
        /// 解码后的 item。
        item: Box<Item>,
    },
    /// 后端 worker 汇报一轮结果（游标、计数、错误）。
    BackendReport {
        /// 后端名。
        name: String,
        /// 各设备的 manifest 游标（由 worker 维护，交引擎持久化）。
        cursors: BTreeMap<String, Cursor>,
        /// 该 worker 累计发布条数。
        published: u64,
        /// 该 worker 累计应用条数。
        applied: u64,
        /// 本轮错误（`None` 表示正常）。
        error: Option<String>,
    },
}

/// 引擎产出的动作（由守护进程执行）。
#[derive(Debug)]
pub enum Action {
    /// 把内容写入本机剪贴板。
    ApplyToClipboard(ClipContent),
    /// 手动复制：真正写入剪贴板后才响应客户端。
    CopyToClipboard {
        /// 请求 id。
        id: u64,
        /// 已解密并核对身份的内容。
        content: ClipContent,
    },
    /// 发布对象到各后端（由守护进程的 worker 线程消费）。
    Publish {
        /// 对象名。
        name: String,
        /// manifest 条目（含哈希/大小/类型，worker 据此维护 manifest）。
        entry: ManifestEntry,
        /// 容器字节。
        bytes: Vec<u8>,
    },
    /// 回给控制层的响应。
    Reply(ReplyPayload),
    /// 推送给订阅者的事件。
    Notify(ControlEvent),
    /// 立刻拉取一次后端。
    PollNow,
    /// 请守护进程重新读取剪贴板并作为事件送回（resume / push 用）。
    RecheckClipboard,
    /// 落盘状态。
    SaveState,
    /// 日志。
    Log {
        /// 级别。
        level: log::Level,
        /// 文本。
        message: String,
    },
}

/// 类型化的响应载荷。
#[derive(Debug)]
pub enum ReplyPayload {
    /// `status`：引擎部分字段由守护进程补齐（pid、剪贴板描述等）。
    Status {
        /// 请求 id。
        id: u64,
        /// 报告（部分字段留给守护进程填写）。
        report: Box<StatusReport>,
    },
    /// `history.list`。
    HistoryList {
        /// 请求 id。
        id: u64,
        /// 条目。
        items: Vec<HistorySummary>,
    },
    /// `history.show`。
    HistoryShow {
        /// 请求 id。
        id: u64,
        /// 预览。
        preview: Box<HistoryPreview>,
    },
    /// `doctor`。
    Doctor {
        /// 请求 id。
        id: u64,
        /// 报告。
        report: Box<DoctorReport>,
    },
    /// 成功但无数据。
    Done {
        /// 请求 id。
        id: u64,
    },
    /// 失败。
    Error {
        /// 请求 id。
        id: u64,
        /// 错误体。
        body: ErrorBody,
    },
}

/// 同步引擎。
pub struct Engine {
    paths: Paths,
    config: Config,
    identity: Identity,
    /// 载荷加密器（未加入空间时为明文）。
    cipher: SpaceCipher,
    /// 空间 id（未加入空间时为 `None`）。
    space_id: Option<String>,
    history: History,
    state: DaemonState,
    suppress: TtlSet,
    seen: SeenSet,
    /// 最近一次观察到的剪贴板指纹。
    last_fingerprint: Option<String>,
    /// 最近一次发布的载荷指纹（用于 `only_if_idle` 判断）。
    last_published_fingerprint: Option<String>,
    /// 上次状态落盘时间。
    last_save_ms: u64,
}

impl Engine {
    /// 装配引擎：打开历史、恢复状态、准备抑制表与已处理集合。
    pub fn new(
        paths: &Paths,
        config: Config,
        identity: Identity,
        space: Option<cx_core::LocalSpace>,
    ) -> Result<Self, CoreError> {
        let history = History::open(paths)?;
        let state = DaemonState::load(paths)?;
        let seen = SeenSet::from_names(SEEN_CAPACITY, state.seen_items.iter().map(String::as_str));
        let (cipher, space_id) = match space {
            Some(space) => {
                let id = space.meta.space_id.clone();
                (space.cipher, Some(id))
            }
            None => (SpaceCipher::Plaintext, None),
        };

        Ok(Self {
            paths: paths.clone(),
            config,
            identity,
            cipher,
            space_id,
            history,
            state,
            suppress: TtlSet::new(DEFAULT_SUPPRESS_CAPACITY, DEFAULT_SUPPRESS_TTL_MS),
            seen,
            last_fingerprint: None,
            last_published_fingerprint: None,
            last_save_ms: 0,
        })
    }

    /// 当前配置。
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 是否允许外发内容。要求加密时，密钥未就绪必须失败关闭，不能静默明文降级。
    pub fn remote_ready(&self) -> bool {
        !self.config.space.encrypt || self.cipher.space_key().is_some()
    }

    /// 设备身份。
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// 历史条数。
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// 是否暂停。
    pub fn is_paused(&self) -> bool {
        self.state.paused
    }

    /// 处理一个事件，返回需要外部执行的动作。
    pub fn handle(&mut self, now_ms: u64, event: Event) -> Vec<Action> {
        match event {
            Event::ClipboardChanged(content) => self.on_clipboard_changed(now_ms, content),
            Event::Prime(content) => {
                self.last_fingerprint = Some(match content {
                    Some(content) => content.fingerprint_hex(),
                    None => "<empty>".to_string(),
                });
                Vec::new()
            }
            Event::Command(command) => self.on_command(now_ms, command),
            Event::Tick => {
                let mut actions = Vec::new();
                if self
                    .history
                    .needs_compaction(self.config.sync.history_max_items)
                {
                    match self.history.compact(self.config.sync.history_max_items) {
                        Ok(kept) => actions.push(Action::Log {
                            level: log::Level::Info,
                            message: format!("历史已压缩，保留 {kept} 条"),
                        }),
                        Err(err) => actions.push(Action::Log {
                            level: log::Level::Warn,
                            message: format!("历史压缩失败：{err}"),
                        }),
                    }
                }
                if now_ms.saturating_sub(self.last_save_ms) > 5 * 60_000 {
                    actions.push(Action::SaveState);
                }
                actions
            }
        }
    }

    fn on_clipboard_changed(&mut self, now_ms: u64, content: Option<ClipContent>) -> Vec<Action> {
        let Some(content) = content else {
            // 剪贴板被清空：记下这个状态，避免下次相同内容被误判为"新"
            self.last_fingerprint = Some("<empty>".to_string());
            return Vec::new();
        };

        let fingerprint = content.fingerprint_hex();
        if self.last_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            return Vec::new();
        }
        self.last_fingerprint = Some(fingerprint.clone());

        if self.state.paused {
            // 暂停期间的变化不发布；恢复时会重新读一次剪贴板
            return vec![Action::Notify(ControlEvent::new(
                event::CLIP_SKIPPED,
                json!({ "reason": "paused", "kind": content.kind().as_str() }),
            ))];
        }

        if self.suppress.contains(&fingerprint, now_ms) {
            // 这是"我们把远端内容写进本机剪贴板"引起的回声
            log::debug!("检测到回声（{}），已抑制", content.describe());
            return Vec::new();
        }

        self.publish(now_ms, content, false)
    }

    fn on_command(&mut self, now_ms: u64, command: Command) -> Vec<Action> {
        match command {
            Command::HistoryChecked {
                id,
                position,
                expected_name,
                operation,
                max_chars,
            } => {
                let matches = self
                    .history_summaries(position.saturating_add(1))
                    .get(position)
                    .is_some_and(|item| item.name == expected_name);
                if !matches {
                    return vec![Action::Reply(ReplyPayload::Error {
                        id,
                        body: ErrorBody {
                            code: "history_changed".into(),
                            msg: "历史已变化，操作已取消；请刷新后重新选择".into(),
                        },
                    })];
                }
                let command = match operation.as_str() {
                    "show" => Command::HistoryShow {
                        id,
                        position,
                        max_chars,
                    },
                    "copy" => Command::Copy { id, position },
                    "delete" => Command::HistoryDelete { id, position },
                    _ => {
                        return vec![Action::Reply(ReplyPayload::Error {
                            id,
                            body: ErrorBody {
                                code: "bad_args".into(),
                                msg: "未知历史操作".into(),
                            },
                        })];
                    }
                };
                self.on_command(now_ms, command)
            }
            Command::Pause { id } => {
                self.state.paused = true;
                vec![
                    Action::Reply(ReplyPayload::Done { id }),
                    Action::Notify(ControlEvent::new(
                        event::STATUS_CHANGED,
                        json!({"paused": true}),
                    )),
                    Action::SaveState,
                ]
            }
            Command::Resume { id } => {
                self.state.paused = false;
                // 恢复时允许重新发布：清掉"上次所见"，并请守护进程重读剪贴板
                self.last_fingerprint = None;
                vec![
                    Action::Reply(ReplyPayload::Done { id }),
                    Action::Notify(ControlEvent::new(
                        event::STATUS_CHANGED,
                        json!({"paused": false}),
                    )),
                    Action::RecheckClipboard,
                    Action::SaveState,
                ]
            }
            Command::Status { id } => vec![Action::Reply(ReplyPayload::Status {
                id,
                report: Box::new(self.status_skeleton()),
            })],
            Command::HistoryList { id, limit } => {
                let items = self.history_summaries(limit);
                vec![Action::Reply(ReplyPayload::HistoryList { id, items })]
            }
            Command::HistoryShow {
                id,
                position,
                max_chars,
            } => match self.history_preview(position, max_chars) {
                Ok(preview) => vec![Action::Reply(ReplyPayload::HistoryShow {
                    id,
                    preview: Box::new(preview),
                })],
                Err(err) => vec![Action::Reply(ReplyPayload::Error {
                    id,
                    body: error_body(&err),
                })],
            },
            Command::HistoryDelete { id, position } => match self.delete_from_history(position) {
                Ok((remaining, name)) => {
                    log::info!("已删除历史条目 {name}（剩余 {remaining} 条）");
                    vec![Action::Reply(ReplyPayload::Done { id })]
                }
                Err(err) => vec![Action::Reply(ReplyPayload::Error {
                    id,
                    body: error_body(&err),
                })],
            },
            Command::Copy { id, position } => match self.copy_from_history(now_ms, position) {
                Ok(actions) => actions
                    .into_iter()
                    .map(|action| match action {
                        Action::ApplyToClipboard(content) => {
                            Action::CopyToClipboard { id, content }
                        }
                        other => other,
                    })
                    .collect(),
                Err(err) => vec![Action::Reply(ReplyPayload::Error {
                    id,
                    body: error_body(&err),
                })],
            },
            Command::Push { id, content, force } => {
                let Some(content) = content else {
                    // 由守护进程重读剪贴板后再走一次 ClipboardChanged
                    return vec![
                        Action::Reply(ReplyPayload::Done { id }),
                        Action::RecheckClipboard,
                    ];
                };
                let mut actions = self.publish(now_ms, content, force);
                actions.push(Action::Reply(ReplyPayload::Done { id }));
                actions
            }
            Command::Pull { id } => vec![Action::Reply(ReplyPayload::Done { id }), Action::PollNow],
            Command::Doctor { id } => vec![Action::Reply(ReplyPayload::Doctor {
                id,
                report: Box::new(self.doctor()),
            })],
            Command::Reload { id, config } => {
                if config.backends != self.config.backends
                    || config.space != self.config.space
                    || config.lan != self.config.lan
                {
                    return vec![Action::Reply(ReplyPayload::Error { id, body: ErrorBody {
                        code: "restart_required".into(), msg: "后端、空间或局域网配置已变化；请重启守护进程后生效，未将新配置套用到旧连接".into(),
                    } })];
                }
                self.config = *config;
                vec![
                    Action::Reply(ReplyPayload::Done { id }),
                    Action::Log {
                        level: log::Level::Info,
                        message: "配置已重载".to_string(),
                    },
                ]
            }
            Command::ApplyRemote {
                backend,
                name,
                item,
            } => {
                let mut actions = self.apply_remote(now_ms, &backend, &name, &item);
                let accepted = self.state.is_seen(&name) && self.flush_state().is_ok();
                actions.push(Action::Reply(if accepted {
                    ReplyPayload::Done { id: 0 }
                } else {
                    ReplyPayload::Error {
                        id: 0,
                        body: ErrorBody {
                            code: "apply_pending".into(),
                            msg: "对象未成功入库，稍后重试".into(),
                        },
                    }
                }));
                actions
            }
            Command::BackendReport {
                name,
                cursors,
                published,
                applied,
                error,
            } => self.on_backend_report(now_ms, &name, cursors, published, applied, error),
        }
    }

    /// 发布本机内容：忽略规则 → 体积 → 加密 → 入库 → 通知。
    fn publish(&mut self, now_ms: u64, content: ClipContent, force: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.state.paused {
            actions.push(Action::Notify(ControlEvent::new(
                event::CLIP_SKIPPED,
                json!({"reason":"paused"}),
            )));
            return actions;
        }

        // 忽略规则：空白文本与 glob 规则
        if let Some(text) = content.text() {
            if self.config.sync.ignore_blank_text && cx_core::pattern::is_blank(text) {
                self.state.skipped_ignored = self.state.skipped_ignored.saturating_add(1);
                actions.push(Action::Notify(ControlEvent::new(
                    event::CLIP_SKIPPED,
                    json!({ "reason": "blank_text" }),
                )));
                return actions;
            }
            if let Some(pattern) = self
                .config
                .sync
                .ignore_globs
                .iter()
                .find(|pattern| cx_core::pattern::glob_match(pattern, text))
            {
                self.state.skipped_ignored = self.state.skipped_ignored.saturating_add(1);
                log::debug!("内容命中忽略规则 {pattern}，跳过");
                actions.push(Action::Notify(ControlEvent::new(
                    event::CLIP_SKIPPED,
                    json!({ "reason": "ignore_glob", "pattern": pattern }),
                )));
                return actions;
            }
        }

        // 体积上限（用保守估计，避免为超大图片先编码一遍 PNG）
        if content.approx_len() as u64 > self.config.sync.max_item_bytes {
            self.state.skipped_oversize = self.state.skipped_oversize.saturating_add(1);
            log::info!(
                "{} 超过上限 {} 字节，跳过",
                content.describe(),
                self.config.sync.max_item_bytes
            );
            actions.push(Action::Notify(ControlEvent::new(
                event::CLIP_SKIPPED,
                json!({ "reason": "too_large", "approx_bytes": content.approx_len() }),
            )));
            return actions;
        }

        // 归一化载荷与哈希（图片在这一步编码为 PNG）
        let plain = match content.plain_bytes() {
            Ok(plain) => plain,
            Err(err) => {
                actions.push(Action::Log {
                    level: log::Level::Warn,
                    message: format!("内容编码失败，跳过：{err}"),
                });
                return actions;
            }
        };
        let hash = content_hash_hex(&plain);
        if !force && self.state.last_published_hash.as_deref() == Some(hash.as_str()) {
            return actions;
        }

        match self.build_item(now_ms, &content, &plain, &hash) {
            Ok((name, item, bytes)) => {
                if let Err(err) = self.history.append(&name, &item) {
                    actions.push(Action::Log {
                        level: log::Level::Warn,
                        message: format!("写入历史失败：{err}"),
                    });
                    return actions;
                }
                self.seen.insert(&name);
                self.state.mark_seen(&name);
                self.state.last_published_hash = Some(hash.clone());
                self.last_published_fingerprint = Some(content.fingerprint_hex());
                self.last_save_ms = now_ms;

                let entry = ManifestEntry {
                    name: name.clone(),
                    day: name
                        .split('-')
                        .next()
                        .and_then(|ts| ts.parse::<u64>().ok())
                        .map(cx_proto::civil::day_from_ms)
                        .unwrap_or_default(),
                    size: bytes.len() as u64,
                    hash: hash.clone(),
                    kind: item.kind,
                    created_ms: item.meta.created_ms,
                    mime: item.meta.mime.clone(),
                };
                if self.remote_ready() {
                    actions.push(Action::Publish {
                        name: name.clone(),
                        entry,
                        bytes,
                    });
                    actions.push(Action::Notify(ControlEvent::new(
                        event::CLIP_PUBLISHED,
                        json!({"name": name, "kind": content.kind().as_str(), "hash": hash,
                            "bytes": item.plain_len(), "summary": content.describe(), "stage": "queued"}),
                    )));
                } else {
                    actions.push(Action::Notify(ControlEvent::new(
                        event::CLIP_SKIPPED,
                        json!({"reason": "encryption_not_ready", "name": name, "local_history": true}),
                    )));
                }
                actions.push(Action::SaveState);
            }
            Err(err) => actions.push(Action::Log {
                level: log::Level::Warn,
                message: format!("构造对象失败：{err}"),
            }),
        }

        actions
    }

    /// 组装 item（含加密）并编码为容器字节。
    fn build_item(
        &mut self,
        now_ms: u64,
        content: &ClipContent,
        plain: &[u8],
        hash: &str,
    ) -> Result<(String, Item, Vec<u8>), CoreError> {
        let name = ItemName::new(now_ms, &self.identity.device_id, hash)?;
        let sealed = self.cipher.seal(&name.file_name(), plain)?;

        let mut id = [0u8; cx_proto::ID_LEN];
        getrandom::fill(&mut id).map_err(|e| CoreError::SpaceKey(e.to_string()))?;

        let (seq, persist_due) = self.identity.bump_seq();
        if persist_due {
            // 序号按批落盘：崩溃最多丢 31 个序号，不影响排序正确性
            self.identity.save(&self.paths)?;
        }

        let kind = content.kind().to_proto();
        let meta = ItemMeta {
            id: id.to_vec(),
            origin_device: self.identity.device_id.clone(),
            origin_name: self.identity.name.clone(),
            created_ms: name.created_ms,
            seq,
            kind,
            mime: Some(mime_for(kind).to_string()),
            plain_hash: content_hash(plain).to_vec(),
            plain_len: plain.len() as u32,
            nonce: sealed.nonce,
        };
        let item = Item::new(kind, sealed.cipher, meta, sealed.payload)?;
        let bytes = item.encode()?;
        Ok((name.file_name(), item, bytes))
    }

    /// 收到远端对象：校验 → 解密 → 入库 → 按策略应用。
    fn apply_remote(&mut self, now_ms: u64, backend: &str, name: &str, item: &Item) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.state.paused {
            return actions;
        }
        if self.seen.contains(name) || self.state.is_seen(name) {
            return actions;
        }

        let plain = match self.cipher.open(name, item) {
            Ok(plain) => plain,
            Err(err) => {
                actions.push(Action::Log {
                    level: log::Level::Warn,
                    message: format!("远端对象 {name} 无法解密/校验失败：{err}"),
                });
                return actions;
            }
        };

        let kind = match item.kind {
            cx_proto::Kind::Text => ClipKind::Text,
            cx_proto::Kind::Image => ClipKind::Image,
        };
        let content = match ClipContent::from_plain(kind, &plain) {
            Ok(content) => content,
            Err(err) => {
                actions.push(Action::Log {
                    level: log::Level::Warn,
                    message: format!("远端对象 {name} 载荷非法：{err}"),
                });
                return actions;
            }
        };

        if let Some(text) = content.text()
            && self.config.sync.ignore_blank_text
            && cx_core::pattern::is_blank(text)
        {
            self.seen.insert(name);
            self.state.mark_seen(name);
            return actions;
        }

        if let Err(err) = self.history.append(name, item) {
            actions.push(Action::Log {
                level: log::Level::Warn,
                message: format!("远端对象 {name} 写入历史失败：{err}"),
            });
            return actions;
        }
        self.seen.insert(name);
        self.state.mark_seen(name);
        // 只给"真正送来这条内容"的后端计数：否则多后端场景下计数会成倍虚高
        self.state.backend_mut(backend, true).count_applied(1);

        // 应用策略
        let fingerprint = content.fingerprint_hex();
        let idle = self.last_fingerprint == self.last_published_fingerprint;
        let should_apply = match self.config.sync.apply_remote {
            ApplyRemoteMode::Latest => true,
            ApplyRemoteMode::OnlyIfIdle => idle,
            ApplyRemoteMode::Manual => false,
        };

        if should_apply {
            self.suppress.insert(fingerprint.clone(), now_ms);
            self.last_fingerprint = Some(fingerprint);
            actions.push(Action::ApplyToClipboard(content));
            actions.push(Action::Notify(ControlEvent::new(
                event::CLIP_APPLIED,
                json!({ "name": name }),
            )));
        } else {
            actions.push(Action::Notify(ControlEvent::new(
                event::CLIP_SKIPPED,
                json!({ "reason": "manual_or_busy", "name": name }),
            )));
        }
        actions.push(Action::SaveState);
        actions
    }

    /// 把历史内容写回剪贴板（`cx copy`）。
    /// 删除一条本机历史，返回 `(剩余条数, 被删对象名)`。
    ///
    /// 删除后把名字留在已处理集合里：删除只作用于本机，而后端的游标如果还没越过它，
    /// 下一次轮询会把它重新拉回来——用户刚刚明确说不要它了，不该再冒出来。
    fn delete_from_history(&mut self, position: usize) -> Result<(usize, String), CoreError> {
        let removed = self.history.delete_at(position)?;
        self.seen.insert(&removed.name);
        self.state.mark_seen(&removed.name);
        let _ = self.state.save(&self.paths);
        Ok((self.history.len(), removed.name))
    }

    fn copy_from_history(
        &mut self,
        now_ms: u64,
        position: usize,
    ) -> Result<Vec<Action>, CoreError> {
        let item = self.history.read_newest(position)?;
        let name = self
            .history
            .newest_first(position + 1)
            .last()
            .map(|entry| entry.name.clone())
            .unwrap_or_default();
        let plain = if name.is_empty() {
            // 历史索引中没有对象名（重建过的索引）：直接按内容类型还原
            match item.kind {
                cx_proto::Kind::Text => item.payload.clone(),
                cx_proto::Kind::Image => item.payload.clone(),
            }
        } else {
            self.cipher.open(&name, &item)?
        };
        let kind = match item.kind {
            cx_proto::Kind::Text => ClipKind::Text,
            cx_proto::Kind::Image => ClipKind::Image,
        };
        let content =
            ClipContent::from_plain(kind, &plain).map_err(|e| CoreError::Clip(e.to_string()))?;
        let fingerprint = content.fingerprint_hex();

        // 写回剪贴板会触发 watcher：放进抑制表，避免把自己刚复制的内容再上传一遍
        self.suppress.insert(fingerprint.clone(), now_ms);
        self.last_fingerprint = Some(fingerprint);

        Ok(vec![Action::ApplyToClipboard(content)])
    }

    /// 状态骨架（守护进程会补齐 pid、剪贴板描述等字段）。
    fn status_skeleton(&self) -> StatusReport {
        let mut backends: Vec<cx_proto::control::BackendStatus> = self
            .config
            .backends
            .iter()
            .map(|backend| {
                let state = self.state.backend(&backend.name);
                let view = match state {
                    Some(state) => cx_proto::BackendStateView {
                        enabled: backend.enabled,
                        last_ok_ms: state.last_ok_ms,
                        last_error: state.last_error.clone(),
                        failures: state.consecutive_failures,
                        next_attempt_ms: state.next_attempt_ms,
                        published: state.published,
                        applied: state.applied,
                    },
                    None => cx_proto::BackendStateView {
                        enabled: backend.enabled,
                        ..Default::default()
                    },
                };
                cx_proto::control::backend_status(&backend.name, backend.kind.as_str(), &view)
            })
            .collect();
        // 给每个后端补一句"它是哪个"：界面上配了两个同类后端时，光看名字分不清。
        // 就地回填而不是另加自由函数——上一轮正是因为函数插入位置搞错而作废。
        for status in &mut backends {
            let Some(section) = self
                .config
                .backends
                .iter()
                .find(|section| section.name == status.name)
            else {
                continue;
            };
            status.detail = match section.kind {
                cx_core::config::BackendKind::Server => {
                    section.url.clone().map(|url| match &section.space {
                        Some(space) => format!("{url} · 空间 {space}"),
                        None => url,
                    })
                }
                cx_core::config::BackendKind::Webdav => section.url.clone(),
                cx_core::config::BackendKind::S3 => Some(format!(
                    "{} @ {}",
                    section.bucket.clone().unwrap_or_default(),
                    section
                        .endpoint
                        .clone()
                        .unwrap_or_else(|| "AWS 官方".to_string())
                )),
            };
        }

        StatusReport {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: 0,
            started_ms: 0,
            paused: self.state.paused,
            device_id: self.identity.device_id.clone(),
            device_name: self.identity.name.clone(),
            space_id: self.space_id.clone(),
            encrypted: self.cipher.space_key().is_some(),
            backends,
            history_len: self.history.len(),
            seen_len: self.seen.len(),
            skipped_oversize: self.state.skipped_oversize,
            skipped_ignored: self.state.skipped_ignored,
            last_published_hash: self.state.last_published_hash.clone(),
            clipboard: String::new(),
            watch: String::new(),
            no_clipboard: false,
        }
    }

    /// 历史摘要列表（0 = 最新）。
    pub fn history_summaries(&self, limit: usize) -> Vec<HistorySummary> {
        self.history
            .newest_first(limit)
            .into_iter()
            .enumerate()
            .map(|(position, entry)| HistorySummary {
                position,
                name: entry.name.clone(),
                kind: entry.kind.as_str().to_string(),
                created_ms: entry.created_ms,
                bytes: entry.plain_len,
                hash: entry.plain_hash.clone(),
                origin: entry.origin_name.clone(),
                origin_device: entry.origin_device.clone(),
            })
            .collect()
    }

    /// 历史内容预览（文本截断到 `max_chars`，图片只给尺寸）。
    pub fn history_preview(
        &self,
        position: usize,
        max_chars: usize,
    ) -> Result<HistoryPreview, CoreError> {
        let entry = self
            .history
            .newest_first(position + 1)
            .last()
            .cloned()
            .ok_or_else(|| {
                CoreError::HistoryCorrupt(format!(
                    "历史只有 {} 条，取不到第 {} 条",
                    self.history.len(),
                    position + 1
                ))
            })?;
        let item = self.history.read_newest(position)?;
        let plain = if entry.name.is_empty() {
            item.payload.clone()
        } else {
            self.cipher.open(&entry.name, &item)?
        };

        match item.kind {
            cx_proto::Kind::Text => {
                let text = String::from_utf8_lossy(&plain).to_string();
                let truncated = text.chars().count() > max_chars;
                let shown: String = text.chars().take(max_chars).collect();
                Ok(HistoryPreview {
                    position,
                    kind: "text".to_string(),
                    text: Some(shown),
                    image: None,
                    truncated,
                    bytes: entry.plain_len,
                })
            }
            cx_proto::Kind::Image => {
                let image = cx_clip::RawImage::from_png(&plain)
                    .map_err(|e| CoreError::Clip(e.to_string()))?;
                Ok(HistoryPreview {
                    position,
                    kind: "image".to_string(),
                    text: None,
                    image: Some(format!("{}x{}", image.width, image.height)),
                    truncated: false,
                    bytes: entry.plain_len,
                })
            }
        }
    }

    /// 本机诊断（剪贴板与会话相关项由守护进程补充）。
    pub fn doctor(&self) -> DoctorReport {
        let mut checks = Vec::new();

        let problems = self.config.problems();
        checks.push(DoctorCheck {
            name: "配置".to_string(),
            ok: problems.is_empty(),
            detail: if problems.is_empty() {
                "无问题".to_string()
            } else {
                problems.join("；")
            },
        });

        checks.push(DoctorCheck {
            name: "空间".to_string(),
            ok: self.cipher.space_key().is_some(),
            detail: match (&self.space_id, self.cipher.space_key().is_some()) {
                (Some(id), true) => format!("已加入加密空间 {}", &id[..id.len().min(8)]),
                (Some(id), false) => format!(
                    "空间 {} 未加密（对象存储里可直接读到内容）",
                    &id[..id.len().min(8)]
                ),
                (None, _) => "尚未加入空间（cx space init / cx space join）".to_string(),
            },
        });

        checks.push(DoctorCheck {
            name: "历史存储".to_string(),
            ok: self.history.skipped_frames() == 0,
            detail: format!(
                "{} 条，{} 字节；重建索引时跳过 {} 个坏帧",
                self.history.len(),
                self.history.log_size(),
                self.history.skipped_frames()
            ),
        });

        checks.push(DoctorCheck {
            name: "后端".to_string(),
            ok: self.config.enabled_backends().count() > 0,
            detail: match self.config.enabled_backends().count() {
                0 => "没有启用任何后端（当前只是本机历史）".to_string(),
                n => format!("启用 {n} 个后端"),
            },
        });

        for backend in &self.config.backends {
            let Some(state) = self.state.backend(&backend.name) else {
                continue;
            };
            if let Some(err) = &state.last_error {
                checks.push(DoctorCheck {
                    name: format!("后端 {}", backend.name),
                    ok: false,
                    detail: format!(
                        "最近错误：{err}（连续失败 {} 次）",
                        state.consecutive_failures
                    ),
                });
            }
        }

        DoctorReport::new(checks)
    }

    /// 后端 worker 汇报一轮结果：更新游标、计数与退避状态。
    fn on_backend_report(
        &mut self,
        now_ms: u64,
        name: &str,
        cursors: BTreeMap<String, Cursor>,
        published: u64,
        applied: u64,
        error: Option<String>,
    ) -> Vec<Action> {
        let enabled = self
            .config
            .backend(name)
            .map(|backend| backend.enabled)
            .unwrap_or(true);
        let state = self.state.backend_mut(name, enabled);
        state.cursors = cursors;
        state.published = published;
        state.applied = applied;

        match error {
            Some(message) => {
                let failures = state.consecutive_failures;
                let message_for_log = message.clone();
                state.record_failure(now_ms, message, 0);
                log::debug!(
                    "后端 {name} 失败（第 {} 次）：{message_for_log}",
                    failures + 1
                );
            }
            None => state.record_success(now_ms),
        }

        let mut actions = vec![Action::Notify(ControlEvent::new(
            event::STATUS_CHANGED,
            json!({ "backend": name }),
        ))];
        // 状态变更不必每次都落盘，跟着其它保存点走
        if now_ms.saturating_sub(self.last_save_ms) > 5_000 {
            actions.push(Action::SaveState);
        }
        actions
    }

    /// 供守护进程启动时恢复各后端游标。
    pub fn cursors_snapshot(&self) -> BTreeMap<String, BTreeMap<String, Cursor>> {
        self.state
            .backends
            .iter()
            .map(|(name, state)| (name.clone(), state.cursors.clone()))
            .collect()
    }

    /// 最近的历史条目（供局域网直连做"我有哪些内容"的清单）。
    ///
    /// 返回 `ManifestEntry` 而不是局域网自己的类型：这样 cx-sync 不需要依赖 cx-lan，
    /// 由守护进程做一次转换即可。
    pub fn recent_entries(&self, limit: usize) -> Vec<ManifestEntry> {
        self.history
            .newest_first(limit)
            .into_iter()
            .enumerate()
            .filter(|(position, entry)| {
                self.remote_ready()
                    && self
                        .history
                        .read_newest(*position)
                        .ok()
                        .is_some_and(|item| self.cipher.open(&entry.name, &item).is_ok())
            })
            .map(|(_, entry)| ManifestEntry {
                name: entry.name.clone(),
                day: cx_proto::civil::day_from_ms(entry.created_ms),
                size: entry.frame_len as u64,
                hash: entry.plain_hash.clone(),
                kind: entry.kind,
                created_ms: entry.created_ms,
                mime: None,
            })
            .collect()
    }

    /// 已处理集合长度（测试与状态展示用）。
    pub fn seen_len(&self) -> usize {
        self.seen.len()
    }

    /// 把状态（已处理集合、游标、计数、最后发布哈希）落盘。
    ///
    /// 守护进程在执行 [`Action::SaveState`] 与退出前调用；测试用它模拟"重启后仍记得
    /// 已发布过什么"。状态文件很小（≤ 1024 条名字），每次裁剪/发布后写一次是划算的：
    /// 换来"进程被强杀也不会重复上传历史内容"。
    pub fn flush_state(&mut self) -> Result<(), CoreError> {
        self.state.seen_items = self.seen.to_names();
        self.state.save(&self.paths)
    }
}

fn mime_for(kind: cx_proto::Kind) -> &'static str {
    match kind {
        cx_proto::Kind::Text => "text/plain; charset=utf-8",
        cx_proto::Kind::Image => "image/png",
    }
}

fn error_body(err: &CoreError) -> ErrorBody {
    let code = match err {
        CoreError::NotInitialized(_) => errcode::NOT_INITIALIZED,
        CoreError::Config(_) => errcode::BAD_ARGS,
        _ => errcode::INTERNAL,
    };
    ErrorBody {
        code: code.to_string(),
        msg: err.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use cx_clip::RawImage;
    use cx_core::Paths;

    struct Harness {
        engine: Engine,
        root: std::path::PathBuf,
        now: u64,
    }

    impl Harness {
        fn new(mut config: Config) -> Self {
            // 本测试夹具没有空间密钥，旧发布语义测试必须明确选择明文模式。
            config.space.encrypt = false;
            // 注意：并行测试不能用时间戳命名目录（同毫秒会撞名，互相删掉对方的数据）
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("cx-engine-{}-{unique}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let paths = Paths::from_root(&root);
            paths.ensure().unwrap();
            let identity = Identity::load_or_create(&paths, Some("test-box")).unwrap();
            let engine = Engine::new(&paths, config, identity, None).unwrap();
            Self {
                engine,
                root,
                now: 1_760_000_000_000,
            }
        }

        fn tick(&mut self, ms: u64) {
            self.now += ms;
        }

        fn feed(&mut self, content: ClipContent) -> Vec<Action> {
            self.engine
                .handle(self.now, Event::ClipboardChanged(Some(content)))
        }

        fn text(text: &str) -> ClipContent {
            ClipContent::Text(text.to_string())
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn published(actions: &[Action]) -> Option<&str> {
        actions.iter().find_map(|a| match a {
            Action::Publish { name, .. } => Some(name.as_str()),
            _ => None,
        })
    }

    fn applied(actions: &[Action]) -> Option<&ClipContent> {
        actions.iter().find_map(|a| match a {
            Action::ApplyToClipboard(content) | Action::CopyToClipboard { content, .. } => {
                Some(content)
            }
            _ => None,
        })
    }

    #[test]
    fn 要求加密但没有密钥时仅记录本机历史() {
        let mut h = Harness::new(Config::default());
        h.engine.config.space.encrypt = true;
        let actions = h.feed(Harness::text("fake-secret-only-local"));
        assert!(published(&actions).is_none());
        assert_eq!(h.engine.history_len(), 1);
        assert!(!h.engine.remote_ready());
        assert!(actions.iter().any(|action| matches!(action, Action::Notify(event)
            if event.event == event::CLIP_SKIPPED && event.data["reason"] == "encryption_not_ready")));
    }

    #[test]
    fn 稳定对象校验拒绝插入新条目后的旧位置() {
        let mut h = Harness::new(Config::default());
        h.feed(Harness::text("selected-a"));
        let expected_name = h.engine.history_summaries(1)[0].name.clone();
        h.tick(10);
        h.feed(Harness::text("new-b"));
        for operation in ["show", "copy", "delete"] {
            let actions = h.engine.handle(
                h.now,
                Event::Command(Command::HistoryChecked {
                    id: 77,
                    position: 0,
                    expected_name: expected_name.clone(),
                    operation: operation.into(),
                    max_chars: 64,
                }),
            );
            assert!(
                matches!(&actions[0], Action::Reply(ReplyPayload::Error { body, .. }) if body.code == "history_changed")
            );
            assert_eq!(h.engine.history_len(), 2);
        }
    }

    #[test]
    fn 新内容被发布并入库() {
        let mut h = Harness::new(Config::default());
        let actions = h.feed(Harness::text("hello"));

        let name = published(&actions).expect("应发布");
        assert!(name.ends_with(".cxi"));
        assert!(actions.iter().any(|a| matches!(a, Action::SaveState)));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::Notify(e) if e.event == event::CLIP_PUBLISHED
        )));
        assert_eq!(h.engine.history_len(), 1);
        assert_eq!(h.engine.seen_len(), 1);
    }

    #[test]
    fn 相同内容不重复发布() {
        let mut h = Harness::new(Config::default());
        assert!(published(&h.feed(Harness::text("same"))).is_some());
        h.tick(1_000);
        assert!(
            published(&h.feed(Harness::text("same"))).is_none(),
            "内容相同（指纹一致）不应再次发布"
        );
        assert_eq!(h.engine.history_len(), 1);
    }

    #[test]
    fn 剪贴板清空后同内容再次发布() {
        let mut h = Harness::new(Config::default());
        h.feed(Harness::text("x"));
        h.tick(10);
        let actions = h.engine.handle(h.now, Event::ClipboardChanged(None));
        assert!(actions.is_empty());
        h.tick(10);
        let actions = h.feed(Harness::text("x"));
        assert!(published(&actions).is_none(), "指纹未变，不应发布");
        h.tick(10);
        h.feed(Harness::text("y"));
        assert!(
            published(&h.feed(Harness::text("x"))).is_some(),
            "内容变化后允许再发布"
        );
    }

    #[test]
    fn 空白文本被忽略() {
        let mut h = Harness::new(Config::default());
        let actions = h.feed(Harness::text("   \n\t "));
        assert!(published(&actions).is_none());
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::Notify(e) if e.event == event::CLIP_SKIPPED
        )));
        assert_eq!(h.engine.history_len(), 0);
    }

    #[test]
    fn glob_规则命中则跳过() {
        let mut config = Config::default();
        config.sync.ignore_globs = vec!["*密码*".to_string()];
        let mut h = Harness::new(config);

        // 不含"密码"两字的内容应正常发布
        assert!(published(&h.feed(Harness::text("这是一段普通文本"))).is_some());
        h.tick(10);
        assert!(published(&h.feed(Harness::text("我的密码是 123"))).is_none());
    }

    #[test]
    fn 超出体积上限则跳过() {
        let mut config = Config::default();
        config.sync.max_item_bytes = 1024;
        let mut h = Harness::new(config);

        let big = "x".repeat(4096);
        let actions = h.feed(Harness::text(&big));
        assert!(published(&actions).is_none());
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::Notify(e) if e.event == event::CLIP_SKIPPED
        )));
    }

    #[test]
    fn 图片内容可发布且历史记录类型正确() {
        let mut h = Harness::new(Config::default());
        let image = RawImage::new(
            2,
            2,
            vec![
                10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
            ],
        )
        .unwrap();
        let actions = h.feed(ClipContent::Image(image.clone()));
        assert!(published(&actions).is_some());

        let summaries = h.engine.history_summaries(4);
        assert_eq!(summaries[0].kind, "image");
        let preview = h.engine.history_preview(0, 64).unwrap();
        assert_eq!(preview.kind, "image");
        assert_eq!(preview.image.as_deref(), Some("2x2"));
        assert!(preview.text.is_none());
    }

    #[test]
    fn 暂停期间不发布_恢复后补发() {
        let mut h = Harness::new(Config::default());
        let actions = h
            .engine
            .handle(h.now, Event::Command(Command::Pause { id: 1 }));
        assert!(h.engine.is_paused());
        assert!(actions.iter().any(|a| matches!(a, Action::Reply(_))));

        h.tick(10);
        let actions = h.feed(Harness::text("暂停期间复制的内容"));
        assert!(published(&actions).is_none());
        assert_eq!(h.engine.history_len(), 0);

        h.tick(10);
        let actions = h
            .engine
            .handle(h.now, Event::Command(Command::Resume { id: 2 }));
        assert!(!h.engine.is_paused());
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::RecheckClipboard)),
            "恢复后应请求重读剪贴板"
        );

        h.tick(10);
        let actions = h.feed(Harness::text("暂停期间复制的内容"));
        assert!(published(&actions).is_some(), "恢复后同一内容应被发布");
    }

    #[test]
    fn push_强制发布相同内容() {
        let mut h = Harness::new(Config::default());
        h.feed(Harness::text("abc"));
        h.tick(10);
        // 普通路径：相同内容不发布
        assert!(published(&h.feed(Harness::text("abc"))).is_none());

        // force 路径：必须发布（并留下第二条历史）
        h.tick(10);
        let actions = h.engine.handle(
            h.now,
            Event::Command(Command::Push {
                id: 1,
                content: Some(Harness::text("abc")),
                force: true,
            }),
        );
        assert!(published(&actions).is_some());
        assert_eq!(h.engine.history_len(), 2);
    }

    #[test]
    fn 远端内容被应用并抑制回声() {
        let mut h = Harness::new(Config::default());
        let name = "1759299998712-ab12cd34-9f3c1e77.cxi";
        let plain = "来自另一台设备";
        let item = plain_item(name, plain.as_bytes());

        let actions = h.engine.handle(
            h.now,
            Event::Command(Command::ApplyRemote {
                backend: "test".to_string(),
                name: name.to_string(),
                item: Box::new(item),
            }),
        );
        let content = applied(&actions).expect("应写入本机剪贴板");
        assert_eq!(content.text(), Some(plain));
        assert_eq!(h.engine.history_len(), 1);

        // 写入剪贴板触发的 watcher 事件必须被抑制
        h.tick(100);
        let actions = h.feed(ClipContent::Text(plain.to_string()));
        assert!(published(&actions).is_none(), "回声不应被再次发布");
        assert_eq!(h.engine.history_len(), 1);
    }

    #[test]
    fn 远端对象重复到达只应用一次() {
        let mut h = Harness::new(Config::default());
        let name = "1759299998713-ab12cd34-11112222.cxi";
        let item = plain_item(name, b"once");

        let first = h.engine.handle(
            h.now,
            Event::Command(Command::ApplyRemote {
                backend: "test".to_string(),
                name: name.to_string(),
                item: Box::new(item.clone()),
            }),
        );
        assert!(applied(&first).is_some());

        h.tick(10);
        let second = h.engine.handle(
            h.now,
            Event::Command(Command::ApplyRemote {
                backend: "test".to_string(),
                name: name.to_string(),
                item: Box::new(item),
            }),
        );
        assert!(
            second
                .iter()
                .all(|action| matches!(action, Action::Reply(ReplyPayload::Done { .. }))),
            "已处理对象仅确认，不重复应用"
        );
        assert_eq!(h.engine.history_len(), 1);
    }

    #[test]
    fn manual_模式只入库不写剪贴板() {
        let mut config = Config::default();
        config.sync.apply_remote = ApplyRemoteMode::Manual;
        let mut h = Harness::new(config);
        let name = "1759299998714-ab12cd34-33334444.cxi";

        let actions = h.engine.handle(
            h.now,
            Event::Command(Command::ApplyRemote {
                backend: "test".to_string(),
                name: name.to_string(),
                item: Box::new(plain_item(name, b"manual")),
            }),
        );
        assert!(applied(&actions).is_none(), "manual 模式不应写剪贴板");
        assert_eq!(h.engine.history_len(), 1, "但仍应入库，便于 cx copy");
    }

    #[test]
    fn 删除本机历史后条目消失且不会立刻被拉回来() {
        let mut h = Harness::new(Config::default());
        h.feed(Harness::text("第一条"));
        h.tick(10);
        h.feed(Harness::text("第二条"));
        h.tick(20);
        h.feed(Harness::text("第三条"));
        h.tick(30);
        assert_eq!(h.engine.history_len(), 3);

        // 删掉位置 1（0 = 最新）=> 中间那条
        let actions = h.engine.handle(
            h.now,
            Event::Command(Command::HistoryDelete { id: 7, position: 1 }),
        );
        assert!(
            actions.iter().any(|a| matches!(a, Action::Reply(_))),
            "应当回一条应答"
        );
        assert_eq!(h.engine.history_len(), 2, "历史应当少一条");

        // 删除只作用于本机历史，因此不该产生任何发布动作
        assert!(published(&actions).is_none(), "删除不该顺手发布什么");

        // 它仍然留在"已处理"集合里：某个后端游标落后而重新拉到它时，不该再应用一次
        let name = h
            .engine
            .recent_entries(10)
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(name.len(), 2);
        assert!(!name.iter().any(|n| n.contains("第二条")));
    }

    #[test]
    fn 越界删除返回错误而不是悄悄成功() {
        let mut h = Harness::new(Config::default());
        h.feed(Harness::text("只有一条"));
        h.tick(10);

        let actions = h.engine.handle(
            h.now,
            Event::Command(Command::HistoryDelete { id: 9, position: 5 }),
        );
        let replied_error = actions
            .iter()
            .any(|a| matches!(a, Action::Reply(ReplyPayload::Error { .. })));
        assert!(replied_error, "越界删除应当回一条失败应答");
        assert_eq!(h.engine.history_len(), 1, "失败的删除不该改动历史");
    }
    #[test]
    fn only_if_idle_在本机剪贴板被用户占用时跳过() {
        let mut config = Config::default();
        config.sync.apply_remote = ApplyRemoteMode::OnlyIfIdle;
        let mut h = Harness::new(config);
        let now = h.now;

        // 语义：剪贴板里是"我们自己发布的内容"⇒ 空闲，可以覆盖；
        //       剪贴板里是用户刚复制、但被忽略规则丢弃的内容 ⇒ 用户正在使用，不覆盖。
        h.feed(Harness::text("   ")); // 空白文本被忽略，不发布 ⇒ 属于用户占用
        h.tick(10);
        let busy_name = "1759299998715-ab12cd34-55556666.cxi";
        let actions = h.engine.handle(
            h.now,
            Event::Command(Command::ApplyRemote {
                backend: "test".to_string(),
                name: busy_name.to_string(),
                item: Box::new(plain_item(busy_name, b"remote")),
            }),
        );
        assert!(applied(&actions).is_none(), "用户正在使用剪贴板时不应覆盖");

        // 本机发布了自己的内容 ⇒ 剪贴板回到"我们持有"的状态
        h.tick(10);
        assert!(published(&h.feed(Harness::text("本机自己发布的内容"))).is_some());
        h.tick(10);
        let idle_name = "1759299998716-ab12cd34-77778888.cxi";
        let actions = h.engine.handle(
            h.now,
            Event::Command(Command::ApplyRemote {
                backend: "test".to_string(),
                name: idle_name.to_string(),
                item: Box::new(plain_item(idle_name, b"remote")),
            }),
        );
        assert!(applied(&actions).is_some(), "空闲时应应用远端内容");
        assert!(h.now > now);
    }

    #[test]
    fn copy_把历史写回剪贴板并抑制回声() {
        let mut h = Harness::new(Config::default());
        h.feed(Harness::text("第一条"));
        h.tick(10);
        h.feed(Harness::text("第二条"));
        assert_eq!(h.engine.history_len(), 2);

        h.tick(10);
        let actions = h
            .engine
            .handle(h.now, Event::Command(Command::Copy { id: 7, position: 1 }));
        let content = applied(&actions).expect("应写回剪贴板");
        assert_eq!(content.text(), Some("第一条"), "position=1 是倒数第二条");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CopyToClipboard { id: 7, .. }))
        );

        // 写回产生的 watcher 事件不应导致重复上传
        h.tick(10);
        let echo = h.feed(Harness::text("第一条"));
        assert!(published(&echo).is_none());
        assert_eq!(h.engine.history_len(), 2);
    }

    #[test]
    fn copy_越界返回错误响应() {
        let mut h = Harness::new(Config::default());
        let actions = h
            .engine
            .handle(h.now, Event::Command(Command::Copy { id: 9, position: 3 }));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Reply(ReplyPayload::Error { id: 9, .. })))
        );
    }

    #[test]
    fn 历史列表与预览() {
        let mut h = Harness::new(Config::default());
        h.feed(Harness::text("内容一"));
        h.tick(10);
        h.feed(Harness::text("内容二更长一些"));

        let items = h.engine.history_summaries(10);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].position, 0);
        assert_eq!(items[0].kind, "text");

        let preview = h.engine.history_preview(0, 3).unwrap();
        assert!(preview.truncated, "超过 3 个字符应标记截断");
        assert_eq!(preview.text.unwrap().chars().count(), 3);

        let preview = h.engine.history_preview(1, 64).unwrap();
        assert_eq!(preview.text.as_deref(), Some("内容一"));
        assert!(!preview.truncated);
    }

    #[test]
    fn 状态报告包含设备与计数() {
        let mut h = Harness::new(Config::default());
        h.feed(Harness::text("x"));
        let actions = h
            .engine
            .handle(h.now, Event::Command(Command::Status { id: 1 }));
        let report = actions
            .iter()
            .find_map(|a| match a {
                Action::Reply(ReplyPayload::Status { report, .. }) => Some(report.clone()),
                _ => None,
            })
            .expect("应有状态响应");
        assert_eq!(report.device_name, "test-box");
        assert_eq!(report.history_len, 1);
        assert!(!report.paused);
        assert!(!report.encrypted, "未加入空间时为明文");
    }

    #[test]
    fn 重载配置生效() {
        let mut h = Harness::new(Config::default());
        let mut new_config = h.engine.config.clone();
        new_config.sync.max_item_bytes = 8;
        let actions = h.engine.handle(
            h.now,
            Event::Command(Command::Reload {
                id: 1,
                config: Box::new(new_config),
            }),
        );
        assert!(actions.iter().any(|a| matches!(a, Action::Reply(_))));

        h.tick(10);
        assert!(
            published(&h.feed(Harness::text("超过八字节的内容"))).is_none(),
            "新配置的体积上限应立即生效"
        );
    }

    #[test]
    fn 诊断报告给出可操作信息() {
        let h = Harness::new(Config::default());
        let report = h.engine.doctor();
        assert!(!report.all_ok(), "未加入空间、没有后端时应报告问题");
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"配置"));
        assert!(names.contains(&"空间"));
        assert!(names.contains(&"历史存储"));
        assert!(names.contains(&"后端"));
        let space = report.checks.iter().find(|c| c.name == "空间").unwrap();
        assert!(space.detail.contains("cx space init"));
    }

    #[test]
    fn 重启后不重复发布相同内容() {
        let mut h = Harness::new(Config::default());
        h.feed(Harness::text("跨越重启的内容"));
        // 模拟守护进程执行 Action::SaveState
        h.engine.flush_state().unwrap();

        // 模拟重启：在同一份磁盘状态上新建引擎（旧引擎保持不动作，数据目录随后由 Harness 清理）
        let root = h.root.clone();
        let config = h.engine.config().clone();
        let paths = Paths::from_root(&root);
        let identity = Identity::load_or_create(&paths, None).unwrap();
        let mut engine = Engine::new(&paths, config, identity, None).unwrap();
        let actions = engine.handle(
            1_760_000_000_999,
            Event::Command(Command::Push {
                id: 1,
                content: Some(ClipContent::Text("跨越重启的内容".to_string())),
                force: false,
            }),
        );
        assert!(
            published(&actions).is_none(),
            "重启后同内容应被 last_published_hash 拦住"
        );
    }

    /// 构造一个明文 item（测试用，绕过加密器）。
    fn plain_item(name: &str, plain: &[u8]) -> Item {
        let created_ms: u64 = name[..13].parse().unwrap();
        Item::new(
            cx_proto::Kind::Text,
            cx_proto::Cipher::None,
            ItemMeta {
                id: vec![1u8; cx_proto::ID_LEN],
                origin_device: "ffffffffffffffffffffffffffffffff".to_string(),
                origin_name: "other-box".to_string(),
                created_ms,
                seq: 1,
                kind: cx_proto::Kind::Text,
                mime: Some("text/plain; charset=utf-8".to_string()),
                plain_hash: content_hash(plain).to_vec(),
                plain_len: plain.len() as u32,
                nonce: Vec::new(),
            },
            plain.to_vec(),
        )
        .unwrap()
    }
}
