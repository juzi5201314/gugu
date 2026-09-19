//! raw link provenance 与 release 安全 profile 契约段和平面的确定性测试。
//!
//! 覆盖默认 release、profile 参与契约身份与 action key、契约漂移拒绝、per-domain
//! secret 的互异性与 domain 绑定、伪造 link 的稳定分类。全部进程内运行。

use super::extent::ExtentId;
use super::message::LinkError;
use super::provenance::{POISON_WORD, ProvenancePlane, ReleaseRejection, classify_chain_error};
use super::provenance_schema::{
    DEBUG_EXTRA_CHECKS, PROVENANCE_CHECKS, PROVENANCE_DOMAINS, PROVENANCE_PROFILE_NAME,
    PROVENANCE_PROFILE_REVISION, PROVENANCE_REJECTIONS, PROVENANCE_SCHEMA, ProvenanceDemand,
    ProvenancePolicyV1, ProvenanceRuntimeContract, RELEASE_BASELINE_CHECKS, SECURITY_EXTRA_CHECKS,
    SafetyProfile,
};
use super::size_class::RuntimeSizeClassTable;
use super::slab::{
    Epoch, MemoryDomainId, OwnerGeneration, OwnerId, OwnerToken, RawInvariant, RouteKey,
    SlabGeneration, SlabTable,
};

/// 在指定 domain 上构造一个 4 slot 的测试 descriptor。
fn descriptor_for_domain(domain: MemoryDomainId) -> super::slab::SlabDescriptor {
    let classes = RuntimeSizeClassTable::ladder(domain).expect("阶梯可构建");
    let class = &classes.classes()[0];
    let mut table = SlabTable::new();
    let id = table
        .create(
            class,
            OwnerToken {
                domain,
                owner_id: OwnerId::from_raw(1),
                generation: OwnerGeneration::from_raw(1),
                route_key: RouteKey::from_raw(1),
            },
            ExtentId::from_raw(1),
            u64::from(class.slot_stride) * 4,
            0,
            Epoch::from_raw(0),
        )
        .expect("描述符可创建");
    table.descriptor(id).expect("描述符存在").clone()
}

/// 用真实编译检查默认 profile：release、指纹可复现、dump 固定口径。
#[test]
fn real_compilation_defaults_to_release_provenance() {
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
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.provenance_runtime().mode(), SafetyProfile::Release);
    assert_eq!(plan.provenance_runtime().profile(), PROVENANCE_PROFILE_NAME);
    assert_eq!(
        plan.provenance_runtime().profile_revision(),
        PROVENANCE_PROFILE_REVISION
    );
    let fingerprint = plan.provenance_contract_fingerprint();
    assert_ne!(fingerprint, [0; 32]);
    // 冷/热两次编译的 provenance 契约逐字节一致。
    let replay = compile();
    assert_eq!(
        replay
            .image_plan()
            .expect("镜像计划")
            .provenance_contract_fingerprint(),
        fingerprint
    );
    let dump = compilation.dump_runtime().expect("runtime dump");
    assert!(dump.contains("provenance schema=1 profile=mosaic-provenance revision=1 mode=release"));
    assert!(dump.contains(&format!(
        "provenance-checks {}",
        PROVENANCE_CHECKS.join(",")
    )));
    assert!(dump.contains(&format!(
        "provenance-active {}",
        RELEASE_BASELINE_CHECKS.join(",")
    )));
    assert!(dump.contains(&format!(
        "provenance-domains {}",
        PROVENANCE_DOMAINS.join(",")
    )));
    assert!(dump.contains("provenance-demand "));
    assert!(dump.contains("provenance-fingerprint "));
}

