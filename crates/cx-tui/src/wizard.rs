//! 添加后端的表单（单页，像网页表单那样填）。
//!
//! 设计要点：
//! - **一屏看到全部字段**，不做"一步一步问答"；类型是单选行，换类型时下方字段跟着换；
//! - **焦点清晰**：Tab / ↑↓ 在类型与字段间移动，也可以**用鼠标点**某一行直接定位；
//! - **校验就地提示**：提交时一次标出所有没填的必填项，不跳步、不丢已填内容；
//! - **按钮**：[保存并测试] 与 [取消]，同样支持点与键（Enter / Esc）；
//! - **密钥类字段密文回显**（只显示 ● 与长度），避免肩窥，也不写日志。
//!
//! 这里只产出草稿，不碰配置文件、不碰网络：落盘与连通性自测由 `cx` 命令层完成
//! （`cx tui` 与 `cx backend add` 本来就是同一个二进制，同进程内的库调用）。

/// 表单支持的后端类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardKind {
    /// 自建服务端 `cx-server`。
    Server,
    /// WebDAV（Nextcloud 等）。
    Webdav,
    /// S3 兼容（AWS / MinIO / R2 / 阿里云 OSS）。
    S3,
}

const SERVER_FIELDS: [Field; 4] = [
    Field::text("后端名", "home", "唯一名字，用于 cx backend list / test"),
    Field::text(
        "服务端地址",
        "http://192.168.1.9:8688",
        "cx-server 监听的地址",
    ),
    Field::text(
        "空间 id",
        "服务端创建的空间 id",
        "服务端空间 id；本机加密空间需另行创建或加入",
    ),
    Field::secret(
        "注册密钥",
        "cx-server 生成的注册密钥；保存时用它自助注册换取设备令牌（密文输入，不回显）",
    ),
];

const WEBDAV_FIELDS: [Field; 4] = [
    Field::text("后端名", "dav", "唯一名字"),
    Field::text(
        "URL",
        "https://cloud.example.com/remote.php/dav/files/me/clipboardx",
        "WebDAV 目录地址",
    ),
    Field::text("用户名", "me", "留空表示匿名访问"),
    Field::secret("口令", "匿名时留空；账号模式建议用应用密码；粘贴原样保留"),
];

/// S3 端点模板：选一个服务商，把**端点 / 区域 / 寻址**一起填好。
///
/// 为什么要有它：这三项是"抄写题"——`oss-cn-beijing.aliyuncs.com` 与 `cn-beijing`
/// 必须成对，MinIO 常常要 path 式寻址，R2 的区域字面值是 `auto`。凭记忆敲只会多出
/// 一种输错的方式，而模板只填这三项：桶名与凭据永远由用户自己填（模板不知道你的账号）。
const S3_TEMPLATE_CHOICES: &[(&str, &str)] = &[
    ("自定义", ""),
    ("阿里云 OSS（北京）", "oss-beijing"),
    ("阿里云 OSS（杭州）", "oss-hangzhou"),
    ("MinIO（本机）", "minio"),
    ("Cloudflare R2", "r2"),
    ("AWS S3", "aws"),
];

/// 模板 → `(端点, 区域, 寻址)`。
fn s3_template(id: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match id {
        "oss-beijing" => Some(("https://oss-cn-beijing.aliyuncs.com", "cn-beijing", "")),
        "oss-hangzhou" => Some(("https://oss-cn-hangzhou.aliyuncs.com", "cn-hangzhou", "")),
        // 本机 MinIO：path 式寻址最稳（虚拟主机名在 127.0.0.1 上解析不了）
        "minio" => Some(("http://127.0.0.1:9000", "us-east-1", "path")),
        // R2 的区域字面值就是 auto，端点里的账号 id 需要自己替换
        "r2" => Some(("https://<账号id>.r2.cloudflarestorage.com", "auto", "")),
        // AWS 官方：端点可留空（SDK 自己拼），区域必须填
        "aws" => Some(("", "us-east-1", "")),
        _ => None,
    }
}

/// S3 表单里各字段的下标（插了模板行之后集中放在这里，免得数字散落各处）。
mod s3_fields {
    pub(super) const TEMPLATE: usize = 2;
    pub(super) const ENDPOINT: usize = 3;
    pub(super) const REGION: usize = 4;
    pub(super) const ADDRESSING: usize = 8;
}

