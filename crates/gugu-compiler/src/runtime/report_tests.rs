//! 报告模型、emergency buffer 与渲染的确定性测试。
//!
//! NDJSON 的字段顺序、schema 名与退出尾部必须稳定；截断后 JSON 仍然合法，
//! `location` 与 `exit_code` 始终完整。

use super::report::{
    EmergencyBuffer, EmittedReport, RenderFormat, ReportLedger, SourceLocation, collect_backtrace,
    render_format,
};
use super::startup::{BacktraceMode, DiagnosticsFormat};
use super::startup_schema::{ExitCategory, ReportEvent, ReportReason};

fn location() -> SourceLocation {
    SourceLocation {
        file: "main.gg".to_owned(),
        line: 3,
        column: 5,
    }
}

fn emit_fatal(ledger: &mut ReportLedger, format: RenderFormat) -> &EmittedReport {
    ledger.emit(
        ReportEvent::Termination,
        ExitCategory::RuntimeFailure,
        ReportReason::OutOfMemory,
        Some("GC heap limit exceeded while allocating 4096 bytes".to_owned()),
        None,
        Vec::new(),
        2,
        format,
    );
    ledger.emitted().last().expect("已发布报告")
}

#[test]
fn json_report_matches_contract_schema_field_order() {
    let mut ledger = ReportLedger::new();
    let report = emit_fatal(&mut ledger, RenderFormat::Json);
    assert_eq!(
        report.text(),
        "{\"schema\":\"gugu-runtime-report-v1\",\"event\":\"termination\",\"class\":\"runtime-failure\",\"reason\":\"out-of-memory\",\"message\":\"GC heap limit exceeded while allocating 4096 bytes\",\"location\":null,\"backtrace\":[],\"exit_code\":2}\n"
    );
    assert!(!report.truncated());
    let record = report.record();
    assert_eq!(record.epoch(), 0);
    assert_eq!(record.event(), ReportEvent::Termination);
    assert_eq!(record.class(), ExitCategory::RuntimeFailure);
    assert_eq!(record.reason(), ReportReason::OutOfMemory);
    assert_eq!(record.exit_code(), 2);
}

#[test]
fn panic_event_carries_location_object() {
    let mut ledger = ReportLedger::new();
    ledger.emit(
        ReportEvent::Panic,
        ExitCategory::ProgramFailure,
        ReportReason::UnhandledPanic,
        Some("index out of bounds".to_owned()),
        Some(location()),
        Vec::new(),
        1,
        RenderFormat::Json,
    );
    let report = ledger.emitted().last().expect("已发布报告");
    assert_eq!(
        report.text(),
        "{\"schema\":\"gugu-runtime-report-v1\",\"event\":\"panic\",\"class\":\"program-failure\",\"reason\":\"unhandled-panic\",\"message\":\"index out of bounds\",\"location\":{\"file\":\"main.gg\",\"line\":3,\"column\":5},\"backtrace\":[],\"exit_code\":1}\n"
    );
}

#[test]
fn json_escapes_control_characters_and_quotes() {
    let mut ledger = ReportLedger::new();
    ledger.emit(
        ReportEvent::Termination,
        ExitCategory::RuntimeFailure,
        ReportReason::RuntimeInvariant,
        Some("bad \"value\"\n\t\x01".to_owned()),
        None,
        Vec::new(),
        2,
        RenderFormat::Json,
    );
    let text = ledger.emitted()[0].text();
    assert!(text.contains("\"message\":\"bad \\\"value\\\"\\n\\t\\u0001\""));
}

#[test]
fn text_report_contains_required_elements() {
    let mut ledger = ReportLedger::new();
    let report = emit_fatal(&mut ledger, RenderFormat::Text);
    let text = report.text();
    assert!(text.starts_with("runtime-report\n"));
    assert!(text.contains("event: termination\n"));
    assert!(text.contains("class: runtime-failure\n"));
    assert!(text.contains("reason: out-of-memory\n"));
    assert!(text.contains("message: GC heap limit exceeded while allocating 4096 bytes\n"));
    assert!(text.contains("exit: runtime-failure (2)\n"));
}

#[test]
fn both_format_writes_text_then_json() {
    let mut ledger = ReportLedger::new();
    let report = emit_fatal(&mut ledger, RenderFormat::Both);
    let text = report.text();
    let json_start = text.find("{\"schema\":").expect("JSON 报告存在");
    assert!(text.starts_with("runtime-report\n"));
    assert!(text[json_start..].contains("\"exit_code\":2}\n"));
}

