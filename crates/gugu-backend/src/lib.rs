//! Gugu x86_64 后端：消费已验证 LIR world 与目标描述，完成 lowering、寄存器分配、编码、
//! 元数据 section 与 ELF/PE 镜像写出。
//!
//! 本 crate 只依赖 `gugu-base`、`gugu-runtime-abi`、`gugu-gir` 与 `gugu-lir`；需要完整
//! `Compiler` 的验收 harness 与回归留在上层 driver。
#![forbid(unsafe_code)]

pub mod a64;
pub mod macho;
mod metadata;
mod native_link;
pub mod regalloc;
pub mod x64;
pub use native_link::{NativeImport, NativeImportKind, NativeLink};

use gugu_base::{
    canonical, diagnostics, diagnostics::Diagnostic, hash, query, source, source::SourceMap,
    target, target::TargetName,
};
use gugu_gir::{gir, gir::GirWorldV1, hir, hir::Validated, late, mono::MonoWorldV1, semantics};
use gugu_lir as lir;
use gugu_runtime_abi as runtime;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendPlan {
    pub target: TargetName,
    pub entry: String,
    pub function_count: u32,
    pub semantic_fingerprint: [u8; 32],
    pub runtime_checks_elided_count: u32,
    pub mono_instance_count: u32,
    pub mono_root_count: u32,
    pub mono_graph_fingerprint: [u8; 32],
    pub type_id_count: u32,
    pub type_universe_fingerprint: [u8; 32],
    pub late_constant_count: u32,
    pub late_constants_fingerprint: [u8; 32],
    pub gir_body_count: u32,
    pub gir_block_count: u32,
    pub gir_statement_count: u32,
    pub gir_fingerprint: [u8; 32],
    pub placement_count: u32,
    pub turn_region_count: u32,
    pub local_heap_count: u32,
    pub shared_heap_count: u32,
    pub placement_fingerprint: [u8; 32],
    pub lir_body_count: u32,
    pub lir_block_count: u32,
    pub lir_instruction_count: u32,
    pub lir_memory_operation_count: u32,
    pub lir_safepoint_count: u32,
    pub lir_fingerprint: [u8; 32],
    pub optimization_revision: u32,
    pub poll_budget: u32,
    pub poll_count: u32,
    pub poll_free_leaf_count: u32,
    pub poll_summary_fingerprint: [u8; 32],
    pub raw_size_class_count: u32,
    pub raw_shard_count: u32,
    pub raw_batch_max_items: u32,
    pub raw_batch_soft_bytes: u64,
    pub raw_message_node_capacity: u32,
    pub raw_model_fingerprint: [u8; 32],
    pub coroutine_runtime: crate::runtime::CoroutineRuntimeContract,
    pub scheduler_local_capacity: u32,
    pub scheduler_remote_shard_count: u32,
    pub scheduler_batch_max_items: u32,
    pub scheduler_service_interval: u32,
    pub scheduler_service_batch: u32,
    pub scheduler_contract_fingerprint: [u8; 32],
    pub scheduler_runtime: crate::runtime::SchedulerRuntimeContract,
    pub foreign_max_blocking_workers: u32,
    pub foreign_ordinary_sites: u32,
    pub foreign_dirty_sites: u32,
    pub foreign_leaf_sites: u32,
    pub blocking_io_sites: u32,
    pub foreign_contract_fingerprint: [u8; 32],
    pub foreign_runtime: crate::runtime::ForeignRuntimeContract,
    pub wait_inline_select_cases: u32,
    pub wait_scratch_class_count: u32,
    pub wait_node_class_count: u32,
    pub wait_contract_fingerprint: [u8; 32],
    pub wait_demand: crate::runtime::WaitDemand,
    pub wait_runtime: crate::runtime::WaitRuntimeContract,
    pub sync_contract_fingerprint: [u8; 32],
    pub sync_demand: crate::runtime::SyncDemand,
    pub sync_primitive_count: u32,
    pub sync_runtime: crate::runtime::SyncRuntimeContract,
    pub stackmap_function_count: u32,
    pub stackmap_safepoint_count: u32,
    pub stackmap_map_count: u32,
    pub stackmap_root_words: u32,
    pub stackmap_contract_fingerprint: [u8; 32],
    pub stackmap_demand: crate::runtime::StackMapDemand,
    pub gc_metadata_type_count: u32,
    pub gc_metadata_trace_bytes: u32,
    pub gc_metadata_value_bytes: u32,
    pub gc_metadata_vtable_count: u32,
    pub gc_metadata_root_count: u32,
    pub gc_metadata_arena_bytes: u64,
    pub gc_metadata_block_bytes: u32,
    pub gc_metadata_line_bytes: u32,
    pub gc_metadata_contract_fingerprint: [u8; 32],
    pub gc_metadata_demand: crate::runtime::GcMetadataDemand,
    pub gc_type_section: Vec<u8>,
    pub gc_metadata_section: Vec<u8>,
    pub edge_contract_fingerprint: [u8; 32],
    pub edge_demand: crate::runtime::EdgeDemand,
    pub edge_runtime: crate::runtime::EdgeRuntimeContract,
    pub barrier_contract_fingerprint: [u8; 32],
    pub barrier_demand: crate::runtime::BarrierDemand,
    pub barrier_card_granularity_bytes: u32,
    pub barrier_card_mark_buffer_entries: u32,
    pub barrier_card_mark_stamp_entries: u32,
    pub barrier_flush_reason_count: u32,
    pub barrier_card_mark_batch_fields: u32,
    pub barrier_record_count: u32,
    pub barrier_runtime: crate::runtime::BarrierRuntimeContract,
    pub local_heap_contract_fingerprint: [u8; 32],
    pub local_heap_demand: crate::runtime::LocalHeapDemand,
    pub local_heap_runtime: crate::runtime::LocalHeapRuntimeContract,
    pub shared_heap_contract_fingerprint: [u8; 32],
    pub shared_heap_demand: crate::runtime::SharedHeapDemand,
    pub shared_heap_runtime: crate::runtime::SharedHeapRuntimeContract,
    pub block_return_contract_fingerprint: [u8; 32],
    pub block_return_demand: crate::runtime::BlockReturnDemand,
    pub block_return_runtime: crate::runtime::BlockReturnRuntimeContract,
    pub compression_contract_fingerprint: [u8; 32],
    pub compression_demand: crate::runtime::CompressionDemand,
    pub compression_runtime: crate::runtime::CompressionRuntimeContract,
    pub compression_capability: crate::target::PointerCompression,
    pub routing_contract_fingerprint: [u8; 32],
    pub routing_demand: crate::runtime::RoutingDemand,
    pub routing_runtime: crate::runtime::RoutingRuntimeContract,
    pub provenance_contract_fingerprint: [u8; 32],
    pub provenance_demand: crate::runtime::ProvenanceDemand,
    pub provenance_runtime: crate::runtime::ProvenanceRuntimeContract,
    pub combining_contract_fingerprint: [u8; 32],
    pub combining_demand: crate::runtime::CombiningDemand,
    pub combining_runtime: crate::runtime::CombiningRuntimeContract,
    pub mark_contract_fingerprint: [u8; 32],
    pub mark_demand: crate::runtime::MarkDemand,
    pub mark_runtime: crate::runtime::MarkRuntimeContract,
    pub mark_cycle_state_count: u32,
    pub mark_condition_count: u32,
    pub mark_snapshot_participant_count: u32,
    pub mark_credit_pool: u64,
    pub mark_mailbox_consumer_count: u32,
    pub mark_ticket_field_count: u32,
    pub mark_record_count: u32,
    pub pacing_contract_fingerprint: [u8; 32],
    pub pacing_profile: String,
    pub pacing_profile_revision: u32,
    pub pacing_min_growth_budget: u64,
    pub pacing_assist_threshold: u64,
    pub pacing_assist_quantum: u64,
    pub pacing_mark_cost_per_byte: u32,
    pub pacing_gc_cpu_fraction: u32,
    pub pacing_gc_cpu_window_cost: u64,
    pub pacing_remark_cost_budget: u64,
    pub pacing_evacuation_pause_bytes: u64,
    pub pacing_evacuation_pause_roots: u32,
    pub pacing_evacuation_pause_fields: u32,
    pub pacing_pressure_enter_ratio: u32,
    pub pacing_pressure_clear_ratio: u32,
    pub pacing_credit_source_count: u32,
    pub turn_region_sites: u32,
    pub turn_region_publish_sites: u32,
    pub turn_region_reset_sites: u32,
    pub turn_region_promote_sites: u32,
    pub turn_region_transfer_sites: u32,
    pub turn_region_capacity_class_count: u32,
    pub turn_region_object_limit: u32,
    pub turn_region_max_bytes: u64,
    pub turn_region_total_bytes: u64,
    pub turn_region_contract_fingerprint: [u8; 32],
    pub turn_region_demand: crate::runtime::TurnRegionDemand,
    pub turn_region_runtime: crate::runtime::TurnRegionRuntimeContract,
    pub pacing_demand: crate::runtime::GcPacingDemand,
    pub pacing_pressure_poll_bytes: u64,
    pub pacing_owner_drain_items: u32,
    pub pacing_owner_drain_bytes: u64,
    pub pacing_owner_drain_interval_bytes: u64,
    pub pacing_runtime: crate::runtime::GcPacingRuntimeContract,
    pub resource_cell_header_bytes: u32,
    pub resource_class_count: u32,
    pub resource_kind_count: u32,
    pub release_descriptor_count: u32,
    pub resource_sites: u32,
    pub release_sites: u32,
    pub platform_profile: String,
    pub platform_op_count: u32,
    pub platform_range_class_count: u32,
    pub platform_contract_fingerprint: [u8; 32],
    /// 已绑定的 runtime glue 数量。
    pub runtime_glue_bound: u32,
    /// 实际引用、仍走 stub 兜底的 runtime glue 数量。
    pub runtime_glue_stubbed: u32,
    /// runtime glue 绑定表摘要指纹。
    pub runtime_glue_fingerprint: [u8; 32],
    pub platform_demand: runtime::PlatformRangeDemand,
    pub ledger_category_count: u32,
    /// 目标描述符指纹。
    pub target_descriptor_digest: [u8; 32],
    /// 目标页大小。
    pub target_page_size: u32,
    /// 目标 CPU 基线。
    pub target_cpu_baseline: crate::target::CpuBaseline,
    /// 导入策略 revision。
    pub import_policy_revision: u32,
    /// encoder 契约指纹。
    pub x64_encoder_fingerprint: [u8; 32],
    /// form 目录长度。
    pub x64_form_count: u32,
    /// lowering 规则 revision。
    pub x64_lowering_revision: u32,
    /// lowered 站点总数。
    pub x64_site_count: u32,
    /// 站点机器指令总数。
    pub x64_instruction_count: u32,
    /// 片段字节总数。
    pub x64_encoded_bytes: u32,
    /// 重定位总数。
    pub x64_relocation_count: u32,
    /// 冷边站点数。
    pub x64_cold_edge_count: u32,
    /// 压缩引用解码序列字节数。
    pub x64_decode_sequence_bytes: u32,
    /// 片段世界指纹。
    pub x64_fragment_fingerprint: [u8; 32],
    /// 收缩后的 rel8 条数。
    pub x64_rel8_count: u32,
    /// 热块数。
    pub x64_hot_block_count: u32,
    /// 冷块数。
    pub x64_cold_block_count: u32,
    /// 入口 mangled 符号。
    pub x64_entry_symbol: String,
    /// 分配后的最大栈帧字节数。
    pub x64_frame_size_max: u32,
    /// 溢出槽总数。
    pub x64_spill_slot_count: u32,
    /// 溢出区字节数。
    pub x64_spill_bytes: u32,
    /// 保存的 callee-saved GPR 数。
    pub x64_saved_gpr_count: u32,
    /// 溢出重载次数。
    pub x64_reload_count: u32,
    /// 溢出写回次数。
    pub x64_spill_store_count: u32,
    /// 并行拷贝移动总数。
    pub x64_copy_move_count: u32,
    /// 并行拷贝破环次数。
    pub x64_copy_cycle_count: u32,
    /// 峰值活跃 GPR 数。
    pub x64_peak_live_gpr: u32,
    /// 峰值活跃 XMM 数。
    pub x64_peak_live_xmm: u32,
    /// 分配器处理的值总数。
    pub x64_allocated_values: u32,
    /// 协程栈检查偏移。
    pub coroutine_stack_check_offset: u32,
    /// `[r15 + poll_flags]` 偏移。
    pub scheduler_poll_flags_offset: u32,
    /// 栈图 section 名。
    pub x64_stackmap_name: String,
    /// 展开 section 名。
    pub x64_unwind_name: String,
    /// 源码位置 section 名。
    pub x64_source_name: String,
    /// 栈图 section 字节。
    pub x64_stackmap_section: Vec<u8>,
    /// 展开 section 字节。
    pub x64_unwind_section: Vec<u8>,
    /// 源码位置 section 字节。
    pub x64_source_section: Vec<u8>,
    /// 进入栈图表的函数数。
    pub x64_stackmap_functions: u32,
    /// 栈图安全点数。
    pub x64_stackmap_safepoints: u32,
    /// 去重后的 map 数。
    pub x64_stackmap_maps: u32,
    /// 展开表中的函数数。
    pub x64_unwind_functions: u32,
    /// 落地记录数。
    pub x64_landing_count: u32,
    /// 源码位置记录数。
    pub x64_source_records: u32,
    /// 三节内容指纹。
    pub x64_metadata_fingerprint: [u8; 32],
    /// Linux ELF 镜像字节。Windows 为空。
    pub linux_image: Vec<u8>,
    /// `static-pie`、`dynamic-pie`，Windows 为空。
    pub linux_image_kind: String,
    /// ELF `e_entry`。
    pub linux_entry_vaddr: u64,
    /// 运行时相对重定位条数。
    pub linux_relative_relocs: u32,
    /// `PT_LOAD` 数量。
    pub linux_load_segments: u32,
    /// `PT_INTERP` 路径。无动态导入时为空。
    pub linux_interpreter: String,
    /// 镜像字节指纹。
    pub linux_image_fingerprint: [u8; 32],
    /// Windows PE 镜像字节。Linux 为空。
    pub windows_image: Vec<u8>,
    /// `exe`、`cdylib`，Linux 为空。
    pub windows_image_kind: String,
    /// PE `AddressOfEntryPoint`。
    pub windows_entry_rva: u32,
    /// DIR64 重定位条数。
    pub windows_reloc_count: u32,
    /// 导入的 DLL 数量。
    pub windows_import_dlls: u32,
    /// PE 镜像字节指纹。
    pub windows_image_fingerprint: [u8; 32],
    /// 内部 macOS 目标的 Mach-O 镜像。
    pub macos_image: Vec<u8>,
}

