//! runtime 报告模型、emergency buffer 与 text/NDJSON 渲染的确定性参照实现。
//!
//! 报告只面向 stderr：不调用用户格式化 trait、不使用普通分配器、不做异步 I/O。
//! 渲染直接写进固定容量的 emergency buffer；空间不足时先截断 message，再从尾部
//! 丢弃 backtrace 帧，`location` 与 `exit_code` 通过预留始终完整，NDJSON 始终合法。

use super::startup::{BacktraceMode, DiagnosticsFormat};
use super::startup_schema::{ExitCategory, REPORT_SCHEMA_NAME, ReportEvent, ReportReason};

/// 报告中的源位置；`Panic.location` 是原始 panic 调用点。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceLocation {
    pub(crate) file: String,
    pub(crate) line: u32,
    pub(crate) column: u32,
}

/// 一条报告事件的完整内容。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReportRecord {
    epoch: u64,
    event: ReportEvent,
    class: ExitCategory,
    reason: ReportReason,
    message: Option<String>,
    location: Option<SourceLocation>,
    frames: Vec<String>,
    exit_code: i64,
}

impl ReportRecord {
    /// 返回报告序号。
    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 返回事件类别。
    pub(crate) const fn event(&self) -> ReportEvent {
        self.event
    }

    /// 返回逻辑退出类别。
    pub(crate) const fn class(&self) -> ExitCategory {
        self.class
    }

    /// 返回 reason 短名。
    pub(crate) const fn reason(&self) -> ReportReason {
        self.reason
    }

    /// 返回消息。
    pub(crate) fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// 返回源位置。
    pub(crate) fn location(&self) -> Option<&SourceLocation> {
        self.location.as_ref()
    }

    /// 返回回溯帧。
    pub(crate) fn frames(&self) -> &[String] {
        &self.frames
    }

    /// 返回退出码。
    pub(crate) const fn exit_code(&self) -> i64 {
        self.exit_code
    }
}

/// 定容、无分配的报告缓冲区。
#[derive(Clone, Debug)]
pub(crate) struct EmergencyBuffer {
    bytes: Box<[u8; super::startup_schema::EMERGENCY_BUFFER_BYTES as usize]>,
    len: usize,
    truncated: bool,
}

impl EmergencyBuffer {
    /// 创建空缓冲区。
    pub(crate) fn new() -> Self {
        Self {
            bytes: Box::new([0; super::startup_schema::EMERGENCY_BUFFER_BYTES as usize]),
            len: 0,
            truncated: false,
        }
    }

    /// 返回剩余可写字节数。
    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len() - self.len
    }

    /// 返回是否发生过截断。
    pub(crate) const fn truncated(&self) -> bool {
        self.truncated
    }

    /// 返回已渲染的文本；截断只发生在字符边界。
    pub(crate) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).expect("缓冲区只接收合法 UTF-8")
    }

    /// 追加文本；超出容量时在字符边界截断并标记。
    pub(crate) fn push_str(&mut self, text: &str) {
        let remaining = self.remaining();
        if text.len() <= remaining {
            self.bytes[self.len..self.len + text.len()].copy_from_slice(text.as_bytes());
            self.len += text.len();
            return;
        }
        let mut take = remaining;
        while take > 0 && !text.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        self.truncated = true;
    }

    /// 追加十进制整数。
    fn push_i64(&mut self, value: i64) {
        const DIGITS: &[u8; 10] = b"0123456789";
        let magnitude = value.unsigned_abs();
        let mut digits = [0u8; 20];
        let mut count = 0;
        let mut rest = magnitude;
        loop {
            digits[count] = DIGITS[usize::try_from(rest % 10).expect("十进制余数必在范围内")];
            count += 1;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        if value < 0 {
            self.push_str("-");
        }
        for index in (0..count).rev() {
            let byte = [digits[index]];
            let text = std::str::from_utf8(&byte).expect("十进制数字是 ASCII");
            self.push_str(text);
        }
    }

    /// 追加文本；写入量不超过 `budget`，超出部分按字符边界截断并标记。
    pub(crate) fn push_str_bounded(&mut self, text: &str, budget: usize) {
        let mut take = text.len().min(budget);
        while take > 0 && !text.is_char_boundary(take) {
            take -= 1;
        }
        self.push_str(&text[..take]);
        if take < text.len() {
            self.truncated = true;
        }
    }

    /// 追加 JSON 字符串字面量（含引号）。
    ///
    /// 转义后的全部输出不超过 `budget` 字节；超出时在字符边界截断并标记。
    /// 调用方必须保证 `budget` 不超过当前剩余空间与其后内容的预留之和。
    fn push_json_string_bounded(&mut self, raw: &str, budget: usize) {
        self.push_str("\"");
        let mut used = 1usize;
        for character in raw.chars() {
            let escaped = escape_len(character);
            if used + escaped + 1 > budget {
                self.truncated = true;
                break;
            }
            push_escaped(self, character);
            used += escaped;
        }
        self.push_str("\"");
    }
}

