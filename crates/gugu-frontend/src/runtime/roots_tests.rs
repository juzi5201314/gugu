//! 镜像内根 walker 的契约回归：Gugu 常量、section 偏移与状态区槽位必须与 compiler 同源。
//!
//! 只解析进程内嵌的源文本与 Rust 常量，不读镜像、不启动子进程。

use super::coroutine::CoroutineHot;
use super::entry::{Rt0Payload, state};
use super::stackmap_codec::{FUNCTION_BYTES, HEADER_BYTES, SAFEPOINT_BYTES, TYPED_ROOT_BYTES};
use super::stackmap_schema::{
    REGISTER_NAMES, ROOT_KIND_NAMES, SAFEPOINT_KIND_NAMES, STACKMAP_ENDIAN, STACKMAP_MAGIC,
    STACKMAP_POINTER_SIZE, STACKMAP_SECTION_VERSION,
};
use std::mem::offset_of;

/// 解析 `roots.gg` 里的无符号常量；成对的 `#[cfg(arch = "aarch64")]` 声明取 x64 变体，
/// 因为这个镜像测试对照的是 x64 codec 常量。
fn roots_constant(name: &str) -> u64 {
    let prefixes = [
        format!("pub const {name}: uint = "),
        format!("const {name}: uint = "),
        format!("{name}: uint = "),
    ];
    let mut gated_a64 = false;
    for line in super::RUNTIME_ROOTS_SOURCE.lines() {
        let trimmed = line.trim();
        let literal = prefixes
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix.as_str()));
        if let Some(literal) = literal
            && !gated_a64
        {
            return super::tests::parse_uint_literal(literal)
                .unwrap_or_else(|| panic!("{name} 不是可解析的无符号字面量：{literal}"));
        }
        gated_a64 = trimmed == "#[cfg(arch = \"aarch64\")]";
    }
    panic!("Gugu 源缺少 x64 变体常量 {name}");
}

/// 解析 `gc_probes.gg` 里的无符号常量。
#[cfg(feature = "runtime-gc-probes")]
fn probe_constant(name: &str) -> u64 {
    super::tests::source_uint_constant(super::RUNTIME_GC_PROBES_SOURCE, name)
}