#[test]
fn emergency_format_is_fixed_plain_text() {
    let mut ledger = ReportLedger::new();
    let report = emit_fatal(&mut ledger, RenderFormat::Emergency);
    let text = report.text();
    assert!(text.starts_with("gugu emergency report\n"));
    assert!(text.contains("reason: out-of-memory\n"));
    assert!(text.contains("exit: runtime-failure (2)\n"));
    assert!(!text.contains("{"));
}

#[test]
fn oversized_message_truncates_but_keeps_exit_tail() {
    let mut ledger = ReportLedger::new();
    let long = "字".repeat(8000);
    ledger.emit(
        ReportEvent::Termination,
        ExitCategory::RuntimeFailure,
        ReportReason::OutOfMemory,
        Some(long),
        Some(SourceLocation {
            file: "deep/nested/module.gg".to_owned(),
            line: 128,
            column: 12,
        }),
        Vec::new(),
        2,
        RenderFormat::Json,
    );
    let report = ledger.emitted().last().expect("已发布报告");
    assert!(report.truncated());
    let text = report.text();
    assert!(text.ends_with(",\"exit_code\":2}\n"));
    // location 结构完整；文件名按截断策略最后收缩。
    assert!(text.contains("\"location\":{\"file\":\""));
    assert!(text.contains(",\"line\":128,\"column\":12}"));
    assert!(text.contains("\"backtrace\":[]"));
}

#[test]
fn backtrace_frames_drop_from_tail() {
    let mut ledger = ReportLedger::new();
    let frames: Vec<String> = (0..64)
        .map(|index| "frame-".repeat(40) + &index.to_string())
        .collect();
    ledger.emit(
        ReportEvent::Termination,
        ExitCategory::RuntimeFailure,
        ReportReason::StackOverflow,
        Some("stack overflow".to_owned()),
        None,
        frames,
        2,
        RenderFormat::Json,
    );
    let report = ledger.emitted().last().expect("已发布报告");
    assert!(report.truncated());
    let text = report.text();
    assert!(text.ends_with("\"],\"exit_code\":2}\n"));
    assert!(text.contains("\"backtrace\":[\""));
}

#[test]
fn multibyte_truncation_stays_on_char_boundary() {
    let mut buffer = EmergencyBuffer::new();
    buffer.push_str("方案：");
    for _ in 0..5000 {
        buffer.push_str("协");
    }
    assert!(buffer.truncated());
    assert_eq!(buffer.as_str().chars().last(), Some('协'));
}

#[test]
fn ledger_epochs_are_monotonic() {
    let mut ledger = ReportLedger::new();
    assert_eq!(ledger.next_epoch(), 0);
    let first = ledger.emit(
        ReportEvent::Panic,
        ExitCategory::ProgramFailure,
        ReportReason::UnhandledPanic,
        None,
        None,
        Vec::new(),
        1,
        RenderFormat::Json,
    );
    assert_eq!(first, 0);
    let second = ledger.emit(
        ReportEvent::Termination,
        ExitCategory::ProgramFailure,
        ReportReason::UnhandledPanic,
        None,
        None,
        Vec::new(),
        1,
        RenderFormat::Json,
    );
    assert_eq!(second, 1);
    assert_eq!(ledger.next_epoch(), 2);
    assert_eq!(ledger.emitted().len(), 2);
}

#[test]
fn render_format_follows_diagnostics_config() {
    assert_eq!(render_format(DiagnosticsFormat::Text), RenderFormat::Text);
    assert_eq!(render_format(DiagnosticsFormat::Json), RenderFormat::Json);
    assert_eq!(render_format(DiagnosticsFormat::Both), RenderFormat::Both);
}

#[test]
fn backtrace_collection_is_best_effort() {
    let frames = vec!["main".to_owned()];
    assert!(collect_backtrace(BacktraceMode::Off, &frames).is_empty());
    assert_eq!(
        collect_backtrace(BacktraceMode::Triggering, &frames),
        frames
    );
    assert_eq!(collect_backtrace(BacktraceMode::Full, &frames), frames);
    assert!(collect_backtrace(BacktraceMode::Triggering, &[]).is_empty());
}

#[test]
fn location_backtrace_both_present_still_render_exit() {
    let mut ledger = ReportLedger::new();
    let long_file = "d/".repeat(200) + "end.gg";
    ledger.emit(
        ReportEvent::Panic,
        ExitCategory::ProgramFailure,
        ReportReason::UnhandledPanic,
        Some("m".to_owned()),
        Some(SourceLocation {
            file: long_file,
            line: 1,
            column: 1,
        }),
        vec!["f".repeat(120)],
        1,
        RenderFormat::Json,
    );
    let report = ledger.emitted().last().expect("已发布报告");
    assert!(report.text().contains(",\"exit_code\":1}\n"));
}