/// 返回字符转义后的宽度。
fn escape_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        character if (character as u32) < 0x20 => 6,
        _ => character.len_utf8(),
    }
}

/// 把单个字符按 JSON 转义写进缓冲区。
fn push_escaped(buffer: &mut EmergencyBuffer, character: char) {
    match character {
        '"' => buffer.push_str("\\\""),
        '\\' => buffer.push_str("\\\\"),
        '\u{8}' => buffer.push_str("\\b"),
        '\u{c}' => buffer.push_str("\\f"),
        '\n' => buffer.push_str("\\n"),
        '\r' => buffer.push_str("\\r"),
        '\t' => buffer.push_str("\\t"),
        other if (other as u32) < 0x20 => {
            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            let value = other as u32;
            let hex = [
                b'\\',
                b'u',
                b'0',
                b'0',
                DIGITS[usize::try_from((value >> 4) & 0xf).expect("掩码后必在范围内")],
                DIGITS[usize::try_from(value & 0xf).expect("掩码后必在范围内")],
            ];
            let text = std::str::from_utf8(&hex).expect("十六进制转义是 ASCII");
            buffer.push_str(text);
        }
        other => {
            let mut encoded = [0u8; 4];
            buffer.push_str(other.encode_utf8(&mut encoded));
        }
    }
}

/// 报告的渲染形态；emergency 形态不依赖任何诊断配置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RenderFormat {
    /// 人类可读文本。
    Text,
    /// 稳定的逐行 JSON。
    Json,
    /// 依次输出文本与 JSON。
    Both,
    /// 固定纯文本 emergency report。
    Emergency,
}

/// 由诊断配置推导渲染形态。
pub(crate) fn render_format(diagnostics: DiagnosticsFormat) -> RenderFormat {
    match diagnostics {
        DiagnosticsFormat::Text => RenderFormat::Text,
        DiagnosticsFormat::Json => RenderFormat::Json,
        DiagnosticsFormat::Both => RenderFormat::Both,
    }
}

/// 按报告格式渲染一条记录。
pub(crate) fn render(record: &ReportRecord, format: RenderFormat, buffer: &mut EmergencyBuffer) {
    match format {
        RenderFormat::Text => render_text(record, buffer),
        RenderFormat::Json => render_json(record, buffer),
        RenderFormat::Both => {
            render_text(record, buffer);
            render_json(record, buffer);
        }
        RenderFormat::Emergency => render_emergency(record, buffer),
    }
}

/// `,\"backtrace\":[` 加闭合 `]` 的固定字节开销。
const BACKTRACE_OVERHEAD: usize = 15;

/// 返回 location 字段的固定结构开销（不含文件名字节）。
fn location_overhead(record: &ReportRecord) -> usize {
    match &record.location {
        Some(location) => {
            // `,"location":{"file":` + 引号对 + `,"line":` + 行号 + `,"column":` + 列号 + `}`。
            20 + 2
                + 8
                + decimal_digits(i64::from(location.line))
                + 10
                + decimal_digits(i64::from(location.column))
                + 1
        }
        // `,"location":null`。
        None => 16,
    }
}