/// 探针成功输出的字节数必须与字面量一致：多写一个字节会让镜像 stdout 与 bench 期望不符，
/// 少写一个字节会截断报告。字面量里的 `\n` 是源级转义，按还原后的字节计长度。
#[cfg(feature = "runtime-gc-probes")]
#[test]
fn gc_probe_success_message_length_matches_its_literal() {
    let line = super::RUNTIME_GC_PROBES_SOURCE
        .lines()
        .find(|line| line.contains("c\"") && line.contains("gc:gc-atomics:ok"))
        .expect("探针缺少成功输出字面量");
    let literal = line
        .split_once("c\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(text, _)| text)
        .expect("成功输出必须是 c 字面量");
    let bytes = literal.replace("\\n", "\n").len();
    assert_eq!(
        probe_constant("GC_PROBE_SUCCESS_BYTES"),
        u64::try_from(bytes).expect("长度适配 u64"),
        "成功输出长度必须等于字面量字节数"
    );
}

#[test]
fn roots_source_mirrors_the_stackmap_section_constants() {
    let magic = u64::from_le_bytes(*STACKMAP_MAGIC);
    assert_eq!(roots_constant("ROOT_SECTION_MAGIC"), magic);
    // 头部第二字按小端打包「版本 u16 / 指针宽度 u8 / 字节序 u8」。
    let version_word = u32::from(STACKMAP_SECTION_VERSION)
        | u32::from(STACKMAP_POINTER_SIZE) << 16
        | u32::from(STACKMAP_ENDIAN) << 24;
    assert_eq!(
        roots_constant("ROOT_SECTION_VERSION_WORD"),
        u64::from(version_word)
    );
    assert_eq!(
        roots_constant("ROOT_SECTION_HEADER_BYTES"),
        u64::try_from(HEADER_BYTES).expect("头字节数适配 u64")
    );
    assert_eq!(
        roots_constant("ROOT_FUNCTION_BYTES"),
        u64::try_from(FUNCTION_BYTES).expect("函数记录适配 u64")
    );
    assert_eq!(
        roots_constant("ROOT_SAFEPOINT_BYTES"),
        u64::try_from(SAFEPOINT_BYTES).expect("安全点记录适配 u64")
    );
    assert_eq!(
        roots_constant("ROOT_TYPED_ENTRY_BYTES"),
        u64::try_from(TYPED_ROOT_BYTES).expect("逐根条目适配 u64")
    );
    // map 索引表每项 8 字节（start/end 两个 section 内偏移）。
    assert_eq!(roots_constant("ROOT_MAP_INDEX_BYTES"), 8);
    // map 记录固定头＝槽数 4 + 五个掩码 10 + 保留 2。
    assert_eq!(roots_constant("ROOT_MAP_HEAD_BYTES"), 16);
    assert_eq!(roots_constant("ROOT_RECORD_BYTES"), 4 * 8);
    assert_eq!(
        roots_constant("ROOT_GPR_LANES"),
        u64::try_from(REGISTER_NAMES.len() + 1).expect("寄存器槽数适配 u64")
    );
    assert_eq!(
        roots_constant("ROOT_GPR_BYTES"),
        roots_constant("ROOT_GPR_LANES") * 8
    );
    assert_eq!(roots_constant("ROOT_NO_ENTRY"), u64::from(u32::MAX));
    // fatal 报告是同长度单行文本：站点标签必须填满固定宽度，长度常量才不会截断报告。
    let report = roots_constant("ROOT_FAIL_BYTES");
    let mut sites = 0;
    for line in super::RUNTIME_ROOTS_SOURCE.lines() {
        let Some(start) = line.find("c\"runtime fatal: root scan: ") else {
            continue;
        };
        let literal = &line[start + 2..];
        let end = literal
            .find('"')
            .unwrap_or_else(|| panic!("roots.gg 的报告字面量没有收尾引号：{line}"));
        // Gugu 的 `c"..."` 不把结尾 NUL 计入报告长度，换行是最后一个字节。
        let text = literal[..end].replace("\\n", "\n").replace("\\\\", "\\");
        assert_eq!(
            text.len() as u64,
            report,
            "报告 `{text}` 的长度必须等于 ROOT_FAIL_BYTES"
        );
        assert!(text.ends_with('\n'), "报告必须以换行结束：{text}");
        sites += 1;
    }
    assert_eq!(sites, 8, "每个站点都必须有自己的报告行");
}

#[test]
fn roots_source_mirrors_root_and_safepoint_kind_order() {
    let kind = |name: &str| {
        u64::try_from(
            ROOT_KIND_NAMES
                .iter()
                .position(|candidate| *candidate == name)
                .unwrap_or_else(|| panic!("根种类目录缺少 {name}")),
        )
        .expect("根种类下标适配 u64")
    };
    for (constant, name) in [
        ("ROOT_KIND_DIRECT", "heap-direct"),
        ("ROOT_KIND_INTERIOR", "heap-interior"),
        ("ROOT_KIND_HANDLE", "shared-handle"),
        ("ROOT_KIND_COMPRESSED", "compressed-ref"),
        ("ROOT_KIND_STACK_INTERIOR", "stack-interior"),
    ] {
        assert_eq!(
            roots_constant(constant),
            kind(name),
            "{constant} 与 compiler 的 {name} 判别值不一致"
        );
    }
    let safepoint = |name: &str| {
        let index = SAFEPOINT_KIND_NAMES
            .iter()
            .position(|candidate| *candidate == name)
            .unwrap_or_else(|| panic!("安全点目录缺少 {name}"));
        u64::try_from(index).expect("安全点下标适配 u64")
    };
    assert_eq!(
        roots_constant("SAFEPOINT_KIND_CALL_RETURN"),
        safepoint("call-return")
    );
    assert_eq!(
        roots_constant("SAFEPOINT_KIND_POLL_RESUME"),
        safepoint("poll-resume")
    );
    assert_eq!(
        roots_constant("SAFEPOINT_KIND_SUSPEND_RESUME"),
        safepoint("suspend-resume")
    );
    assert_eq!(
        roots_constant("SAFEPOINT_KIND_FOREIGN_BRIDGE"),
        safepoint("foreign-bridge")
    );
    assert_eq!(
        roots_constant("SAFEPOINT_KIND_MORESTACK_ENTRY"),
        safepoint("morestack-entry")
    );
}

#[test]
fn roots_source_mirrors_runtime_state_and_payload_slots() {
    // 载荷字段：walker 按同一份 `Rt0Payload` 取 section/长度/代码基址。
    assert_eq!(
        roots_constant("ROOT_PAYLOAD_STACKMAP_OFFSET"),
        u64::try_from(offset_of!(Rt0Payload, stackmap)).expect("字段偏移适配 u64")
    );
    assert_eq!(
        roots_constant("ROOT_PAYLOAD_BASE_OFFSET"),
        u64::try_from(offset_of!(Rt0Payload, fragment_base)).expect("字段偏移适配 u64")
    );
    assert_eq!(
        roots_constant("ROOT_PAYLOAD_LENGTH_OFFSET"),
        u64::try_from(offset_of!(Rt0Payload, stackmap_len)).expect("字段偏移适配 u64")
    );
    // 状态区槽位：payload 地址、加载偏移、processor 页里的 state 指针、hot 里的 processor。
    assert_eq!(roots_constant("ROOT_STATE_PAYLOAD_OFFSET"), state::PAYLOAD);
    assert_eq!(roots_constant("ROOT_STATE_BIAS_OFFSET"), state::BIAS);
    assert_eq!(
        roots_constant("ROOT_HOT_PROCESSOR_OFFSET"),
        u64::try_from(offset_of!(CoroutineHot, current_processor)).expect("字段偏移适配 u64")
    );
    assert_eq!(
        roots_constant("ROOT_PROCESSOR_STATE_OFFSET"),
        u64::try_from(super::processor::PROCESSOR_STATE_OFFSET).expect("processor 槽适配 u64")
    );
}

#[test]
fn rt0_writes_the_same_state_slots_as_the_root_walker_reads() {
    // writer（core.gg 的 rt0_start）与 reader（roots.gg 的 walker）必须落在同一批槽位。
    let source = super::RUNTIME_CORE_SOURCE;
    for expected in [
        format!(
            "store_word(state + {}, block.payload_va + bias)",
            state::PAYLOAD
        ),
        format!("store_word(state + {}, address_bias)", state::BIAS),
        format!("let cold = state + {}", state::COLD),
        format!("let reason = word(state + {})", state::REASON),
    ] {
        assert!(
            source.contains(&expected),
            "core.gg 缺少状态区写入 `{expected}`；walker 会在错误的槽位取 section"
        );
    }
}

/// 探针源必须与生产 runtime 一起通过全部前端/后端与镜像校验。
///
/// 探针只在 `runtime-gc-probes` 打开时注入，因此这条断言也只在同一 feature 下成立；它把
/// 「探针本身编译不过」挡在真实镜像执行之前，避免用退出码把编译失败伪装成运行期结论。
#[cfg(feature = "runtime-gc-probes")]
#[test]
fn gc_probe_image_passes_every_compiler_contract() {
    let compilation = crate::Compiler::shared().compile_full_for_test(
        crate::CompileRequest::runtime_gc_probe("gc-atomics", crate::TargetName::X86_64Linux),
    );
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
}
