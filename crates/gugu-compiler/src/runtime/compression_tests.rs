//! 压缩契约段与 cage 编码边界的确定性测试。
//!
//! 覆盖默认关闭、开关与需求闭合、粒度/上限/canonical 能力拒绝、指纹与 dump 口径。全部
//! 进程内运行：真实编译只跑默认 profile，其余用例直接构造契约。

use super::cage::is_canonical;
use super::compression_schema::{
    CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract,
};
use super::gc_metadata_contract::GC_ARENA_BYTES;
use crate::target::PointerCompression;

/// 启用态契约：`4` 个 arena 粒度的 cage 与单点需求。
fn enabled_contract() -> CompressionRuntimeContract {
    CompressionRuntimeContract::build(
        CompressionDemand {
            decode_sites: 1,
            compressed_root_slots: 1,
        },
        CompressionPolicyV1::cage(4 * GC_ARENA_BYTES),
        PointerCompression::x86_64(),
    )
    .expect("启用态契约可构建")
}

/// 真实编译默认走 full-pointer：不启用 cage、不预留地址，指纹可复现。
#[test]
fn real_compilation_defaults_to_disabled_compression() {
    let source = "fn main() { let value = 1 _ = value }";
    let compile = || {
        crate::Compiler::new().compile(crate::CompileRequest::single_file(
            "main.gg",
            source,
            crate::TargetName::X86_64Linux,
        ))
    };
    let compilation = compile();
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let raw = compilation.raw_contract().expect("真实契约必须存在");
    let compression = raw.compression();
    assert!(!compression.enabled());
    assert_eq!(compression.cage_bytes(), 0);
    assert_eq!(compression.cage_granule_bytes(), GC_ARENA_BYTES);
    assert_eq!(compression.max_cage_bytes(), 1 << 32);
    assert_eq!(compression.canonical_bits(), 48);
    assert!(compression.capability().supported);
    assert_eq!(compression.demand().decode_sites, 0);
    assert_eq!(compression.demand().compressed_root_slots, 0);
    let again = compile();
    assert_eq!(
        again
            .raw_contract()
            .expect("真实契约必须存在")
            .compression()
            .fingerprint(),
        compression.fingerprint(),
        "同一输入的内容身份必须稳定"
    );
    assert!(
        compression.dump().starts_with(
            "compression schema=1 profile=mosaic-compression revision=1 enabled=false"
        ),
        "dump 头必须固定 profile 与开关：{}",
        compression.dump()
    );
    assert!(
        raw.dump().contains("compression-ffi-rules resolve-then-pin,no-compressed-pass-through,save-requires-active-lease"),
        "整体 dump 必须包含 FFI 交接规则目录"
    );
}

/// 同一需求下 cage 字节数参与内容身份：不同容量必须给出不同指纹与字节。
#[test]
fn cage_bytes_participate_in_contract_identity() {
    let small = CompressionRuntimeContract::build(
        CompressionDemand {
            decode_sites: 1,
            compressed_root_slots: 1,
        },
        CompressionPolicyV1::cage(2 * GC_ARENA_BYTES),
        PointerCompression::x86_64(),
    )
    .expect("小 cage 契约可构建");
    let large = CompressionRuntimeContract::build(
        CompressionDemand {
            decode_sites: 1,
            compressed_root_slots: 1,
        },
        CompressionPolicyV1::cage(4 * GC_ARENA_BYTES),
        PointerCompression::x86_64(),
    )
    .expect("大 cage 契约可构建");
    assert_ne!(small.fingerprint(), large.fingerprint());
    assert_ne!(small.canonical_bytes(), large.canonical_bytes());
    assert_eq!(small.cage_bytes(), 2 * GC_ARENA_BYTES);
    assert_eq!(large.cage_bytes(), 4 * GC_ARENA_BYTES);
    assert_eq!(
        small.fingerprint(),
        CompressionRuntimeContract::build(
            small.demand(),
            CompressionPolicyV1::cage(2 * GC_ARENA_BYTES),
            PointerCompression::x86_64(),
        )
        .expect("同参数契约可构建")
        .fingerprint(),
        "同参数构建必须给出同指纹"
    );
}