/// 显式 profile 改变契约身份与 action key；同 profile 冷/热一致。
#[test]
fn safety_profile_participates_in_contract_identity_and_action_key() {
    let source = "fn main() { let value = 1 _ = value }";
    let compile = |policy: ProvenancePolicyV1| {
        crate::Compiler::new().compile(
            crate::CompileRequest::single_file("main.gg", source, crate::TargetName::X86_64Linux)
                .with_provenance_policy(policy),
        )
    };
    let release = compile(ProvenancePolicyV1::release());
    let debug = compile(ProvenancePolicyV1 {
        profile: SafetyProfile::Debug,
    });
    let security = compile(ProvenancePolicyV1 {
        profile: SafetyProfile::Security,
    });
    for compilation in [&release, &debug, &security] {
        assert!(
            compilation.is_success(),
            "{:?}",
            compilation.diagnostics().items()
        );
    }
    let release_plan = release.image_plan().expect("镜像计划");
    let debug_plan = debug.image_plan().expect("镜像计划");
    let security_plan = security.image_plan().expect("镜像计划");
    assert_eq!(debug_plan.provenance_runtime().mode(), SafetyProfile::Debug);
    assert_eq!(
        security_plan.provenance_runtime().mode(),
        SafetyProfile::Security
    );
    // debug/security 激活基线加各自额外检查；三个 profile 的指纹互不相同。
    let expected_debug: Vec<String> = RELEASE_BASELINE_CHECKS
        .iter()
        .chain(DEBUG_EXTRA_CHECKS.iter())
        .map(|name| (*name).to_owned())
        .collect();
    let expected_security: Vec<String> = RELEASE_BASELINE_CHECKS
        .iter()
        .chain(SECURITY_EXTRA_CHECKS.iter())
        .map(|name| (*name).to_owned())
        .collect();
    assert_eq!(
        debug_plan.provenance_runtime().active_checks.clone(),
        expected_debug
    );
    assert_eq!(
        security_plan.provenance_runtime().active_checks.clone(),
        expected_security
    );
    assert_ne!(
        release_plan.provenance_contract_fingerprint(),
        debug_plan.provenance_contract_fingerprint()
    );
    assert_ne!(
        release_plan.provenance_contract_fingerprint(),
        security_plan.provenance_contract_fingerprint()
    );
    assert_ne!(
        debug_plan.provenance_contract_fingerprint(),
        security_plan.provenance_contract_fingerprint()
    );
    // 整体契约身份（runtime raw fingerprint）与 action key 随 profile 变化。
    assert_ne!(
        release.runtime_raw_fingerprint(),
        debug.runtime_raw_fingerprint()
    );
    assert_ne!(release.action_key(), debug.action_key());
    // 同一 profile 重放保持一致。
    let replay = compile(ProvenancePolicyV1 {
        profile: SafetyProfile::Debug,
    });
    assert_eq!(
        replay
            .image_plan()
            .expect("镜像计划")
            .provenance_contract_fingerprint(),
        debug_plan.provenance_contract_fingerprint()
    );
    assert_eq!(replay.action_key(), debug.action_key());
}

/// 契约段漂移：mode、激活检查、目录、指纹任一与登记值不一致都必须拒绝。
#[test]
fn provenance_contract_rejects_drift() {
    let build =
        |policy| ProvenanceRuntimeContract::build(ProvenanceDemand::derive(2, 7, 5), policy);
    let contract = build(ProvenancePolicyV1::release()).expect("release 契约可构建");
    contract.verify().expect("release 契约自洽");
    let debug_fingerprint = build(ProvenancePolicyV1 {
        profile: SafetyProfile::Debug,
    })
    .expect("debug 契约可构建")
    .fingerprint();
    assert_ne!(contract.fingerprint, debug_fingerprint);

    // 激活检查与 release profile 不一致。
    let mut drifted = contract.clone();
    drifted.active_checks = DEBUG_EXTRA_CHECKS
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert!(drifted.verify().is_err());

    // 激活检查引用不在目录中的检查。
    let mut unknown = contract.clone();
    unknown.active_checks[0] = "teleport".to_owned();
    assert!(unknown.verify().is_err());

    // 检查目录漂移。
    let mut checks = contract.clone();
    checks.checks = PROVENANCE_CHECKS[1..]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert!(checks.verify().is_err());

    // 拒绝分类漂移。
    let mut rejections = contract.clone();
    rejections.rejections = PROVENANCE_REJECTIONS[1..]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert!(rejections.verify().is_err());

    // 指纹与内容不一致。
    let mut fingerprint = contract.clone();
    fingerprint.fingerprint = [1; 32];
    assert!(fingerprint.verify().is_err());

    // schema 漂移。
    let mut schema = contract.clone();
    schema.schema = PROVENANCE_SCHEMA + 1;
    assert!(schema.verify().is_err());
}