/// `plan` 消费的已装配机器世界；按目标架构分派到 x64 片段世界或 a64 整程序产物。
pub enum PlanCodegen<'a> {
    /// x64 片段世界（镜像在 `plan` 内经 `link_world` 链接）。
    X64(&'a x64::codegen::X64World),
    /// a64 整程序装配产物（镜像在 `a64::program::build` 内已写出）。
    A64 {
        /// 装配产物（world + image）。
        program: &'a a64::program::Program,
        /// 本次编译已解析的 runtime glue 绑定表。
        glues: &'a x64::glue::RuntimeGlueBindings,
    },
}

/// 后端计划里与机器世界相关的统计面；x64 字段名保留，a64 侧填语义最近的真实值或 0。
struct CodegenStats {
    runtime_glue_bound: u32,
    runtime_glue_stubbed: u32,
    runtime_glue_fingerprint: [u8; 32],
    encoder_fingerprint: [u8; 32],
    form_count: u32,
    lowering_revision: u32,
    site_count: u32,
    instruction_count: u32,
    encoded_bytes: u32,
    relocation_count: u32,
    cold_edge_count: u32,
    decode_sequence_bytes: u32,
    fragment_fingerprint: [u8; 32],
    rel8_count: u32,
    hot_block_count: u32,
    cold_block_count: u32,
    entry_symbol: String,
    frame_size_max: u32,
    spill_slot_count: u32,
    spill_bytes: u32,
    saved_gpr_count: u32,
    reload_count: u32,
    spill_store_count: u32,
    copy_move_count: u32,
    copy_cycle_count: u32,
    peak_live_gpr: u32,
    peak_live_xmm: u32,
    allocated_values: u32,
    stackmap_name: String,
    unwind_name: String,
    source_name: String,
    stackmap_section: Vec<u8>,
    unwind_section: Vec<u8>,
    source_section: Vec<u8>,
    stackmap_functions: u32,
    stackmap_safepoints: u32,
    stackmap_maps: u32,
    unwind_functions: u32,
    landing_count: u32,
    source_records: u32,
    metadata_fingerprint: [u8; 32],
    linux: x64::elf::LinuxImage,
    windows: x64::pe::PeImage,
    macos_image: Vec<u8>,
}