const S3_FIELDS: [Field; 10] = [
    Field::text("后端名", "oss", "唯一名字"),
    Field::text("桶名", "clipboardx", "请先创建桶；连接检查不自动创建"),
    Field::choice(
        "模板",
        S3_TEMPLATE_CHOICES,
        "←/→ 选服务商：自动填好端点、区域与寻址；桶名与凭据仍然自己填",
    ),
    Field::text(
        "端点",
        "https://oss-cn-hangzhou.aliyuncs.com",
        "AWS 官方留空；OSS 用 virtual-host，MinIO 可用 path-style",
    ),
    Field::text(
        "区域",
        "cn-hangzhou",
        "必须与端点同区域（阿里云杭州 = cn-hangzhou）；选模板会自动填",
    ),
    Field::text("AccessKeyId", "（RAM 控制台创建）", "只给该桶读写权限即可"),
    Field::secret("SecretAccessKey", "与 AccessKeyId 配对；密文输入，不回显"),
    Field::text("前缀", "clipboardx", "对象名会落在该前缀下"),
    Field::choice(
        "寻址",
        &[
            ("自动", ""),
            ("虚拟主机", "virtual-host"),
            ("路径式", "path"),
        ],
        "←/→ 切换；留自动即可：OSS / AWS 走 virtual-host，MinIO 可试路径式",
    ),
    Field::secret(
        "会话令牌",
        "仅 STS 临时凭据需要；长期 RAM Key 留空（密文输入）",
    ),
];

impl WizardKind {
    /// 全部类型（界面上的顺序）。
    pub const ALL: [WizardKind; 3] = [WizardKind::Server, WizardKind::Webdav, WizardKind::S3];

    /// 名字。
    pub fn label(self) -> &'static str {
        match self {
            WizardKind::Server => "自建服务端",
            WizardKind::Webdav => "WebDAV",
            WizardKind::S3 => "S3 兼容",
        }
    }

    /// 一句话用途。
    pub fn hint(self) -> &'static str {
        match self {
            WizardKind::Server => "自己掌控，需要先跑 cx-server",
            WizardKind::Webdav => "Nextcloud / ownCloud 等现成网盘",
            WizardKind::S3 => "AWS / MinIO / R2 / 阿里云 OSS",
        }
    }

    /// 该类型的字段表。
    pub fn fields(self) -> &'static [Field] {
        match self {
            WizardKind::Server => &SERVER_FIELDS,
            WizardKind::Webdav => &WEBDAV_FIELDS,
            WizardKind::S3 => &S3_FIELDS,
        }
    }
}

/// 表单里的一个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// 显示名。
    pub label: &'static str,
    /// 输入示例（可一键填入）。
    pub sample: &'static str,
    /// 说明。
    pub help: &'static str,
    /// 是否密文（只回显 ●）。
    pub secret: bool,
    /// 可选值：`(显示名, 写入配置的值)`。非空表示这是个"选择题"而不是文本框
    /// —— 让用户凭记忆敲 `virtual-host` 这种东西只会多一种输错的方式。
    pub choices: Option<&'static [(&'static str, &'static str)]>,
}

impl Field {
    const fn text(label: &'static str, sample: &'static str, help: &'static str) -> Field {
        Field {
            label,
            sample,
            help,
            secret: false,
            choices: None,
        }
    }

    const fn secret(label: &'static str, help: &'static str) -> Field {
        Field {
            label,
            sample: "",
            help,
            secret: true,
            choices: None,
        }
    }

    const fn choice(
        label: &'static str,
        choices: &'static [(&'static str, &'static str)],
        help: &'static str,
    ) -> Field {
        Field {
            label,
            sample: "",
            help,
            secret: false,
            choices: Some(choices),
        }
    }

    /// 是不是选择题。
    pub fn is_choice(&self) -> bool {
        self.choices.is_some_and(|choices| !choices.is_empty())
    }

    /// 当前值的显示名（选择题才有意义）。
    pub fn choice_label(&self, value: &str) -> Option<&'static str> {
        let choices = self.choices?;
        let index = choice_index(choices, value)?;
        Some(choices[index].0)
    }

    /// 是否必填（端点 / 用户名 / 前缀可空）。
    pub fn required(&self) -> bool {
        // 端点可空（AWS 官方留空由 SDK 拼）、用户名可空（匿名 WebDAV）、
        // 前缀可空、模板/寻址/会话令牌都有默认值
        !matches!(
            self.label,
            "端点" | "用户名" | "前缀" | "模板" | "寻址" | "会话令牌"
        )
    }
}

/// 值在选项表里的下标；`auto` 视同空值（配置里两种写法等价）。
fn choice_index(choices: &'static [(&'static str, &'static str)], value: &str) -> Option<usize> {
    let wanted = if value == "auto" { "" } else { value };
    choices.iter().position(|(_, stored)| *stored == wanted)
}

/// 表单收集出来的草稿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDraft {
    /// 类型。
    pub kind: WizardKind,
    /// 字段值，顺序与 `WizardKind::fields()` 一致。
    pub values: Vec<String>,
}

