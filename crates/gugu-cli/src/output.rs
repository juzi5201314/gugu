use std::{
    env, fmt,
    io::IsTerminal,
    path::{Component, Path, PathBuf},
};

use clap::ValueEnum;
use gugu_compiler::{Compilation, TargetName};
use serde_json::{Value, json};

use crate::GlobalArgs;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum OutputFormat {
    #[default]
    Text,
    Json,
    JsonDiagnosticShort,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::JsonDiagnosticShort => "json-diagnostic-short",
        })
    }
}

impl OutputFormat {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "json-diagnostic-short" => Ok(Self::JsonDiagnosticShort),
            _ => Err(format!(
                "未知输出格式 `{value}`，可选值为 text、json、json-diagnostic-short"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lower")]
pub(crate) enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorMode {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            _ => Err(format!(
                "未知颜色策略 `{value}`，可选值为 auto、always、never"
            )),
        }
    }
}

pub(crate) fn parse_color(value: &str) -> Result<ColorMode, String> {
    ColorMode::parse(value)
}

pub(crate) fn should_color(mode: ColorMode) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => std::io::stderr().is_terminal(),
    }
}

pub(crate) fn emit_event(reason: &str, payload: Value) {
    let envelope = json!({
        "reason": reason,
        "payload": payload
    });
    println!("{envelope}");
}

pub(crate) fn print_compilation(
    compilation: &Compilation,
    check_only: bool,
    options: &GlobalArgs,
    target: TargetName,
) {
    match options.format.unwrap_or_default() {
        OutputFormat::Text => print_compilation_text(compilation, check_only, options, target),
        OutputFormat::Json => print_compilation_json(compilation, check_only, options, target),
        OutputFormat::JsonDiagnosticShort => {
            for diagnostic in compilation.diagnostics().items() {
                emit_diagnostic(diagnostic, OutputFormat::JsonDiagnosticShort);
            }
        }
    }
}

fn wants_dump_gir(options: &GlobalArgs) -> bool {
    options.z.iter().any(|flag| flag == "dump-gir")
}

fn wants_dump_runtime(options: &GlobalArgs) -> bool {
    options.z.iter().any(|flag| flag == "dump-runtime")
}

pub(crate) fn print_compilation_text(
    compilation: &Compilation,
    check_only: bool,
    options: &GlobalArgs,
    target: TargetName,
) {
    if wants_dump_gir(options)
        && let Some(dump) = compilation.dump_gir()
    {
        print!("{dump}");
    }
    if options.z.iter().any(|flag| flag == "dump-lir")
        && let Some(dump) = compilation.dump_lir()
    {
        print!("{dump}");
    }
    if wants_dump_runtime(options)
        && let Some(dump) = compilation.dump_runtime()
    {
        print!("{dump}");
    }
    if options.verbose {
        println!("target: {target}");
    }
    if !options.quiet {
        for node in compilation.action_graph().nodes() {
            println!(
                "action {:02} {:<16} {}",
                node.id(),
                node.kind(),
                node.status()
            );
        }
    }
    for diagnostic in compilation.diagnostics().items() {
        let rendered = diagnostic.render_text();
        if should_color(options.color.unwrap_or_default()) {
            eprintln!("\x1b[1;31m{rendered}\x1b[0m");
        } else {
            eprintln!("{rendered}");
        }
    }
    if let Some(plan) = compilation.image_plan() {
        if check_only {
            println!("check succeeded: {}", plan.target());
        } else {
            println!(
                "bootstrap plan ready: target={}, entry={}, image not emitted",
                plan.target(),
                plan.entry()
            );
        }
    } else if compilation.diagnostics().has_errors() {
        println!("{} failed", if check_only { "check" } else { "build" });
    } else if options.quiet {
        println!(
            "{} succeeded: no executable entry",
            if check_only { "check" } else { "build" }
        );
    }
}