/// 渲染逐行 JSON；字段顺序与契约登记的 `REPORT_FIELDS` 一致。
///
/// 每个阶段都为其后的阶段预留精确的结构开销：message 先截断，backtrace 帧从尾部
/// 丢弃，location 文件名最后收缩；`exit_code` 与字段结构始终完整，JSON 始终合法。
fn render_json(record: &ReportRecord, buffer: &mut EmergencyBuffer) {
    let exit_tail = exit_tail_size(record.exit_code);
    buffer.push_str("{\"schema\":\"");
    buffer.push_str(REPORT_SCHEMA_NAME);
    buffer.push_str("\",\"event\":\"");
    buffer.push_str(record.event.name());
    buffer.push_str("\",\"class\":\"");
    buffer.push_str(record.class.name());
    buffer.push_str("\",\"reason\":\"");
    buffer.push_str(record.reason.name());
    buffer.push_str("\",\"message\":");
    let reserve = location_overhead(record) + BACKTRACE_OVERHEAD + exit_tail;
    let budget = buffer.remaining().saturating_sub(reserve);
    match &record.message {
        Some(message) => buffer.push_json_string_bounded(message, budget),
        None => buffer.push_str("null"),
    }
    render_json_location(record, buffer, exit_tail);
    render_json_frames(record, buffer, exit_tail);
    buffer.push_str(",\"exit_code\":");
    buffer.push_i64(record.exit_code);
    buffer.push_str("}\n");
}

/// 渲染 location 字段；文件名在预算内收缩。
fn render_json_location(record: &ReportRecord, buffer: &mut EmergencyBuffer, exit_tail: usize) {
    let Some(location) = &record.location else {
        buffer.push_str(",\"location\":null");
        return;
    };
    // location_overhead 含引号对；这里为文件内容预算减去引号。
    let structure = location_overhead(record) - 2;
    let budget = buffer
        .remaining()
        .saturating_sub(structure + BACKTRACE_OVERHEAD + exit_tail);
    buffer.push_str(",\"location\":{\"file\":");
    buffer.push_json_string_bounded(&location.file, budget);
    buffer.push_str(",\"line\":");
    buffer.push_i64(i64::from(location.line));
    buffer.push_str(",\"column\":");
    buffer.push_i64(i64::from(location.column));
    buffer.push_str("}");
}

/// 渲染 backtrace 字段；空间不足时从尾部丢帧。
fn render_json_frames(record: &ReportRecord, buffer: &mut EmergencyBuffer, exit_tail: usize) {
    buffer.push_str(",\"backtrace\":[");
    for (index, frame) in record.frames.iter().enumerate() {
        // 每帧至少还要容纳引号对、分隔逗号、闭合 `]` 与退出尾部。
        if buffer.remaining() <= exit_tail + 4 {
            buffer.truncated = true;
            break;
        }
        if index > 0 {
            buffer.push_str(",");
        }
        let budget = buffer.remaining().saturating_sub(exit_tail + 2);
        buffer.push_json_string_bounded(frame, budget);
    }
    buffer.push_str("]");
}

/// 返回 JSON 退出尾部 `,"exit_code":N}` 加换行的字节数。
fn exit_tail_size(exit_code: i64) -> usize {
    13 + decimal_digits(exit_code) + 2
}

/// 返回十进制整数的位数（含负号）。
fn decimal_digits(value: i64) -> usize {
    let digits = {
        let magnitude = value.unsigned_abs();
        let mut count = 1usize;
        let mut rest = magnitude / 10;
        while rest > 0 {
            count += 1;
            rest /= 10;
        }
        count
    };
    digits + usize::from(value < 0)
}

/// 文本路径为最后的退出行预留的字节数。
const TEXT_TAIL_RESERVE: usize = 96;

/// 渲染文本报告；必须包含事件类别、reason、消息、源位置与退出类别。
fn render_text(record: &ReportRecord, buffer: &mut EmergencyBuffer) {
    buffer.push_str("runtime-report\n");
    buffer.push_str("event: ");
    buffer.push_str(record.event.name());
    buffer.push_str("\nclass: ");
    buffer.push_str(record.class.name());
    buffer.push_str("\nreason: ");
    buffer.push_str(record.reason.name());
    if let Some(message) = &record.message {
        let budget = buffer.remaining().saturating_sub(TEXT_TAIL_RESERVE);
        buffer.push_str("\nmessage: ");
        buffer.push_str_bounded(message, budget);
    }
    if let Some(location) = &record.location {
        buffer.push_str("\nlocation: ");
        let reserve = TEXT_TAIL_RESERVE + 24;
        buffer.push_str_bounded(&location.file, buffer.remaining().saturating_sub(reserve));
        buffer.push_str(":");
        buffer.push_i64(i64::from(location.line));
        buffer.push_str(":");
        buffer.push_i64(i64::from(location.column));
    }
    buffer.push_str("\nbacktrace: ");
    buffer.push_i64(i64::try_from(record.frames.len()).expect("帧数适配 i64"));
    buffer.push_str(" frames\nexit: ");
    buffer.push_str(record.class.name());
    buffer.push_str(" (");
    buffer.push_i64(record.exit_code);
    buffer.push_str(")\n");
}