impl BackendDraft {
    /// 按标签取值。
    pub fn get(&self, label: &str) -> &str {
        self.kind
            .fields()
            .iter()
            .position(|field| field.label == label)
            .and_then(|index| self.values.get(index))
            .map(String::as_str)
            .unwrap_or("")
    }

    /// 后端名（第一个字段恒为后端名）。
    pub fn name(&self) -> &str {
        self.values.first().map(String::as_str).unwrap_or("")
    }
}

/// 焦点落在哪一行（键盘移动与鼠标点击都落到这里）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// 类型单选行。
    Kind,
    /// 第 N 个字段。
    Field(usize),
    /// [保存并测试] 按钮。
    Save,
    /// [取消] 按钮。
    Cancel,
}

/// 表单状态。
#[derive(Debug, Clone)]
pub struct Wizard {
    kind_index: usize,
    /// 每种类型各自的值，切换类型不丢已填内容。
    values: [Vec<String>; 3],
    focus: Focus,
    /// 提交时仍为空、需要标红的字段下标。
    invalid: Vec<usize>,
    /// 提交失败的整体提示。
    pub message: Option<String>,
    pub(crate) scroll: u16,
    pub(crate) max_scroll: u16,
    pub(crate) follow_focus: bool,
    /// 编辑已有后端：密码类字段可以留空（留空 = 沿用库里的现值）。
    edit_mode: bool,
}

impl Default for Wizard {
    fn default() -> Self {
        Self::new()
    }
}

impl Wizard {
    /// 从未提交成功的草稿恢复输入；秘密仅保留在原进程内存中。
    pub fn from_draft(draft: &BackendDraft, error: String) -> Self {
        let mut form = Self::new();
        form.kind_index = WizardKind::ALL
            .iter()
            .position(|kind| *kind == draft.kind)
            .unwrap_or(0);
        form.values[form.kind_index] = draft.values.clone();
        form.focus = Focus::Save;
        form.message = Some(error);
        form
    }
    /// 新建：默认第一种类型，后端名预填示例值（少一个必填项要敲）。
    pub fn new() -> Self {
        let mut values: [Vec<String>; 3] = Default::default();
        for (index, kind) in WizardKind::ALL.iter().enumerate() {
            let mut row: Vec<String> = kind.fields().iter().map(|_| String::new()).collect();
            if let Some(first) = kind.fields().first() {
                row[0] = first.sample.to_string();
            }
            values[index] = row;
        }
        Self {
            kind_index: 0,
            values,
            focus: Focus::Kind,
            invalid: Vec::new(),
            message: None,
            scroll: 0,
            max_scroll: 0,
            follow_focus: true,
            edit_mode: false,
        }
    }

    /// 当前类型。
    pub fn kind(&self) -> WizardKind {
        WizardKind::ALL[self.kind_index]
    }

