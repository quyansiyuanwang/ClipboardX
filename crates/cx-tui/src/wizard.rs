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

const S3_FIELDS: [Field; 9] = [
    Field::text("后端名", "oss", "唯一名字"),
    Field::text("桶名", "clipboardx", "请先创建桶；连接检查不自动创建"),
    Field::text(
        "端点",
        "https://oss-cn-hangzhou.aliyuncs.com",
        "AWS 官方留空；OSS 用 virtual-host，MinIO 可用 path-style",
    ),
    Field::text(
        "区域",
        "cn-hangzhou",
        "必须与端点同区域（阿里云杭州 = cn-hangzhou）",
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
        !matches!(self.label, "端点" | "用户名" | "前缀" | "寻址" | "会话令牌")
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
        self.invalid.retain(|invalid| *invalid != index);
        self.message = None;
        self.follow_focus = true;
        true
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
            if self.value(3).trim().is_empty()
                && let Some(region) = region_from_oss_endpoint(self.value(2))
            {
                self.values[self.kind_index][3] = region;
            }
            let addressing = self.value(7).trim().to_string();
            if !matches!(addressing.as_str(), "" | "auto" | "virtual-host" | "path") {
                self.invalid = vec![7];
                self.focus = Focus::Field(7);
                self.follow_focus = true;
                self.message = Some("寻址只能填 auto、virtual-host 或 path".to_string());
                return Err(vec![7]);
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
                field.required()
                    && !anonymous_password
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
        let mut form = Wizard::new();
        form.set_kind(2); // S3
        type_into(&mut form, 1, "clipboardx");
        type_into(&mut form, 4, "AKID");
        let missing = form.submit().unwrap_err();
        assert!(missing.contains(&3), "区域必填：{missing:?}");
        assert!(missing.contains(&5), "密钥必填：{missing:?}");
        assert_eq!(form.value(1), "clipboardx", "已填内容不该被清掉");
        assert_eq!(form.message.as_deref(), Some("还有 2 个必填项没填"));
        assert!(form.is_invalid(3) && form.is_invalid(5));
        assert_eq!(form.focus(), Focus::Field(3), "焦点落到第一个缺失项");
    }

    #[test]
    fn 补齐后可提交_端点可空() {
        let mut form = Wizard::new();
        form.set_kind(2);
        type_into(&mut form, 1, "bucket");
        type_into(&mut form, 3, "cn-hangzhou");
        type_into(&mut form, 4, "AKID");
        type_into(&mut form, 5, "SECRET");
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
        let mut form = Wizard::new();
        form.set_kind(2);
        for (index, value) in [
            (0, "oss"),
            (1, "clipboard-sync"),
            (2, "https://oss-cn-beijing.aliyuncs.com"),
            (4, "fake-ak"),
            (5, "fake-sk"),
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
            (1, "b"),
            (2, "https://oss-cn-beijing.aliyuncs.com"),
            (4, "ak"),
            (5, "sk"),
        ] {
            type_into(&mut picked, index, value);
        }
        picked.focus_field(7);
        picked.push_char('x');
        assert_eq!(picked.value(7), "", "选择题不接受自由输入");
        assert!(picked.cycle_value(1));
        assert_eq!(picked.value(7), "virtual-host");
        assert!(picked.cycle_value(1));
        assert_eq!(picked.value(7), "path");
        assert!(picked.cycle_value(1), "拨到最后应当绕回第一项");
        assert_eq!(picked.value(7), "");
        assert!(picked.cycle_value(-1));
        assert_eq!(picked.value(7), "path", "向左也能反向拨");
        picked.cycle_value(1);
        assert_eq!(
            picked.submit().expect("选择题出来的值必定合法").get("寻址"),
            "",
            "绕回自动"
        );

        // 兜底：手工改过 config.toml 之类的路径仍可能带来非法值，
        // 提交时依旧就地拦下（而不是等命令层报错）
        let mut restored = Wizard::from_draft(
            &BackendDraft {
                kind: WizardKind::S3,
                values: vec![
                    "oss".into(),
                    "b".into(),
                    "https://oss-cn-beijing.aliyuncs.com".into(),
                    "cn-beijing".into(),
                    "ak".into(),
                    "sk".into(),
                    String::new(),
                    "virtual".into(),
                    String::new(),
                ],
            },
            String::new(),
        );
        assert_eq!(
            restored.submit().unwrap_err(),
            vec![7],
            "非法寻址必须就地报错，而不是等命令层"
        );
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
        let mut form = Wizard::new();
        form.set_kind(2);
        form.focus_field(2);
        form.fill_sample();
        assert_eq!(form.value(2), "https://oss-cn-hangzhou.aliyuncs.com");
        form.focus_field(3);
        form.fill_sample();
        assert_eq!(form.value(3), "cn-hangzhou");
    }
}