pub(crate) fn print_compilation_json(
    compilation: &Compilation,
    check_only: bool,
    options: &GlobalArgs,
    target: TargetName,
) {
    let command = if check_only { "check" } else { "build" };
    if !options.quiet {
        emit_event(
            "build-start",
            json!({ "command": command, "target": target.to_string() }),
        );
    }
    for diagnostic in compilation.diagnostics().items() {
        emit_diagnostic(diagnostic, OutputFormat::Json);
    }
    if wants_dump_gir(options)
        && let Some(dump) = compilation.dump_gir()
    {
        emit_event(
            "gir-dump",
            json!({
                "text": dump,
                "fingerprint": compilation.gir_fingerprint(),
            }),
        );
    }
    if options.z.iter().any(|flag| flag == "dump-lir")
        && let Some(dump) = compilation.dump_lir()
    {
        emit_event(
            "lir-dump",
            json!({
                "text": dump,
                "fingerprint": compilation.lir_fingerprint(),
            }),
        );
    }
    if wants_dump_runtime(options)
        && let Some(dump) = compilation.dump_runtime()
    {
        emit_event(
            "runtime-dump",
            json!({
                "text": dump,
                "fingerprint": compilation.runtime_raw_fingerprint(),
            }),
        );
    }
    let image_plan = compilation.image_plan().map(|plan| {
        json!({
            "target": plan.target().to_string(),
            "entry": plan.entry(),
            "function-count": plan.function_count(),
            "runtime-source-count": plan.runtime_source_count(),
            "type-id-count": plan.type_id_count(),
            "type-universe-fingerprint": plan.type_universe_fingerprint(),
            "late-constant-count": plan.late_constant_count(),
            "late-constants-fingerprint": plan.late_constants_fingerprint(),
            "gir-body-count": plan.gir_body_count(),
            "gir-block-count": plan.gir_block_count(),
            "gir-statement-count": plan.gir_statement_count(),
            "gir-fingerprint": plan.gir_fingerprint(),
            "lir-body-count": plan.lir_body_count(),
            "lir-block-count": plan.lir_block_count(),
            "lir-instruction-count": plan.lir_instruction_count(),
            "lir-memory-operation-count": plan.lir_memory_operation_count(),
            "lir-safepoint-count": plan.lir_safepoint_count(),
            "lir-fingerprint": plan.lir_fingerprint(),
            "optimization-revision": plan.optimization_revision(),
            "poll-budget": plan.poll_budget(),
            "poll-count": plan.poll_count(),
            "poll-free-leaf-count": plan.poll_free_leaf_count(),
            "poll-summary-fingerprint": plan.poll_summary_fingerprint(),
            "runtime-checks-elided-count": plan.runtime_checks_elided_count(),
            "placement-count": plan.placement_count(),
            "turn-region-count": plan.turn_region_count(),
            "local-heap-count": plan.local_heap_count(),
            "shared-heap-count": plan.shared_heap_count(),
            "placement-fingerprint": plan.placement_fingerprint(),
            "raw-size-class-count": plan.raw_size_class_count(),
            "raw-shard-count": plan.raw_shard_count(),
            "raw-batch-max-items": plan.raw_batch_max_items(),
            "raw-batch-soft-bytes": plan.raw_batch_soft_bytes(),
            "raw-message-node-capacity": plan.raw_message_node_capacity(),
            "raw-model-fingerprint": plan.raw_model_fingerprint(),
            "scheduler-local-capacity": plan.scheduler_local_capacity(),
            "scheduler-remote-shard-count": plan.scheduler_remote_shard_count(),
            "scheduler-batch-max-items": plan.scheduler_batch_max_items(),
            "scheduler-service-interval": plan.scheduler_service_interval(),
            "scheduler-service-batch": plan.scheduler_service_batch(),
            "scheduler-contract-fingerprint": plan.scheduler_contract_fingerprint(),
            "scheduler-runtime": plan.scheduler_runtime(),
            "wait-inline-select-cases": plan.wait_inline_select_cases(),
            "wait-scratch-class-count": plan.wait_scratch_class_count(),
            "wait-node-class-count": plan.wait_node_class_count(),
            "wait-contract-fingerprint": plan.wait_contract_fingerprint(),
            "wait-demand": plan.wait_demand(),
            "wait-runtime": plan.wait_runtime(),
            "sync-contract-fingerprint": plan.sync_contract_fingerprint(),
            "sync-demand": plan.sync_demand(),
            "sync-primitive-count": plan.sync_primitive_count(),
            "sync-runtime": plan.sync_runtime(),
            "raw-resource-cell-header-bytes": plan.resource_cell_header_bytes(),
            "raw-resource-class-count": plan.resource_class_count(),
            "raw-resource-kind-count": plan.resource_kind_count(),
            "raw-release-descriptor-count": plan.release_descriptor_count(),
            "raw-resource-sites": plan.resource_sites(),
            "raw-release-sites": plan.release_sites(),
            "platform-profile": plan.platform_profile(),
            "platform-op-count": plan.platform_op_count(),
            "platform-range-class-count": plan.platform_range_class_count(),
            "platform-contract-fingerprint": plan.platform_contract_fingerprint(),
            "platform-range-demand": json!({
                "payload-extents": plan.platform_range_demand().payload_extents,
                "stack-extents": plan.platform_range_demand().stack_extents,
                "metadata-extents": plan.platform_range_demand().metadata_extents,
                "guard-extents": plan.platform_range_demand().guard_extents,
                "owners": plan.platform_range_demand().owners,
            }),
            "ledger-category-count": plan.ledger_category_count(),
            "rt0-step-count": plan.rt0_step_count(),
            "rt0-lifecycle-count": plan.rt0_lifecycle_count(),
            "startup-config-var-count": plan.startup_config_var_count(),
            "startup-fatal-count": plan.startup_fatal_count(),
            "report-reason-count": plan.report_reason_count(),
            "rt0-emergency-buffer-bytes": plan.rt0_emergency_buffer_bytes(),
            "rt0-contract-fingerprint": plan.rt0_contract_fingerprint(),
            "coroutine-runtime": plan.coroutine_runtime(),
            "coroutine-contract-fingerprint": plan.coroutine_runtime().fingerprint(),
            "rt0-demand": json!({
                "entry-present": plan.rt0_entry_present(),
                "main-returns-result": plan.rt0_main_returns_result(),
            }),
            "rt0": plan.rt0().to_string()
        })
    });
    emit_event(
        "build-finish",
        json!({
            "command": command,
            "target": target.to_string(),
            "success": compilation.is_success(),
            "check-only": check_only,
            "image-plan": image_plan
        }),
    );
}