fn x64_stats(
    target: TargetName,
    x64: &x64::codegen::X64World,
    raw: &crate::runtime::RuntimeRawContractV1,
    native_link: &NativeLink,
) -> Result<CodegenStats, String> {
    native_link.validate_bridge(target, x64)?;
    let (linux, windows, macos_image) = match target.descriptor().object_format {
        target::ObjectFormat::Elf64 => (
            x64::elf::link_world(
                x64,
                &raw.gc_metadata().type_section,
                &raw.gc_metadata().metadata_section,
                native_link,
            )?,
            x64::pe::PeImage::absent(),
            Vec::new(),
        ),
        target::ObjectFormat::Pe32Plus => (
            x64::elf::LinuxImage::absent(),
            x64::pe::link_world(
                x64,
                &raw.gc_metadata().type_section,
                &raw.gc_metadata().metadata_section,
                native_link,
            )?,
            Vec::new(),
        ),
        target::ObjectFormat::MachO64 => (
            x64::elf::LinuxImage::absent(),
            x64::pe::PeImage::absent(),
            x64::macho::link_world(
                x64,
                &raw.gc_metadata().type_section,
                &raw.gc_metadata().metadata_section,
                native_link,
            )?
            .bytes,
        ),
    };
    let (runtime_glue_bound, runtime_glue_stubbed) = x64.runtime_glue_counts();
    Ok(CodegenStats {
        runtime_glue_bound,
        runtime_glue_stubbed,
        runtime_glue_fingerprint: x64.glues().fingerprint(),
        encoder_fingerprint: x64.encoder_fingerprint(),
        form_count: u32::try_from(x64.contract.forms.len()).expect("form 数量适配 u32"),
        lowering_revision: x64.contract.lowering_revision,
        site_count: x64.site_count(),
        instruction_count: x64.instruction_count(),
        encoded_bytes: x64.encoded_bytes(),
        relocation_count: x64.relocation_count(),
        cold_edge_count: x64.cold_edge_count(),
        decode_sequence_bytes: x64.decode_sequence_bytes(),
        fragment_fingerprint: x64.fingerprint(),
        rel8_count: x64.rel8_count(),
        hot_block_count: x64.hot_block_count(),
        cold_block_count: x64.cold_block_count(),
        entry_symbol: x64.entry_symbol().to_owned(),
        frame_size_max: x64.frame_size_max(),
        spill_slot_count: x64.spill_slot_count(),
        spill_bytes: x64.spill_bytes(),
        saved_gpr_count: x64.saved_gpr_count(),
        reload_count: x64.reload_count(),
        spill_store_count: x64.spill_store_count(),
        copy_move_count: x64.copy_move_count(),
        copy_cycle_count: x64.copy_cycle_count(),
        peak_live_gpr: x64.peak_live_gpr(),
        peak_live_xmm: x64.peak_live_xmm(),
        allocated_values: x64.allocated_values(),
        stackmap_name: x64.metadata.stackmap_name.clone(),
        unwind_name: x64.metadata.unwind_name.clone(),
        source_name: x64.metadata.source_name.clone(),
        stackmap_section: x64.metadata.stackmap_section.clone(),
        unwind_section: x64.metadata.unwind_section.clone(),
        source_section: x64.metadata.source_section.clone(),
        stackmap_functions: x64.metadata.function_count,
        stackmap_safepoints: x64.metadata.safepoint_count,
        stackmap_maps: x64.metadata.map_count,
        unwind_functions: x64.metadata.unwind_function_count,
        landing_count: x64.metadata.landing_count,
        source_records: x64.metadata.source_record_count,
        metadata_fingerprint: x64.metadata.fingerprint,
        linux,
        windows,
        macos_image,
    })
}