    /// 当前类型的字段。
    pub fn fields(&self) -> &'static [Field] {
        self.kind().fields()
    }

    /// 当前焦点。
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// 当前类型下标（渲染单选行用）。
    pub fn kind_index(&self) -> usize {
        self.kind_index
    }

    /// 字段值。
    pub fn value(&self, index: usize) -> &str {
        self.values[self.kind_index]
            .get(index)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// 该字段是否被判为非法（提交后仍为空）。
    pub fn is_invalid(&self, index: usize) -> bool {
        self.invalid.contains(&index)
    }

    /// 当前焦点字段（焦点不在字段上时为 None）。
    pub fn focused_field(&self) -> Option<usize> {
        match self.focus {
            Focus::Field(index) => Some(index),
            _ => None,
        }
    }

    /// 切到另一种类型。
    /// 编辑已有后端时调用：密文字段可以留空（表示沿用现值）。
    pub fn set_edit_mode(&mut self, editing: bool) {
        self.edit_mode = editing;
    }

    /// 是否处于"编辑已有后端"模式。
    pub fn edit_mode(&self) -> bool {
        self.edit_mode
    }

    pub fn set_kind(&mut self, index: usize) {
        if index < WizardKind::ALL.len() {
            self.kind_index = index;
            self.follow_focus = true;
            self.scroll = 0;
            // 换类型后字段数会变，焦点可能越界：回到类型行最安全
            if matches!(self.focus, Focus::Field(_)) {
                self.focus = Focus::Kind;
            }
            self.invalid.clear();
            self.message = None;
        }
    }

    pub(crate) fn focus_kind(&mut self) {
        self.focus = Focus::Kind;
        self.follow_focus = true;
    }

    /// 类型左移。
    pub fn kind_left(&mut self) {
        self.set_kind(self.kind_index.saturating_sub(1));
    }

    /// 类型右移。
    pub fn kind_right(&mut self) {
        self.set_kind((self.kind_index + 1).min(WizardKind::ALL.len() - 1));
    }

    /// 滚轮/翻页暂时脱离焦点跟随，下次换焦点再跟随。
    pub fn scroll_by(&mut self, delta: i32) {
        self.scroll = (self.scroll as i32 + delta).clamp(0, self.max_scroll as i32) as u16;
        self.follow_focus = false;
    }

    pub(crate) fn viewport(&mut self, rows: u16, lines: usize, focus_line: usize) {
        self.max_scroll = lines.saturating_sub(rows as usize).min(u16::MAX as usize) as u16;
        if self.follow_focus {
            let focus_line = focus_line.min(u16::MAX as usize) as u16;
            if focus_line < self.scroll {
                self.scroll = focus_line;
            } else if focus_line >= self.scroll.saturating_add(rows) {
                self.scroll = focus_line.saturating_add(1).saturating_sub(rows);
            }
        }
        self.scroll = self.scroll.min(self.max_scroll);
        self.follow_focus = false;
    }

    /// 焦点移到下一行（Tab / ↓）：类型 → 字段… → 两个按钮 → 回到类型。
    pub fn focus_next(&mut self) {
        self.follow_focus = true;
        let fields = self.fields().len();
        self.focus = match self.focus {
            Focus::Kind => Focus::Field(0),
            Focus::Field(index) if index + 1 < fields => Focus::Field(index + 1),
            Focus::Field(_) => Focus::Save,
            Focus::Save => Focus::Cancel,
            Focus::Cancel => Focus::Kind,
        };
    }

    /// 焦点移到上一行（Shift+Tab / ↑）。
    pub fn focus_prev(&mut self) {
        self.follow_focus = true;
        let fields = self.fields().len();
        self.focus = match self.focus {
            Focus::Kind => Focus::Cancel,
            Focus::Field(0) => Focus::Kind,
            Focus::Field(index) => Focus::Field(index - 1),
            Focus::Save => Focus::Field(fields - 1),
            Focus::Cancel => Focus::Save,
        };
    }

    /// 把焦点放到某字段（鼠标点击用）。
    pub fn focus_field(&mut self, index: usize) {
        self.follow_focus = true;
        if index < self.fields().len() {
            self.focus = Focus::Field(index);
        }
    }

    /// 聚焦按钮（鼠标点击用）。
    pub fn focus_button(&mut self, save: bool) {
        self.follow_focus = true;
        self.focus = if save { Focus::Save } else { Focus::Cancel };
    }

    /// 往当前字段输入一个字符。
    pub fn push_char(&mut self, ch: char) {
        if let Focus::Field(index) = self.focus {
            // 选择题不接受自由输入：否则把 "virtual-host" 敲错一个字符，
            // 提交前才发现，等于多一种失败方式
            if self.fields()[index].is_choice() {
                return;
            }
            // 一动手输入就消掉该字段的红标
            self.invalid.retain(|invalid| *invalid != index);
            if let Some(value) = self.values[self.kind_index].get_mut(index) {
                value.push(ch);
            }
            self.message = None;
        }
    }

    /// 在选择题上左右拨动；不是选择题时返回 false（让调用方去处理方向键）。
    pub fn cycle_value(&mut self, delta: i32) -> bool {
        let Focus::Field(index) = self.focus else {
            return false;
        };
        let Some(choices) = self.fields()[index].choices else {
            return false;
        };
        if choices.is_empty() {
            return false;
        }
        let current = self.value(index);
        let index_in_choices = choice_index(choices, current).unwrap_or(0) as i32;
        let count = choices.len() as i32;
        let next = (index_in_choices + delta).rem_euclid(count) as usize;
        let value = choices[next].1.to_string();
        if let Some(slot) = self.values[self.kind_index].get_mut(index) {
            *slot = value;
        }
        // 模板不是普通选择题：拨到哪一项就顺手把端点/区域/寻址填好
        if self.kind() == WizardKind::S3 && index == s3_fields::TEMPLATE {
            self.apply_s3_template(index);
        }
        self.invalid.retain(|invalid| *invalid != index);
        self.message = None;
        self.follow_focus = true;
        true
    }

    /// 按当前模板填充端点 / 区域 / 寻址。
    ///
    /// 只在用户**主动选了一个模板**时覆盖；选回"自定义"不动任何东西——
    /// 否则手填的端点会被悄悄清掉。
    fn apply_s3_template(&mut self, index: usize) {
        let template = self.value(index).to_string();
        let Some((endpoint, region, addressing)) = s3_template(&template) else {
            return;
        };
        let row = &mut self.values[self.kind_index];
        if let Some(slot) = row.get_mut(s3_fields::ENDPOINT) {
            *slot = endpoint.to_string();
        }
        if let Some(slot) = row.get_mut(s3_fields::REGION) {
            *slot = region.to_string();
        }
        if let Some(slot) = row.get_mut(s3_fields::ADDRESSING) {
            *slot = addressing.to_string();
        }
    }

    /// 当前字段是不是选择题（渲染与按键路由都要用）。
    pub fn focused_is_choice(&self) -> bool {
        matches!(self.focus, Focus::Field(index) if self.fields()[index].is_choice())
    }

    /// 删掉当前字段末尾一个字符。
    pub fn backspace(&mut self) {
        if let Focus::Field(index) = self.focus
            && let Some(value) = self.values[self.kind_index].get_mut(index)
        {
            value.pop();
        }
    }

    /// 用示例值填充当前字段。
    pub fn fill_sample(&mut self) {
        if let Focus::Field(index) = self.focus {
            let sample = self.fields()[index].sample;
            if let Some(value) = self.values[self.kind_index].get_mut(index) {
                *value = sample.to_string();
            }
        }
    }

    /// 清空当前字段（选择题回到第一项）。
    pub fn clear_field(&mut self) {
        let Focus::Field(index) = self.focus else {
            return;
        };
        let reset = match self.fields()[index].choices {
            Some(choices) if !choices.is_empty() => choices[0].1.to_string(),
            _ => String::new(),
        };
        if let Some(value) = self.values[self.kind_index].get_mut(index) {
            *value = reset;
        }
    }

    /// 提交：校验必填项，通过则给出草稿。
    ///
    /// **一次标出所有空的必填项**并把焦点落到第一个上：像表单一样改完再交，
    /// 而不是"问答式"一步一拦、还把已填内容丢掉。
    pub fn submit(&mut self) -> Result<BackendDraft, Vec<usize>> {
        // S3 的两个预填/校验放在必填检查之前：
        // 1) 区域留空时按端点推断，用户不必自己记 cn-beijing 这类串；
        // 2) 寻址只接受三个取值，就地拦下而不是等命令层报错。
        // （权威的区域默认值在 cx-backend 构造后端时同样推导，这里只是表单预填）
        if self.kind() == WizardKind::S3 {
            if self.value(s3_fields::REGION).trim().is_empty()
                && let Some(region) = region_from_oss_endpoint(self.value(s3_fields::ENDPOINT))
            {
                self.values[self.kind_index][s3_fields::REGION] = region;
            }
            let addressing = self.value(s3_fields::ADDRESSING).trim().to_string();
            if !matches!(addressing.as_str(), "" | "auto" | "virtual-host" | "path") {
                self.invalid = vec![s3_fields::ADDRESSING];
                self.focus = Focus::Field(s3_fields::ADDRESSING);
                self.follow_focus = true;
                self.message = Some("寻址只能填 auto、virtual-host 或 path".to_string());
                return Err(vec![s3_fields::ADDRESSING]);
            }
        }

        let missing: Vec<usize> = self
            .fields()
            .iter()
            .enumerate()
            .filter(|(index, field)| {
                let anonymous_password = self.kind() == WizardKind::Webdav
                    && field.label == "口令"
                    && self.value(2).trim().is_empty();
                let keep_existing_secret = self.edit_mode && field.secret;
                field.required()
                    && !anonymous_password
                    && !keep_existing_secret
                    && if field.secret {
                        self.value(*index).is_empty()
                    } else {
                        self.value(*index).trim().is_empty()
                    }
            })
            .map(|(index, _)| index)
            .collect();
        if !missing.is_empty() {
            self.invalid = missing.clone();
            self.message = Some(format!("还有 {} 个必填项没填", missing.len()));
            self.focus = Focus::Field(missing[0]);
            self.follow_focus = true;
            return Err(missing);
        }
        self.invalid.clear();
        self.message = None;
        Ok(BackendDraft {
            kind: self.kind(),
            values: self.values[self.kind_index].clone(),
        })
    }

    /// 鼠标命中测试：表单内容区的相对行 → 该聚焦的行。
    ///
    /// 0 = 类型行，1..=N = 字段，接着是 [保存] 与 [取消]；由渲染层把屏幕坐标换算成相对行。
    pub fn row_at(&self, row: usize) -> Option<Focus> {
        let fields = self.fields().len();
        if row == 0 {
            return Some(Focus::Kind);
        }
        let index = row - 1;
        if index < fields {
            return Some(Focus::Field(index));
        }
        match index - fields {
            0 => Some(Focus::Save),
            1 => Some(Focus::Cancel),
            _ => None,
        }
    }
}