pub(crate) fn emit_diagnostic(diagnostic: &gugu_compiler::Diagnostic, format: OutputFormat) {
    let span = diagnostic.span();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let payload = json!({
        "file": span.map(|span| sanitize_path(span.path(), &cwd)),
        "line": span.map_or(0, |span| span.line()),
        "column": span.map_or(0, |span| span.column()),
        "severity": diagnostic.severity().to_string(),
        "code": diagnostic.code().to_string(),
        "message": sanitize_message(diagnostic.message(), span.map(|span| span.path()), &cwd),
        "suggestion": Value::Null
    });
    if format == OutputFormat::Json || format == OutputFormat::JsonDiagnosticShort {
        emit_event("compiler-diagnostic", payload);
    }
}

pub(crate) fn emit_cli_error(format: OutputFormat, message: &str) {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let message = sanitize_message(message, None, &cwd);
    match format {
        OutputFormat::Text => eprintln!("error: {message}"),
        OutputFormat::Json | OutputFormat::JsonDiagnosticShort => {
            emit_event(
                "cli-error",
                json!({ "message": message, "suggestion": Value::Null }),
            );
        }
    }
}

pub(crate) fn sanitize_path(path: &Path, cwd: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(cwd) {
        let text = relative.to_string_lossy();
        if !text.is_empty() {
            return text.replace('\\', "/");
        }
    }
    if !path.is_absolute() {
        return path.to_string_lossy().replace('\\', "/");
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                if let Some(text) = value.to_str() {
                    components.push(text);
                }
            }
            Component::ParentDir => {
                if components.pop().is_none() {
                    return external_path(path);
                }
            }
            Component::CurDir | Component::Prefix(_) | Component::RootDir => {}
        }
    }
    if components.is_empty() {
        "<source>".to_owned()
    } else {
        components.join("/")
    }
}