/// per-domain secret 互异、非零，且 link 编码被 domain 绑定：跨 domain 的 codec 无法解码。
#[test]
fn domain_secrets_are_bound_to_their_own_links() {
    let plane = ProvenancePlane::new([7; 32], SafetyProfile::Release, 0x1234).expect("平面可构建");
    for (index, domain) in MemoryDomainId::ALL.iter().enumerate() {
        assert_ne!(plane.secret_for(*domain), &[0; 32]);
        for other in &MemoryDomainId::ALL[index + 1..] {
            assert_ne!(plane.secret_for(*domain), plane.secret_for(*other));
        }
    }
    // 跨 domain 的 codec 对同一字解码必然失败：secret 不同 → checksum 或 tag 不匹配。
    let descriptor = descriptor_for_domain(MemoryDomainId::RUNTIME_RAW);
    let raw_codec = plane.codec_for(MemoryDomainId::RUNTIME_RAW);
    let resource_codec = plane.codec_for(MemoryDomainId::RESOURCE);
    let offset = descriptor.slot_offset(0);
    let word = raw_codec.encode(&descriptor, offset).expect("编码成功");
    assert_eq!(raw_codec.decode(&descriptor, word), Ok(0));
    assert!(matches!(
        resource_codec.decode(&descriptor, word),
        Err(LinkError::Checksum | LinkError::Foreign { .. })
    ));
}

/// 链走查失败按稳定分类记账：链损坏、伪造 link、过期 generation 逐项对应。
#[test]
fn chain_errors_are_classified_stably() {
    use super::message::ChainWalkError;
    assert_eq!(
        classify_chain_error(&ChainWalkError::Link(LinkError::Null)).name(),
        "chain-corruption"
    );
    assert_eq!(
        classify_chain_error(&ChainWalkError::Link(LinkError::Checksum)).name(),
        "chain-corruption"
    );
    assert_eq!(
        classify_chain_error(&ChainWalkError::Link(LinkError::Foreign { descriptor: 3 })).name(),
        "forged-link"
    );
    assert_eq!(
        classify_chain_error(&ChainWalkError::Link(LinkError::OutOfRange {
            offset: 1,
            extent: 2
        }))
        .name(),
        "forged-link"
    );
    assert_eq!(
        classify_chain_error(&ChainWalkError::Link(LinkError::Alignment { offset: 3 })).name(),
        "forged-link"
    );
    assert_eq!(
        classify_chain_error(&ChainWalkError::Link(LinkError::Generation {
            expected: 2,
            actual: 1
        }))
        .name(),
        "stale-generation"
    );
    assert_eq!(
        classify_chain_error(&ChainWalkError::OutsideSpan).name(),
        "forged-link"
    );
    assert_eq!(
        classify_chain_error(&ChainWalkError::Overflow).name(),
        "chain-corruption"
    );
}

/// 伪造 link 的端到端分类：校验位合法但 tag 被替换 → Foreign → forged-link。
#[test]
fn forged_tag_is_rejected_as_forged_link() {
    let plane = ProvenancePlane::new([7; 32], SafetyProfile::Release, 0x1234).expect("平面可构建");
    let codec = plane.codec_for(MemoryDomainId::RUNTIME_RAW);
    let descriptor = descriptor_for_domain(MemoryDomainId::RUNTIME_RAW);
    let offset = descriptor.slot_offset(1);
    // 真实 tag 是 16 位派生值；替换成不匹配的 tag 并按它重算 checksum，
    // 这样的字能通过 checksum 但通不过归属 tag 校验。
    let extent = descriptor.extent.raw();
    let mut forged_tag = 0_u16;
    let word = loop {
        forged_tag = forged_tag.wrapping_add(1);
        let candidate = codec.forge_word(offset, forged_tag).expect("伪造字可构造");
        if codec.decode(&descriptor, candidate) == Err(LinkError::Foreign { descriptor: extent }) {
            break candidate;
        }
    };
    assert!(matches!(
        codec.decode(&descriptor, word),
        Err(LinkError::Foreign { .. })
    ));
    assert_eq!(ReleaseRejection::ForgedLink.name(), "forged-link");
}

/// debug poison 标记字是稳定的非零调试标记，不是安全 secret。
#[test]
fn poison_word_is_a_stable_debug_marker() {
    assert_ne!(POISON_WORD, 0);
    assert_ne!(POISON_WORD, super::message::NULL_LINK);
    assert_eq!(RawInvariant::new("x").message(), "x");
    assert_eq!(SlabGeneration::from_raw(3).raw(), 3);
}