/// 渲染固定纯文本 emergency report；不依赖 `GUGU_RUNTIME_DIAGNOSTICS`/`GUGU_BACKTRACE`。
fn render_emergency(record: &ReportRecord, buffer: &mut EmergencyBuffer) {
    buffer.push_str("gugu emergency report\n");
    buffer.push_str("event: ");
    buffer.push_str(record.event.name());
    buffer.push_str("\nclass: ");
    buffer.push_str(record.class.name());
    buffer.push_str("\nreason: ");
    buffer.push_str(record.reason.name());
    if let Some(message) = &record.message {
        let budget = buffer.remaining().saturating_sub(TEXT_TAIL_RESERVE);
        buffer.push_str("\nmessage: ");
        buffer.push_str_bounded(message, budget);
    }
    buffer.push_str("\nexit: ");
    buffer.push_str(record.class.name());
    buffer.push_str(" (");
    buffer.push_i64(record.exit_code);
    buffer.push_str(")\n");
}

/// 收集报告回溯；栈图尚未接入时为 best-effort 空列表。
///
/// `GUGU_BACKTRACE=0` 不输出；`1` 输出触发故障协程的帧；`full` 输出全部协程与
/// 工作线程帧。帧收集失败不升级成新的用户 panic。
pub(crate) fn collect_backtrace(mode: BacktraceMode, triggering: &[String]) -> Vec<String> {
    match mode {
        BacktraceMode::Off => Vec::new(),
        BacktraceMode::Triggering | BacktraceMode::Full => triggering.to_vec(),
    }
}

/// 已渲染的报告：内容与固定缓冲区一起保存。
#[derive(Clone, Debug)]
pub(crate) struct EmittedReport {
    record: ReportRecord,
    buffer: EmergencyBuffer,
}

impl EmittedReport {
    /// 返回报告内容。
    pub(crate) const fn record(&self) -> &ReportRecord {
        &self.record
    }

    /// 返回渲染后的报告文本。
    pub(crate) fn text(&self) -> &str {
        self.buffer.as_str()
    }

    /// 返回是否发生过截断。
    pub(crate) const fn truncated(&self) -> bool {
        self.buffer.truncated()
    }
}

/// 报告账本：单调序号与已发布报告；`report_epoch` 冲刷检查以 `next_epoch` 为准。
#[derive(Clone, Debug, Default)]
pub(crate) struct ReportLedger {
    next_epoch: u64,
    emitted: Vec<EmittedReport>,
}

impl ReportLedger {
    /// 创建空账本。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 返回下一个报告的序号；终态计划以该值作为冲刷下界。
    pub(crate) const fn next_epoch(&self) -> u64 {
        self.next_epoch
    }

    /// 返回已发布的报告。
    pub(crate) fn emitted(&self) -> &[EmittedReport] {
        &self.emitted
    }

    /// 发布一条报告并返回其序号。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit(
        &mut self,
        event: ReportEvent,
        class: ExitCategory,
        reason: ReportReason,
        message: Option<String>,
        location: Option<SourceLocation>,
        frames: Vec<String>,
        exit_code: i64,
        format: RenderFormat,
    ) -> u64 {
        let record = ReportRecord {
            epoch: self.next_epoch,
            event,
            class,
            reason,
            message,
            location,
            frames,
            exit_code,
        };
        let mut buffer = EmergencyBuffer::new();
        render(&record, format, &mut buffer);
        self.emitted.push(EmittedReport { record, buffer });
        self.next_epoch += 1;
        self.next_epoch - 1
    }
}