fn external_path(path: &Path) -> String {
    path.file_name()
        .map(|name| format!("<external>/{}", name.to_string_lossy()))
        .unwrap_or_else(|| "<external>".to_owned())
}

pub(crate) fn sanitize_message(message: &str, source_path: Option<&Path>, cwd: &Path) -> String {
    let mut clean = message.to_owned();
    if let Some(path) = source_path {
        let displayed_path = path.display().to_string();
        clean = clean.replace(&displayed_path, &sanitize_path(path, cwd));
    }
    let displayed_cwd = cwd.display().to_string();
    clean = clean.replace(&displayed_cwd, ".");
    redact_absolute_path_tokens(&redact_secrets(&clean))
}

fn redact_absolute_path_tokens(message: &str) -> String {
    message
        .split_whitespace()
        .map(|token| {
            let trimmed = token.trim_matches(|character: char| "`'\"(),;".contains(character));
            if trimmed.starts_with('/') || is_windows_absolute(trimmed) {
                "<path>".to_owned()
            } else {
                token.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_windows_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

pub(crate) fn redact_secrets(message: &str) -> String {
    const SECRET_KEYS: &[&str] = &[
        "access_token",
        "refresh_token",
        "authorization",
        "api_key",
        "apikey",
        "password",
        "secret",
        "token",
    ];
    let lowercase = message.to_ascii_lowercase();
    let bytes = lowercase.as_bytes();
    let mut ranges = Vec::new();
    for key in SECRET_KEYS {
        let mut search_start = 0;
        while let Some(relative) = lowercase[search_start..].find(key) {
            let start = search_start + relative;
            let end_key = start + key.len();
            let boundary_before =
                start == 0 || !bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_';
            let boundary_after = end_key == bytes.len()
                || !bytes[end_key].is_ascii_alphanumeric() && bytes[end_key] != b'_';
            if boundary_before && boundary_after {
                let mut cursor = end_key;
                while bytes.get(cursor) == Some(&b' ') || bytes.get(cursor) == Some(&b'\t') {
                    cursor += 1;
                }
                if matches!(bytes.get(cursor), Some(b'=') | Some(b':')) {
                    cursor += 1;
                    while bytes.get(cursor) == Some(&b' ') || bytes.get(cursor) == Some(&b'\t') {
                        cursor += 1;
                    }
                    if cursor < bytes.len() {
                        let value_start = cursor;
                        if bytes[cursor] == b'\'' || bytes[cursor] == b'\"' {
                            let quote = bytes[cursor];
                            cursor += 1;
                            while cursor < bytes.len() && bytes[cursor] != quote {
                                cursor += 1;
                            }
                            if cursor < bytes.len() {
                                cursor += 1;
                            }
                        } else {
                            while cursor < bytes.len()
                                && !bytes[cursor].is_ascii_whitespace()
                                && !matches!(bytes[cursor], b',' | b';' | b'}')
                            {
                                cursor += 1;
                            }
                        }
                        if value_start < cursor {
                            ranges.push((value_start, cursor));
                        }
                    }
                }
            }
            search_start = end_key;
        }
    }
    ranges.sort_unstable_by_key(|(start, _)| *start);
    ranges.dedup();
    let mut redacted = message.to_owned();
    for (start, end) in ranges.into_iter().rev() {
        redacted.replace_range(start..end, "<redacted>");
    }
    redacted
}