/// 子段自洽的篡改必须在 `verify` 里逐条暴露。
#[test]
fn tampered_contract_fields_are_rejected() {
    let contract = enabled_contract();
    let mut disabled = contract.clone();
    disabled.enabled = false;
    assert_eq!(
        disabled.verify().expect_err("关闭开关必须拒绝").message(),
        "存在解码点却没有开启 cage profile"
    );
    let mut reserved = contract.clone();
    reserved.demand = CompressionDemand::default();
    reserved.enabled = false;
    assert_eq!(
        reserved
            .verify()
            .expect_err("关闭态不得预留 cage")
            .message(),
        "未启用 cage profile 时不得预留 cage、出现解码点或压缩根"
    );
    let mut misaligned = contract.clone();
    misaligned.cage_bytes += 1;
    assert_eq!(
        misaligned
            .verify()
            .expect_err("非粒度倍数必须拒绝")
            .message(),
        "cage 字节数必须是 arena 粒度的整数倍"
    );
    let mut oversized = contract.clone();
    oversized.cage_bytes = oversized.max_cage_bytes + GC_ARENA_BYTES;
    assert_eq!(
        oversized.verify().expect_err("超过上限必须拒绝").message(),
        "cage 字节数超过目标能力"
    );
    let mut narrow_canonical = contract.clone();
    narrow_canonical.capability.canonical_bits = 47;
    assert_eq!(
        narrow_canonical
            .verify()
            .expect_err("位宽不一致必须拒绝")
            .message(),
        "目标 canonical 位宽与 cage 编码不一致"
    );
    let mut unsupported = contract.clone();
    unsupported.capability.supported = false;
    assert_eq!(
        unsupported
            .verify()
            .expect_err("不支持的目标必须拒绝")
            .message(),
        "目标不支持 checked pointer compression"
    );
    let mut shifted = contract.clone();
    shifted.cage_generation_shift = 33;
    assert_eq!(
        shifted
            .verify()
            .expect_err("位移与登记值不一致必须拒绝")
            .message(),
        "压缩引用编码位移或掩码与登记值不一致"
    );
    let mut tampered = contract.clone();
    tampered.fingerprint[0] ^= 1;
    assert_eq!(
        tampered.verify().expect_err("指纹篡改必须拒绝").message(),
        "压缩契约指纹与内容不一致"
    );
}

/// 目标能力缺失与「开关关闭却有解码点」都在构建期被拒绝。
#[test]
fn unsupported_capability_and_stray_decode_sites_are_rejected() {
    let unsupported = CompressionRuntimeContract::build(
        CompressionDemand {
            decode_sites: 1,
            compressed_root_slots: 0,
        },
        CompressionPolicyV1::cage(2 * GC_ARENA_BYTES),
        PointerCompression::unsupported(),
    )
    .expect_err("不支持的目标必须拒绝");
    assert_eq!(
        unsupported.message(),
        "目标不支持 checked pointer compression"
    );
    let stray = CompressionRuntimeContract::build(
        CompressionDemand {
            decode_sites: 1,
            compressed_root_slots: 0,
        },
        CompressionPolicyV1::disabled(),
        PointerCompression::x86_64(),
    )
    .expect_err("关闭 profile 时解码点必须拒绝");
    assert_eq!(stray.message(), "存在解码点却没有开启 cage profile");
    let stray_roots = CompressionRuntimeContract::build(
        CompressionDemand {
            decode_sites: 0,
            compressed_root_slots: 1,
        },
        CompressionPolicyV1::disabled(),
        PointerCompression::x86_64(),
    )
    .expect_err("关闭 profile 时压缩根槽必须拒绝");
    assert_eq!(
        stray_roots.message(),
        "未启用 cage profile 时不得预留 cage、出现解码点或压缩根"
    );
}

/// 48 位 canonical 判定：低半、高半与 canonical hole。
#[test]
fn canonical_boundary_is_checked_at_48_bits() {
    assert!(is_canonical((1 << 47) - 1, 48));
    assert!(!is_canonical(1 << 47, 48));
    assert!(!is_canonical((1 << 47) + (1 << 40), 48));
    assert!(is_canonical(u64::MAX, 48));
    assert!(is_canonical(u64::MAX - ((1 << 47) - 1), 48));
    assert!(!is_canonical(u64::MAX - (1 << 47), 48));
    assert!(!is_canonical(0, 0));
    assert!(is_canonical(0, 64));
}