fn a64_stats(
    target: TargetName,
    program: &a64::program::Program,
    glues: &x64::glue::RuntimeGlueBindings,
) -> CodegenStats {
    let descriptor = target.descriptor();
    let world = &program.world;
    let section_bytes = |at: crate::macho::Location| -> Vec<u8> {
        match &world.sections[at.section].content {
            crate::macho::SectionContent::Data(bytes) => {
                bytes[at.offset as usize..].to_vec()
            }
            _ => Vec::new(),
        }
    };
    let stackmap_section = section_bytes(world.maps);
    let unwind_section = section_bytes(world.unwind);
    let code_bytes = match &world.sections[world.code.section].content {
        crate::macho::SectionContent::Code(bytes) | crate::macho::SectionContent::Data(bytes) => {
            bytes.len() as u32
        }
        crate::macho::SectionContent::ZeroFill(_) => 0,
    };
    let entry_symbol = world
        .symbols
        .iter()
        .find(|(_, location)| **location == world.entry)
        .map(|(name, _)| name.clone())
        .unwrap_or_default();
    let mut metadata_hasher = blake3::Hasher::new_derive_key("gugu-a64-metadata-v1");
    metadata_hasher.update(&stackmap_section);
    metadata_hasher.update(&unwind_section);
    let linux = match descriptor.object_format {
        target::ObjectFormat::Elf64 => {
            // ELF e_entry 是头部固定字段；镜像由 `link_entry_dynamic` 写完，直接读回。
            let entry = program
                .image
                .get(24..32)
                .map(|field| u64::from_le_bytes(field.try_into().expect("8 字节字段")))
                .unwrap_or(0);
            x64::elf::LinuxImage {
                bytes: program.image.clone(),
                kind: "elf".into(),
                entry,
                reloc_count: 0,
                reloc_vaddr: 0,
                load_count: 0,
                interpreter: descriptor.linux_interpreter.unwrap_or("").to_owned(),
                fingerprint: *blake3::hash(&program.image).as_bytes(),
            }
        }
        _ => x64::elf::LinuxImage::absent(),
    };
    let macos_image = match descriptor.object_format {
        target::ObjectFormat::MachO64 => program.image.clone(),
        _ => Vec::new(),
    };
    CodegenStats {
        runtime_glue_bound: glues.bound(),
        // a64 catalog 要求所有被引用 glue 都解析到源定义；未实现项在装配期就是错误。
        runtime_glue_stubbed: 0,
        runtime_glue_fingerprint: glues.fingerprint(),
        encoder_fingerprint: a64::contract::fingerprint(),
        form_count: 0,
        lowering_revision: 0,
        // a64 世界尚无站点/帧/寄存器统计面；指令数按定长 4 字节编码字折算。
        site_count: 0,
        instruction_count: code_bytes / 4,
        encoded_bytes: code_bytes,
        relocation_count: (world.relocations.len() + world.pointers.len()) as u32,
        cold_edge_count: 0,
        decode_sequence_bytes: 0,
        fragment_fingerprint: *blake3::hash(&program.image).as_bytes(),
        rel8_count: 0,
        hot_block_count: 0,
        cold_block_count: 0,
        entry_symbol,
        frame_size_max: 0,
        spill_slot_count: 0,
        spill_bytes: 0,
        saved_gpr_count: 0,
        reload_count: 0,
        spill_store_count: 0,
        copy_move_count: 0,
        copy_cycle_count: 0,
        peak_live_gpr: 0,
        peak_live_xmm: 0,
        allocated_values: 0,
        stackmap_name: "__gugu_maps".into(),
        unwind_name: "__gugu_cfa".into(),
        source_name: String::new(),
        stackmap_section,
        unwind_section,
        source_section: Vec::new(),
        stackmap_functions: 0,
        stackmap_safepoints: 0,
        stackmap_maps: 0,
        unwind_functions: 0,
        landing_count: 0,
        source_records: 0,
        metadata_fingerprint: *metadata_hasher.finalize().as_bytes(),
        linux,
        windows: x64::pe::PeImage::absent(),
        macos_image,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "后端计划需要各层 world、运行时契约与原生链接配置"
)]
pub fn plan(
    target: TargetName,
    hir: &Validated,
    mono: &MonoWorldV1,
    gir: &GirWorldV1,
    lir: &crate::lir::Validated,
    raw: &crate::runtime::RuntimeRawContractV1,
    codegen: PlanCodegen<'_>,
    runtime_checks_elided_count: u32,
    native_link: &NativeLink,
) -> Result<Option<BackendPlan>, String> {
    native_link.validate(target)?;
    native_link.validate_bridge_targets(lir)?;
    let descriptor = target.descriptor();
    let module = hir.module();
    let Some(entry) = module.entry else {
        return Ok(None);
    };
    let stats = match codegen {
        PlanCodegen::X64(x64) => x64_stats(target, x64, raw, native_link)?,
        PlanCodegen::A64 { program, glues } => a64_stats(target, program, glues),
    };
    let placement = gir.placement.counts();
    Ok(Some(BackendPlan {
        target,
        entry: module.definitions[entry.index()].name.clone(),
        function_count: mono.instances.len() as u32,
        semantic_fingerprint: hir.fingerprint(),
        runtime_checks_elided_count,
        mono_instance_count: mono.instances.len() as u32,
        mono_root_count: mono.roots.len() as u32,
        mono_graph_fingerprint: mono.graph_fingerprint,
        type_id_count: mono.universe.records.len() as u32,
        type_universe_fingerprint: mono.universe.fingerprint,
        late_constant_count: mono.late.results.len() as u32,
        late_constants_fingerprint: mono.late.fingerprint,
        gir_body_count: gir.bodies.len() as u32,
        gir_block_count: gir.bodies.iter().map(|body| body.blocks.len() as u32).sum(),
        gir_statement_count: gir
            .bodies
            .iter()
            .map(|body| body.statements.len() as u32)
            .sum(),
        gir_fingerprint: gir.fingerprint,
        placement_count: placement.total,
        turn_region_count: placement.turn_region,
        local_heap_count: placement.local_heap,
        shared_heap_count: placement.shared_heap,
        placement_fingerprint: gir.placement.fingerprint,
        lir_body_count: u32::try_from(lir.body_count()).expect("LIR body 数量适配 u32"),
        lir_block_count: u32::try_from(lir.blocks()).expect("LIR block 数量适配 u32"),
        lir_instruction_count: u32::try_from(lir.instructions()).expect("LIR 指令数量适配 u32"),
        lir_memory_operation_count: u32::try_from(lir.memory_operations())
            .expect("LIR Mem 数量适配 u32"),
        lir_safepoint_count: u32::try_from(lir.safepoints()).expect("LIR safepoint 数量适配 u32"),
        lir_fingerprint: lir.fingerprint(),
        optimization_revision: lir.optimization_revision(),
        poll_budget: lir.poll_budget(),
        poll_count: u32::try_from(lir.poll_count()).expect("poll 数量适配 u32"),
        poll_free_leaf_count: u32::try_from(lir.poll_free_leaf_count())
            .expect("poll-free 叶数量适配 u32"),
        poll_summary_fingerprint: lir.poll_summary_fingerprint(),
        raw_size_class_count: raw.class_count(),
        raw_shard_count: raw.shard_count(),
        raw_batch_max_items: raw.batch_limits().items,
        raw_batch_soft_bytes: raw.batch_limits().batch_soft_bytes,
        raw_message_node_capacity: raw.message_node_capacity(),
        raw_model_fingerprint: raw.fingerprint(),
        coroutine_runtime: raw.coroutine().clone(),
        scheduler_local_capacity: raw.scheduler().local_capacity(),
        scheduler_remote_shard_count: raw.scheduler().remote_shards(),
        scheduler_batch_max_items: raw.scheduler().batch_max(),
        scheduler_service_interval: raw.scheduler().service_interval(),
        scheduler_service_batch: raw.scheduler().service_batch(),
        scheduler_contract_fingerprint: raw.scheduler().fingerprint(),
        scheduler_runtime: raw.scheduler().clone(),
        foreign_max_blocking_workers: raw.foreign().max_blocking_workers,
        foreign_ordinary_sites: raw.foreign().demand.ordinary,
        foreign_dirty_sites: raw.foreign().demand.dirty,
        foreign_leaf_sites: raw.foreign().demand.leaf,
        blocking_io_sites: raw.foreign().demand.blocking_io,
        foreign_contract_fingerprint: raw.foreign().fingerprint(),
        foreign_runtime: raw.foreign().clone(),
        wait_inline_select_cases: raw.wait().inline_select_cases(),
        wait_scratch_class_count: raw.wait().scratch_class_count(),
        wait_node_class_count: raw.wait().wait_node_class_count(),
        wait_contract_fingerprint: raw.wait().fingerprint(),
        wait_demand: raw.wait().demand,
        wait_runtime: raw.wait().clone(),
        sync_contract_fingerprint: raw.sync().fingerprint(),
        sync_demand: raw.sync().demand,
        sync_primitive_count: raw.sync().primitive_count(),
        sync_runtime: raw.sync().clone(),
        stackmap_function_count: raw.stackmap().demand().functions,
        stackmap_safepoint_count: raw.stackmap().demand().safepoints,
        stackmap_map_count: raw.stackmap().demand().maps,
        stackmap_root_words: raw.stackmap().demand().root_words,
        stackmap_contract_fingerprint: raw.stackmap().fingerprint,
        stackmap_demand: raw.stackmap().demand(),
        gc_metadata_type_count: raw.gc_metadata().demand.type_count,
        gc_metadata_trace_bytes: raw.gc_metadata().demand.trace_program_bytes,
        gc_metadata_value_bytes: raw.gc_metadata().demand.value_program_bytes,
        gc_metadata_vtable_count: raw.gc_metadata().demand.vtable_count,
        gc_metadata_root_count: raw.gc_metadata().demand.root_range_count,
        gc_metadata_arena_bytes: raw.gc_metadata().arena_bytes,
        gc_metadata_block_bytes: raw.gc_metadata().block_bytes,
        gc_metadata_line_bytes: raw.gc_metadata().line_bytes,
        gc_metadata_contract_fingerprint: raw.gc_metadata().fingerprint,
        gc_metadata_demand: raw.gc_metadata().demand,
        gc_type_section: raw.gc_metadata().type_section.clone(),
        gc_metadata_section: raw.gc_metadata().metadata_section.clone(),
        edge_contract_fingerprint: raw.edge().fingerprint,
        edge_demand: *raw.edge().demand(),
        edge_runtime: raw.edge().clone(),
        barrier_contract_fingerprint: raw.barrier().fingerprint(),
        barrier_demand: raw.barrier().demand(),
        barrier_card_granularity_bytes: raw.barrier().card_granularity_bytes(),
        barrier_card_mark_buffer_entries: raw.barrier().card_mark_buffer_entries(),
        barrier_card_mark_stamp_entries: raw.barrier().card_mark_stamp_entries(),
        barrier_flush_reason_count: raw.barrier().flush_reason_count(),
        barrier_card_mark_batch_fields: raw.barrier().card_mark_batch_field_count(),
        barrier_record_count: raw.barrier().record_count(),
        barrier_runtime: raw.barrier().clone(),
        local_heap_contract_fingerprint: raw.local_heap().fingerprint,
        local_heap_demand: raw.local_heap().demand(),
        local_heap_runtime: raw.local_heap().clone(),
        shared_heap_contract_fingerprint: raw.shared_heap().fingerprint(),
        shared_heap_demand: raw.shared_heap().demand(),
        shared_heap_runtime: raw.shared_heap().clone(),
        block_return_contract_fingerprint: raw.block_return().fingerprint(),
        block_return_demand: raw.block_return().demand(),
        block_return_runtime: raw.block_return().clone(),
        compression_contract_fingerprint: raw.compression().fingerprint(),
        compression_demand: raw.compression().demand(),
        compression_runtime: raw.compression().clone(),
        compression_capability: raw.compression().capability(),
        routing_contract_fingerprint: raw.routing().fingerprint(),
        routing_demand: raw.routing().demand(),
        routing_runtime: raw.routing().clone(),
        provenance_contract_fingerprint: raw.provenance().fingerprint(),
        provenance_demand: raw.provenance().demand(),
        provenance_runtime: raw.provenance().clone(),
        combining_contract_fingerprint: raw.combining().fingerprint(),
        combining_demand: raw.combining().demand(),
        combining_runtime: raw.combining().clone(),
        mark_contract_fingerprint: raw.mark().fingerprint(),
        mark_demand: raw.mark().demand(),
        mark_runtime: raw.mark().clone(),
        mark_cycle_state_count: raw.mark().cycle_states.len() as u32,
        mark_condition_count: raw.mark().condition_count(),
        mark_snapshot_participant_count: raw.mark().snapshot_participant_count(),
        mark_credit_pool: raw.mark().credit_pool(),
        mark_mailbox_consumer_count: raw.mark().mailbox_consumers(),
        mark_ticket_field_count: raw.mark().ticket_field_count(),
        mark_record_count: raw.mark().record_count(),
        pacing_contract_fingerprint: raw.pacing().fingerprint(),
        pacing_profile: raw.pacing().profile().to_owned(),
        pacing_profile_revision: raw.pacing().profile_revision(),
        pacing_min_growth_budget: raw.pacing().min_growth_budget(),
        pacing_assist_threshold: raw.pacing().assist_threshold(),
        pacing_assist_quantum: raw.pacing().assist_quantum(),
        pacing_mark_cost_per_byte: raw.pacing().mark_cost_per_byte(),
        pacing_gc_cpu_fraction: raw.pacing().gc_cpu_fraction(),
        pacing_gc_cpu_window_cost: raw.pacing().gc_cpu_window_cost(),
        pacing_remark_cost_budget: raw.pacing().remark_cost_budget(),
        pacing_evacuation_pause_bytes: raw.pacing().evacuation_pause_bytes(),
        pacing_evacuation_pause_roots: raw.pacing().evacuation_pause_roots(),
        pacing_evacuation_pause_fields: raw.pacing().evacuation_pause_fields(),
        pacing_pressure_enter_ratio: raw.pacing().pressure_enter_ratio(),
        pacing_pressure_clear_ratio: raw.pacing().pressure_clear_ratio(),
        pacing_credit_source_count: raw.pacing().credit_sources.len() as u32,
        pacing_pressure_poll_bytes: raw.pacing().pressure_poll_bytes(),
        pacing_owner_drain_items: raw.pacing().owner_drain_items(),
        pacing_owner_drain_bytes: raw.pacing().owner_drain_bytes(),
        pacing_owner_drain_interval_bytes: raw.pacing().owner_drain_interval_bytes(),
        pacing_demand: raw.pacing().demand(),
        pacing_runtime: raw.pacing().clone(),
        turn_region_sites: raw.region().demand().regions,
        turn_region_publish_sites: raw.region().demand().publish_sites,
        turn_region_reset_sites: raw.region().demand().reset_sites,
        turn_region_promote_sites: raw.region().demand().promote_sites,
        turn_region_transfer_sites: raw.region().demand().transfer_sites,
        turn_region_capacity_class_count: raw.region().capacity_class_count(),
        turn_region_object_limit: raw.region().object_limit(),
        turn_region_max_bytes: raw.region().demand().max_region_bytes,
        turn_region_total_bytes: raw.region().demand().total_bytes,
        turn_region_contract_fingerprint: raw.region().fingerprint(),
        turn_region_demand: raw.region().demand(),
        turn_region_runtime: raw.region().clone(),
        resource_cell_header_bytes: raw.resources().header.header_bytes,
        resource_class_count: raw.resource_class_count(),
        resource_kind_count: raw.resource_kind_count(),
        release_descriptor_count: raw.resources().release.fields.len() as u32,
        resource_sites: raw.resource_demand().resource_sites,
        release_sites: raw.resource_demand().release_sites,
        platform_profile: raw.platform().profile().to_owned(),
        platform_op_count: raw.platform().op_count(),
        platform_range_class_count: raw.platform().class_count(),
        platform_contract_fingerprint: *blake3::Hasher::new_derive_key("gugu-platform-range-v1")
            .update(&raw.platform().canonical_bytes())
            .finalize()
            .as_bytes(),
        platform_demand: *raw.platform_demand(),
        runtime_glue_bound: stats.runtime_glue_bound,
        runtime_glue_stubbed: stats.runtime_glue_stubbed,
        runtime_glue_fingerprint: stats.runtime_glue_fingerprint,
        ledger_category_count: raw.ledger_categories().len() as u32,
        target_descriptor_digest: descriptor.digest(),
        target_page_size: descriptor.page_size,
        target_cpu_baseline: descriptor.cpu_baseline,
        import_policy_revision: crate::target::IMPORT_POLICY_REVISION,
        x64_encoder_fingerprint: stats.encoder_fingerprint,
        x64_form_count: stats.form_count,
        x64_lowering_revision: stats.lowering_revision,
        x64_site_count: stats.site_count,
        x64_instruction_count: stats.instruction_count,
        x64_encoded_bytes: stats.encoded_bytes,
        x64_relocation_count: stats.relocation_count,
        x64_cold_edge_count: stats.cold_edge_count,
        x64_decode_sequence_bytes: stats.decode_sequence_bytes,
        x64_fragment_fingerprint: stats.fragment_fingerprint,
        x64_rel8_count: stats.rel8_count,
        x64_hot_block_count: stats.hot_block_count,
        x64_cold_block_count: stats.cold_block_count,
        x64_entry_symbol: stats.entry_symbol,
        x64_frame_size_max: stats.frame_size_max,
        x64_spill_slot_count: stats.spill_slot_count,
        x64_spill_bytes: stats.spill_bytes,
        x64_saved_gpr_count: stats.saved_gpr_count,
        x64_reload_count: stats.reload_count,
        x64_spill_store_count: stats.spill_store_count,
        x64_copy_move_count: stats.copy_move_count,
        x64_copy_cycle_count: stats.copy_cycle_count,
        x64_peak_live_gpr: stats.peak_live_gpr,
        x64_peak_live_xmm: stats.peak_live_xmm,
        x64_allocated_values: stats.allocated_values,
        coroutine_stack_check_offset: raw.coroutine().stack_check_offset,
        scheduler_poll_flags_offset: raw.scheduler().poll_flags_offset(),
        x64_stackmap_name: stats.stackmap_name,
        x64_unwind_name: stats.unwind_name,
        x64_source_name: stats.source_name,
        x64_stackmap_section: stats.stackmap_section,
        x64_unwind_section: stats.unwind_section,
        x64_source_section: stats.source_section,
        x64_stackmap_functions: stats.stackmap_functions,
        x64_stackmap_safepoints: stats.stackmap_safepoints,
        x64_stackmap_maps: stats.stackmap_maps,
        x64_unwind_functions: stats.unwind_functions,
        x64_landing_count: stats.landing_count,
        x64_source_records: stats.source_records,
        x64_metadata_fingerprint: stats.metadata_fingerprint,
        linux_image: stats.linux.bytes,
        linux_image_kind: stats.linux.kind,
        linux_entry_vaddr: stats.linux.entry,
        linux_relative_relocs: stats.linux.reloc_count,
        linux_load_segments: stats.linux.load_count,
        linux_interpreter: stats.linux.interpreter,
        linux_image_fingerprint: stats.linux.fingerprint,
        windows_image: stats.windows.bytes,
        windows_image_kind: stats.windows.kind,
        windows_entry_rva: stats.windows.entry_rva,
        windows_reloc_count: stats.windows.reloc_count,
        windows_import_dlls: stats.windows.import_dlls,
        windows_image_fingerprint: stats.windows.fingerprint,
        macos_image: stats.macos_image,
    }))
}
