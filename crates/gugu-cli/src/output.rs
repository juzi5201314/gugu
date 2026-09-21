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

fn wants_dump_x64(options: &GlobalArgs) -> bool {
    options.z.iter().any(|flag| flag == "dump-x64")
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
    if wants_dump_x64(options)
        && let Some(dump) = compilation.dump_x64()
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
    let color = should_color(options.color.unwrap_or_default());
    let blocks = compilation
        .diagnostics()
        .items()
        .iter()
        .map(|diagnostic| diagnostic.render_human(compilation.source_map(), color))
        .collect::<Vec<_>>();
    if !blocks.is_empty() {
        eprintln!("{}", blocks.join("\n\n"));
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
        println!(
            "x64: {} sites, {} bytes, {} relocations",
            plan.x64_site_count(),
            plan.x64_encoded_bytes(),
            plan.x64_relocation_count()
        );
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
    if wants_dump_x64(options)
        && let Some(dump) = compilation.dump_x64()
    {
        emit_event(
            "x64-dump",
            json!({
                "text": dump,
                "fingerprint": compilation.x64_fingerprint(),
            }),
        );
    }
    let image_plan = compilation.image_plan().map(image_plan_payload);
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

/// image-plan 的 JSON 视图；与 `-Zdump-runtime` 的文本视图保持同一字段口径。
pub(crate) fn image_plan_payload(plan: &gugu_compiler::ImagePlan) -> Value {
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
        "foreign-max-blocking-workers": plan.foreign_max_blocking_workers(),
        "foreign-ordinary-sites": plan.foreign_ordinary_sites(),
        "foreign-dirty-sites": plan.foreign_dirty_sites(),
        "foreign-leaf-sites": plan.foreign_leaf_sites(),
        "foreign-contract-fingerprint": plan.foreign_contract_fingerprint(),
        "foreign-runtime": plan.foreign_runtime(),
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
        "stackmap-function-count": plan.stackmap_function_count(),
        "stackmap-safepoint-count": plan.stackmap_safepoint_count(),
        "stackmap-map-count": plan.stackmap_map_count(),
        "stackmap-root-words": plan.stackmap_root_words(),
        "stackmap-contract-fingerprint": plan.stackmap_contract_fingerprint(),
        "stackmap-demand": plan.stackmap_demand(),
        "gc-metadata-type-count": plan.gc_metadata_type_count(),
        "gc-metadata-trace-bytes": plan.gc_metadata_trace_bytes(),
        "gc-metadata-value-bytes": plan.gc_metadata_value_bytes(),
        "gc-metadata-vtable-count": plan.gc_metadata_vtable_count(),
        "gc-metadata-root-count": plan.gc_metadata_root_count(),
        "gc-metadata-arena-bytes": plan.gc_metadata_arena_bytes(),
        "gc-metadata-block-bytes": plan.gc_metadata_block_bytes(),
        "gc-metadata-line-bytes": plan.gc_metadata_line_bytes(),
        "gc-metadata-contract-fingerprint": plan.gc_metadata_contract_fingerprint(),
        "gc-metadata-demand": plan.gc_metadata_demand(),
        "gc-type-section-bytes": plan.gc_type_section_bytes(),
        "gc-metadata-section-bytes": plan.gc_metadata_section_bytes(),
        "gc-type-section-fingerprint": plan.gc_type_section_fingerprint(),
        "gc-metadata-section-fingerprint": plan.gc_metadata_section_fingerprint(),
        "local-heap-contract-fingerprint": plan.local_heap_contract_fingerprint(),
        "local-heap-runtime": plan.local_heap_runtime(),
        "local-heap-trigger": plan.local_heap_trigger(),
        "local-heap-demand": plan.local_heap_demand(),
        "shared-heap-contract-fingerprint": plan.shared_heap_contract_fingerprint(),
        "shared-heap-demand": plan.shared_heap_demand(),
        "shared-heap-profile": plan.shared_heap_runtime().profile(),
        "shared-heap-profile-revision": plan.shared_heap_runtime().profile_revision(),
        "shared-heap-handle-tag": plan.shared_heap_runtime().handle_tag(),
        "shared-heap-slot-bytes": plan.shared_heap_runtime().handle_slot_bytes(),
        "shared-heap-payload-record-bytes": plan.shared_heap_runtime().payload_record_bytes(),
        "shared-heap-forwarding-grace-steps": plan.shared_heap_runtime().forwarding_grace_steps(),
        "shared-heap-state-count": plan.shared_heap_runtime().state_count(),
        "shared-heap-transition-count": plan.shared_heap_runtime().transition_count(),
        "shared-heap-forward-fields": plan.shared_heap_runtime().handle_forward_field_count(),
        "shared-heap-records": plan.shared_heap_runtime().record_count(),
        "block-return-contract-fingerprint": plan.block_return_contract_fingerprint(),
        "block-return-demand": plan.block_return_demand(),
        "block-return-profile": plan.block_return_runtime().profile(),
        "block-return-profile-revision": plan.block_return_runtime().profile_revision(),
        "block-return-unit-count": plan.block_return_runtime().unit_count(),
        "block-return-gate-count": plan.block_return_runtime().gate_count(),
        "block-return-grace-steps": plan.block_return_runtime().grace_steps(),
        "compression-contract-fingerprint": plan.compression_contract_fingerprint(),
        "compression-demand": plan.compression_demand(),
        "compression-profile": plan.compression_runtime().profile(),
        "compression-profile-revision": plan.compression_runtime().profile_revision(),
        "compression-enabled": plan.compression_runtime().enabled(),
        "compression-cage-bytes": plan.compression_runtime().cage_bytes(),
        "compression-cage-granule-bytes": plan.compression_runtime().cage_granule_bytes(),
        "compression-max-cage-bytes": plan.compression_runtime().max_cage_bytes(),
        "compression-canonical-bits": plan.compression_runtime().canonical_bits(),
        "compression-capability-supported": plan.compression_capability().supported,
        "routing-contract-fingerprint": plan.routing_contract_fingerprint(),
        "routing-demand": plan.routing_demand(),
        "routing-runtime": plan.routing_runtime(),
        "routing-profile": plan.routing_runtime().profile(),
        "routing-profile-revision": plan.routing_runtime().profile_revision(),
        "routing-mode": plan.routing_runtime().mode().name(),
        "routing-bucket-count": plan.routing_runtime().bucket_count(),
        "routing-max-levels": plan.routing_runtime().max_levels(),
        "routing-hop-limit": plan.routing_runtime().hop_limit(),
        "provenance-contract-fingerprint": plan.provenance_contract_fingerprint(),
        "provenance-demand": plan.provenance_demand(),
        "provenance-runtime": plan.provenance_runtime(),
        "provenance-profile": plan.provenance_runtime().profile(),
        "provenance-profile-revision": plan.provenance_runtime().profile_revision(),
        "provenance-mode": plan.provenance_runtime().mode().name(),
        "provenance-check-count": plan.provenance_runtime().check_count(),
        "provenance-domain-count": plan.provenance_runtime().domain_count(),
        "provenance-rejection-category-count": plan.provenance_runtime().rejection_category_count(),
        "combining-contract-fingerprint": plan.combining_contract_fingerprint(),
        "combining-demand": plan.combining_demand(),
        "combining-runtime": plan.combining_runtime(),
        "combining-profile": plan.combining_runtime().profile(),
        "combining-profile-revision": plan.combining_runtime().profile_revision(),
        "combining-mode": plan.combining_runtime().mode().name(),
        "combining-operation-tag-count": plan.combining_runtime().operation_tag_count(),
        "combining-merge-limit": plan.combining_runtime().merge_limit(),
        "combining-round-item-budget": plan.combining_runtime().round_item_budget(),
        "combining-round-byte-budget": plan.combining_runtime().round_byte_budget(),
        "mark-contract-fingerprint": plan.mark_contract_fingerprint(),
        "mark-runtime": plan.mark_runtime(),
        "mark-cycle-states": plan.mark_cycle_state_count(),
        "mark-conditions": plan.mark_condition_count(),
        "mark-snapshot-participants": plan.mark_snapshot_participant_count(),
        "mark-credit-pool": plan.mark_credit_pool(),
        "mark-mailbox-consumers": plan.mark_mailbox_consumer_count(),
        "mark-ticket-fields": plan.mark_ticket_field_count(),
        "mark-records": plan.mark_record_count(),
        "mark-demand": plan.mark_demand(),
        "barrier-card-granularity-bytes": plan.barrier_card_granularity_bytes(),
        "barrier-card-mark-buffer-entries": plan.barrier_card_mark_buffer_entries(),
        "barrier-card-mark-stamp-entries": plan.barrier_card_mark_stamp_entries(),
        "barrier-flush-reason-count": plan.barrier_flush_reason_count(),
        "barrier-card-mark-batch-fields": plan.barrier_card_mark_batch_fields(),
        "barrier-record-count": plan.barrier_record_count(),
        "barrier-contract-fingerprint": plan.barrier_contract_fingerprint(),
        "barrier-demand": plan.barrier_demand(),
        "edge-contract-fingerprint": plan.edge_contract_fingerprint(),
        "edge-demand": plan.edge_demand(),
        "edge-runtime": plan.edge_runtime(),
        "edge-candidate-quantum": plan.edge_runtime().candidate_quantum,
        "edge-candidate-schema": plan.edge_runtime().candidate_schema,
        "edge-phase-count": plan.edge_runtime().phases.len(),
        "edge-block-state-count": plan.edge_runtime().states.len(),
        "edge-delta-field-count": plan.edge_runtime().edge_delta_field_count(),
        "pacing-contract-fingerprint": plan.pacing_contract_fingerprint(),
        "pacing-profile": plan.pacing_profile(),
        "pacing-profile-revision": plan.pacing_profile_revision(),
        "pacing-min-growth-budget": plan.pacing_min_growth_budget(),
        "pacing-assist-threshold": plan.pacing_assist_threshold(),
        "pacing-assist-quantum": plan.pacing_assist_quantum(),
        "pacing-mark-cost-per-byte": plan.pacing_mark_cost_per_byte(),
        "pacing-gc-cpu-fraction": plan.pacing_gc_cpu_fraction(),
        "pacing-gc-cpu-window-cost": plan.pacing_gc_cpu_window_cost(),
        "pacing-remark-cost-budget": plan.pacing_remark_cost_budget(),
        "pacing-evacuation-pause-bytes": plan.pacing_evacuation_pause_bytes(),
        "pacing-evacuation-pause-roots": plan.pacing_evacuation_pause_roots(),
        "pacing-evacuation-pause-fields": plan.pacing_evacuation_pause_fields(),
        "pacing-pressure-enter-ratio": plan.pacing_pressure_enter_ratio(),
        "pacing-pressure-clear-ratio": plan.pacing_pressure_clear_ratio(),
        "pacing-credit-source-count": plan.pacing_credit_source_count(),
        "pacing-pressure-poll-bytes": plan.pacing_pressure_poll_bytes(),
        "pacing-owner-drain-items": plan.pacing_owner_drain_items(),
        "pacing-owner-drain-bytes": plan.pacing_owner_drain_bytes(),
        "pacing-owner-drain-interval-bytes": plan.pacing_owner_drain_interval_bytes(),
        "turn-region-sites": plan.turn_region_sites(),
        "turn-region-publish-sites": plan.turn_region_publish_sites(),
        "turn-region-reset-sites": plan.turn_region_reset_sites(),
        "turn-region-promote-sites": plan.turn_region_promote_sites(),
        "turn-region-transfer-sites": plan.turn_region_transfer_sites(),
        "turn-region-capacity-class-count": plan.turn_region_capacity_class_count(),
        "turn-region-object-limit": plan.turn_region_object_limit(),
        "turn-region-max-bytes": plan.turn_region_max_bytes(),
        "turn-region-total-bytes": plan.turn_region_total_bytes(),
        "turn-region-contract-fingerprint": plan.turn_region_contract_fingerprint(),
        "turn-region-demand": plan.turn_region_demand(),
        "pacing-demand": plan.pacing_demand(),
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
        "rt0": plan.rt0().to_string(),
        "target-descriptor-digest": plan.target_descriptor_digest(),
        "target-page-size": plan.target_page_size(),
        "target-cpu-baseline": plan.target_cpu_baseline().name(),
        "import-policy-revision": plan.import_policy_revision(),
        "x64-encoder-fingerprint": plan.x64_encoder_fingerprint(),
        "x64-form-count": plan.x64_form_count(),
        "x64-lowering-revision": plan.x64_lowering_revision(),
        "x64-site-count": plan.x64_site_count(),
        "x64-instruction-count": plan.x64_instruction_count(),
        "x64-encoded-bytes": plan.x64_encoded_bytes(),
        "x64-relocation-count": plan.x64_relocation_count(),
        "x64-cold-edge-count": plan.x64_cold_edge_count(),
        "x64-decode-sequence-bytes": plan.x64_decode_sequence_bytes(),
        "x64-fragment-fingerprint": plan.x64_fragment_fingerprint(),
        "x64-rel8-count": plan.x64_rel8_count(),
        "x64-hot-block-count": plan.x64_hot_block_count(),
        "x64-cold-block-count": plan.x64_cold_block_count(),
        "x64-entry-symbol": plan.x64_entry_symbol(),
        "x64-frame-size-max": plan.x64_frame_size_max(),
        "x64-spill-slot-count": plan.x64_spill_slot_count(),
        "x64-spill-bytes": plan.x64_spill_bytes(),
        "x64-saved-gpr-count": plan.x64_saved_gpr_count(),
        "x64-reload-count": plan.x64_reload_count(),
        "x64-spill-store-count": plan.x64_spill_store_count(),
        "x64-copy-move-count": plan.x64_copy_move_count(),
        "x64-copy-cycle-count": plan.x64_copy_cycle_count(),
        "x64-peak-live-gpr": plan.x64_peak_live_gpr(),
        "x64-peak-live-xmm": plan.x64_peak_live_xmm(),
        "x64-allocated-values": plan.x64_allocated_values(),
        "x64-stackmap-section": plan.x64_stackmap_name(),
        "x64-unwind-section": plan.x64_unwind_name(),
        "x64-source-section": plan.x64_source_name(),
        "x64-stackmap-bytes": plan.x64_stackmap_section().len(),
        "x64-unwind-bytes": plan.x64_unwind_section().len(),
        "x64-source-bytes": plan.x64_source_section().len(),
        "x64-stackmap-functions": plan.x64_stackmap_functions(),
        "x64-stackmap-safepoints": plan.x64_stackmap_safepoints(),
        "x64-stackmap-maps": plan.x64_stackmap_maps(),
        "x64-unwind-functions": plan.x64_unwind_functions(),
        "x64-landing-count": plan.x64_landing_count(),
        "x64-source-records": plan.x64_source_records(),
        "x64-metadata-fingerprint": plan.x64_metadata_fingerprint(),
        "linux-image-kind": plan.linux_image_kind(),
        "linux-image-bytes": plan.linux_image().len(),
        "linux-entry-vaddr": plan.linux_entry_vaddr(),
        "linux-relative-relocs": plan.linux_relative_relocs(),
        "linux-load-segments": plan.linux_load_segments(),
        "linux-interpreter": plan.linux_interpreter(),
        "linux-image-fingerprint": plan.linux_image_fingerprint(),
        "windows-image-kind": plan.windows_image_kind(),
        "windows-image-bytes": plan.windows_image().len(),
        "windows-entry-rva": plan.windows_entry_rva(),
        "windows-reloc-count": plan.windows_reloc_count(),
        "windows-import-dlls": plan.windows_import_dlls(),
        "windows-image-fingerprint": plan.windows_image_fingerprint(),
        "coroutine-stack-check-offset": plan.coroutine_stack_check_offset(),
        "scheduler-poll-flags-offset": plan.scheduler_poll_flags_offset(),
    })
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