/// 表单预填用的区域推断（与 cx-backend 的权威实现同规则：只认 `oss-<region>` 与 `s3.oss-<region>`）。
fn region_from_oss_endpoint(endpoint: &str) -> Option<String> {
    let host = endpoint
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(endpoint);
    let host = host.split(['/', ':']).next().unwrap_or_default();
    let rest = host.strip_suffix(".aliyuncs.com")?;
    let rest = rest.strip_prefix("s3.").unwrap_or(rest);
    let rest = rest.strip_prefix("oss-")?;
    let rest = rest.strip_suffix("-internal").unwrap_or(rest);
    (rest.contains('-') && !rest.contains('.')).then(|| rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_into(form: &mut Wizard, index: usize, text: &str) {
        form.focus_field(index);
        form.clear_field();
        for ch in text.chars() {
            form.push_char(ch);
        }
    }

    /// 按标签取字段下标：表单结构以后再插字段，这些断言不会跟着一起错。
    fn idx(kind: WizardKind, label: &str) -> usize {
        kind.fields()
            .iter()
            .position(|field| field.label == label)
            .unwrap_or_else(|| panic!("{kind:?} 表单里没有「{label}」字段"))
    }

    #[test]
    fn 一屏之内有类型行_全部字段与两个按钮() {
        for kind in WizardKind::ALL {
            assert!(kind.fields().len() >= 4, "{} 字段太少", kind.label());
            assert_eq!(kind.fields()[0].label, "后端名");
            assert!(!kind.hint().is_empty());
        }
        let form = Wizard::new();
        let fields = form.fields().len();
        assert_eq!(form.row_at(0), Some(Focus::Kind));
        assert_eq!(form.row_at(1), Some(Focus::Field(0)));
        assert_eq!(form.row_at(fields), Some(Focus::Field(fields - 1)));
        assert_eq!(form.row_at(fields + 1), Some(Focus::Save));
        assert_eq!(form.row_at(fields + 2), Some(Focus::Cancel));
        assert_eq!(form.row_at(fields + 3), None, "越界行没有焦点");
    }

    #[test]
    fn s3_字段说明覆盖最容易踩的两点() {
        let fields = WizardKind::S3.fields();
        let bucket = fields.iter().find(|f| f.label == "桶名").unwrap();
        assert!(
            bucket.help.contains("不自动创建"),
            "默认检查不能自动建桶：{}",
            bucket.help
        );
        let endpoint = fields.iter().find(|f| f.label == "端点").unwrap();
        assert!(
            endpoint.help.contains("path-style"),
            "自建端点的默认寻址方式要说明：{}",
            endpoint.help
        );
        assert!(endpoint.help.contains("留空"), "AWS 官方该留空也要说明");
    }

    #[test]
    fn 密钥类字段是密文输入() {
        assert!(
            WizardKind::S3
                .fields()
                .iter()
                .any(|f| f.label == "SecretAccessKey" && f.secret)
        );
        assert!(
            WizardKind::Webdav
                .fields()
                .iter()
                .any(|f| f.label == "口令" && f.secret)
        );
        assert!(
            WizardKind::Server
                .fields()
                .iter()
                .any(|f| f.label == "注册密钥" && f.secret)
        );
    }

    #[test]
    fn 焦点在类型_字段_按钮之间循环() {
        let mut form = Wizard::new();
        assert_eq!(form.focus(), Focus::Kind);
        form.focus_next();
        assert_eq!(form.focus(), Focus::Field(0));
        for _ in 1..form.fields().len() {
            form.focus_next();
        }
        assert_eq!(form.focus(), Focus::Field(form.fields().len() - 1));
        form.focus_next();
        assert_eq!(form.focus(), Focus::Save);
        form.focus_next();
        assert_eq!(form.focus(), Focus::Cancel);
        form.focus_next();
        assert_eq!(form.focus(), Focus::Kind, "循环回到类型行");

        form.focus_prev();
        assert_eq!(form.focus(), Focus::Cancel, "反向同样循环");
        form.focus_prev();
        assert_eq!(form.focus(), Focus::Save);
        form.focus_prev();
        assert_eq!(form.focus(), Focus::Field(form.fields().len() - 1));
    }

    #[test]
    fn 鼠标点某一行就聚焦它_点越界不动() {
        let mut form = Wizard::new();
        form.focus_field(2);
        assert_eq!(form.focus(), Focus::Field(2));
        form.focus_button(false);
        assert_eq!(form.focus(), Focus::Cancel);
        form.focus_button(true);
        assert_eq!(form.focus(), Focus::Save);
        let before = form.focus();
        form.focus_field(99);
        assert_eq!(form.focus(), before, "点空白处不该改变焦点");
    }

    #[test]
    fn 提交时一次标出所有缺失必填项且不丢已填内容() {
        let kind = WizardKind::S3;
        let mut form = Wizard::new();
        form.set_kind(2); // S3
        let (bucket, ak, sk, region) = (
            idx(kind, "桶名"),
            idx(kind, "AccessKeyId"),
            idx(kind, "SecretAccessKey"),
            idx(kind, "区域"),
        );
        type_into(&mut form, bucket, "clipboardx");
        type_into(&mut form, ak, "AKID");
        let missing = form.submit().unwrap_err();
        assert!(missing.contains(&region), "区域必填：{missing:?}");
        assert!(missing.contains(&sk), "密钥必填：{missing:?}");
        assert_eq!(form.value(bucket), "clipboardx", "已填内容不该被清掉");
        assert_eq!(form.message.as_deref(), Some("还有 2 个必填项没填"));
        assert!(form.is_invalid(region) && form.is_invalid(sk));
        assert_eq!(form.focus(), Focus::Field(region), "焦点落到第一个缺失项");
    }

    #[test]
    fn 补齐后可提交_端点可空() {
        let kind = WizardKind::S3;
        let mut form = Wizard::new();
        form.set_kind(2);
        type_into(&mut form, idx(kind, "桶名"), "bucket");
        type_into(&mut form, idx(kind, "区域"), "cn-hangzhou");
        type_into(&mut form, idx(kind, "AccessKeyId"), "AKID");
        type_into(&mut form, idx(kind, "SecretAccessKey"), "SECRET");
        let draft = form.submit().unwrap();
        assert_eq!(draft.kind, WizardKind::S3);
        assert_eq!(draft.name(), "oss");
        assert_eq!(draft.get("桶名"), "bucket");
        assert_eq!(draft.get("区域"), "cn-hangzhou");
        assert_eq!(draft.get("SecretAccessKey"), "SECRET");
        assert_eq!(draft.get("端点"), "", "端点可空");
        assert!(form.message.is_none());
    }

    #[test]
    fn 一动手输入就消掉红标() {
        let mut form = Wizard::new();
        form.set_kind(1); // WebDAV：URL 必填
        form.submit().unwrap_err();
        assert!(form.is_invalid(1));
        form.focus_field(1);
        form.push_char('h');
        assert!(!form.is_invalid(1));
    }

    #[test]
    fn s3_区域按端点推断且非法寻址就地拦下() {
        let kind = WizardKind::S3;
        let (bucket, endpoint, ak, sk, addressing) = (
            idx(kind, "桶名"),
            idx(kind, "端点"),
            idx(kind, "AccessKeyId"),
            idx(kind, "SecretAccessKey"),
            idx(kind, "寻址"),
        );
        let mut form = Wizard::new();
        form.set_kind(2);
        for (index, value) in [
            (0, "oss"),
            (bucket, "clipboard-sync"),
            (endpoint, "https://oss-cn-beijing.aliyuncs.com"),
            (ak, "fake-ak"),
            (sk, "fake-sk"),
        ] {
            type_into(&mut form, index, value);
        }
        let draft = form.submit().expect("区域应当被自动推断出来");
        assert_eq!(draft.get("区域"), "cn-beijing");
        assert_eq!(draft.get("寻址"), "", "留空表示自动");

        // 寻址是**选择题**：打字输不进错误值，←/→ 只在三个合法取值之间拨
        let mut picked = Wizard::new();
        picked.set_kind(2);
        for (index, value) in [
            (0, "oss"),
            (bucket, "b"),
            (endpoint, "https://oss-cn-beijing.aliyuncs.com"),
            (ak, "ak"),
            (sk, "sk"),
        ] {
            type_into(&mut picked, index, value);
        }
        picked.focus_field(addressing);
        picked.push_char('x');
        assert_eq!(picked.value(addressing), "", "选择题不接受自由输入");
        assert!(picked.cycle_value(1));
        assert_eq!(picked.value(addressing), "virtual-host");
        assert!(picked.cycle_value(1));
        assert_eq!(picked.value(addressing), "path");
        assert!(picked.cycle_value(1), "拨到最后应当绕回第一项");
        assert_eq!(picked.value(addressing), "");
        assert!(picked.cycle_value(-1));
        assert_eq!(picked.value(addressing), "path", "向左也能反向拨");
        picked.cycle_value(1);
        assert_eq!(
            picked.submit().expect("选择题出来的值必定合法").get("寻址"),
            "",
            "绕回自动"
        );

        // 兜底：手工改过 config.toml 之类的路径仍可能带来非法值，
        // 提交时依旧就地拦下（而不是等命令层报错）
        let mut values = vec![String::new(); kind.fields().len()];
        values[0] = "oss".into();
        values[bucket] = "b".into();
        values[endpoint] = "https://oss-cn-beijing.aliyuncs.com".into();
        values[idx(kind, "区域")] = "cn-beijing".into();
        values[ak] = "ak".into();
        values[sk] = "sk".into();
        values[addressing] = "virtual".into();
        let mut restored = Wizard::from_draft(
            &BackendDraft {
                kind: WizardKind::S3,
                values,
            },
            String::new(),
        );
        assert_eq!(
            restored.submit().unwrap_err(),
            vec![addressing],
            "非法寻址必须就地报错，而不是等命令层"
        );
    }

    #[test]
    fn s3_模板一次填好端点区域与寻址() {
        let kind = WizardKind::S3;
        let template = idx(kind, "模板");
        let (endpoint, region, addressing) =
            (idx(kind, "端点"), idx(kind, "区域"), idx(kind, "寻址"));
        let mut form = Wizard::new();
        form.set_kind(2);
        form.focus_field(template);
        assert_eq!(form.value(template), "", "默认自定义");

        // 选「阿里云 OSS（北京）」：端点与区域成对填好，寻址留自动
        assert!(form.cycle_value(1));
        assert_eq!(form.value(template), "oss-beijing");
        assert_eq!(form.value(endpoint), "https://oss-cn-beijing.aliyuncs.com");
        assert_eq!(form.value(region), "cn-beijing");
        assert_eq!(form.value(addressing), "", "OSS 走自动（=virtual-host）");

        // 换 MinIO：端点、区域、寻址三样一起换（本机要 path 式）
        form.cycle_value(1);
        form.cycle_value(1);
        assert_eq!(form.value(template), "minio");
        assert_eq!(form.value(endpoint), "http://127.0.0.1:9000");
        assert_eq!(form.value(region), "us-east-1");
        assert_eq!(form.value(addressing), "path");

        // 选回「自定义」不再改任何字段：手填的区域必须保持原样
        // （拨到「自定义」时模板不写任何字段，既不覆盖也不清空 ——
        //  「选个选项反而弄丢已填内容」是最招人烦的一类 bug）
        type_into(&mut form, region, "eu-west-1");
        form.focus_field(template);
        form.cycle_value(3);
        assert_eq!(form.value(template), "");
        assert_eq!(form.value(region), "eu-west-1", "拨到自定义不该动已有值");
        form.cycle_value(-1);
        assert_eq!(form.value(template), "aws", "往左拨会绕到最后一项");
        assert_eq!(form.value(region), "us-east-1", "AWS 模板会填区域");
        form.cycle_value(1);
        assert_eq!(
            form.value(region),
            "us-east-1",
            "从 AWS 拨回自定义，区域保持模板给的值"
        );
    }

    #[test]
    fn 选择模板后不必自己填端点与区域就能提交() {
        let kind = WizardKind::S3;
        let mut form = Wizard::new();
        form.set_kind(2);
        form.focus_field(idx(kind, "模板"));
        form.cycle_value(1); // 阿里云 OSS（北京）
        type_into(&mut form, idx(kind, "桶名"), "clipboard-sync");
        type_into(&mut form, idx(kind, "AccessKeyId"), "ak");
        type_into(&mut form, idx(kind, "SecretAccessKey"), "sk");
        let draft = form.submit().expect("模板已填好端点与区域");
        assert_eq!(draft.get("端点"), "https://oss-cn-beijing.aliyuncs.com");
        assert_eq!(draft.get("区域"), "cn-beijing");
    }

    #[test]
    fn 换类型时字段跟着换且焦点回到安全位置() {
        let mut form = Wizard::new();
        form.set_kind(2);
        form.focus_field(5);
        form.set_kind(1);
        assert_eq!(form.fields().len(), 4);
        assert_eq!(form.focus(), Focus::Kind);
        assert_eq!(form.value(0), "dav", "每种类型的后端名各自预填");
    }

    #[test]
    fn 匿名_webdav_不要求口令而账号模式要求() {
        let mut form = Wizard::new();
        form.set_kind(1);
        type_into(&mut form, 1, "https://example.invalid/dav");
        assert!(form.submit().is_ok(), "匿名用户名与口令都可空");
        type_into(&mut form, 2, "user");
        assert!(form.submit().unwrap_err().contains(&3));
        type_into(&mut form, 3, "  ");
        assert_eq!(
            form.submit().unwrap().get("口令"),
            "  ",
            "合法秘密不能 trim"
        );
    }

    #[test]
    fn 按示例一键填入() {
        let kind = WizardKind::S3;
        let (endpoint, region) = (idx(kind, "端点"), idx(kind, "区域"));
        let mut form = Wizard::new();
        form.set_kind(2);
        form.focus_field(endpoint);
        form.fill_sample();
        assert_eq!(form.value(endpoint), "https://oss-cn-hangzhou.aliyuncs.com");
        form.focus_field(region);
        form.fill_sample();
        assert_eq!(form.value(region), "cn-hangzhou");
    }
}
