//! 目标描述符、sysroot 与成本/调优 profile 身份回归：字段变化必须移动 descriptor 身份。

use crate::target::{
    Architecture, BackendCostProfile, CPU_FEATURES, CpuBaseline, CpuFeature,
    IMPORT_POLICY_REVISION, ObjectFormat, OperatingSystem, PointerCompression, Rt0Kind,
    TARGET_PAGE_SIZE, TargetName, baseline_cost_profile, target_sysroot_digest,
};
use crate::tuning::RUNTIME_TUNING_PROFILE;

#[test]
fn dimension_names_are_canonical() {
    assert_eq!(Architecture::X86_64.name(), "x86_64");
    assert_eq!(Architecture::Aarch64.name(), "aarch64");
    assert_eq!(OperatingSystem::Linux.name(), "linux");
    assert_eq!(OperatingSystem::Windows.name(), "windows");
    assert_eq!(OperatingSystem::Macos.name(), "macos");
    assert_eq!(ObjectFormat::Elf64.name(), "elf64");
    assert_eq!(ObjectFormat::Pe32Plus.name(), "pe32+");
    assert_eq!(ObjectFormat::MachO64.name(), "macho64");
    assert_eq!(Rt0Kind::MacosLibSystem.name(), "macos-libsystem");
}

#[test]
fn arm_baseline_rejects_all_x86_features() {
    assert_eq!(CpuBaseline::Armv8A.name(), "armv8.0-a");
    for feature in CPU_FEATURES {
        assert!(!CpuBaseline::Armv8A.allows(feature));
    }
}

#[test]
fn aarch64_pointer_compression_uses_48_bit_addresses() {
    let capability = PointerCompression::aarch64();
    assert!(capability.supported);
    assert_eq!(capability.canonical_bits, 48);
    assert_eq!(capability.max_cage_bytes, 1 << 32);
    assert_eq!(capability.min_alignment, 2 * 1024 * 1024);
    assert_eq!(capability, PointerCompression::x86_64());
}

#[test]
fn dimension_values_do_not_register_targets() {
    for name in ["", "x86_64-macos"] {
        let error = TargetName::parse(name).expect_err("unregistered target");
        assert_eq!(error.requested(), name);
    }
    let internal = TargetName::X86_64Macos.descriptor();
    assert_eq!(internal.os, OperatingSystem::Macos);
    assert_eq!(internal.object_format, ObjectFormat::MachO64);
    assert_eq!(internal.rt0, Rt0Kind::MacosLibSystem);
}

#[test]
fn aarch64_targets_are_registered_with_descriptors() {
    for (name, expected) in [
        ("aarch64-linux", TargetName::Aarch64Linux),
        ("aarch64-macos", TargetName::Aarch64Macos),
    ] {
        assert_eq!(TargetName::parse(name).expect("registered target"), expected);
        assert_eq!(
            TargetName::from_compiler_artifact(name).expect("registered target"),
            expected
        );
    }

    let linux = TargetName::Aarch64Linux.descriptor();
    assert_eq!(linux.arch, Architecture::Aarch64);
    assert_eq!(linux.os, OperatingSystem::Linux);
    assert_eq!(linux.object_format, ObjectFormat::Elf64);
    assert_eq!(linux.rt0, Rt0Kind::LinuxSyscall);
    assert_eq!(linux.page_size, TARGET_PAGE_SIZE);
    assert_eq!(linux.cpu_baseline, CpuBaseline::Armv8A);
    assert_eq!(linux.linux_interpreter, Some("/lib/ld-linux-aarch64.so.1"));
    assert_eq!(linux.pointer_compression, PointerCompression::aarch64());

    let macos = TargetName::Aarch64Macos.descriptor();
    assert_eq!(macos.arch, Architecture::Aarch64);
    assert_eq!(macos.os, OperatingSystem::Macos);
    assert_eq!(macos.object_format, ObjectFormat::MachO64);
    assert_eq!(macos.rt0, Rt0Kind::MacosLibSystem);
    assert_eq!(macos.page_size, 16 * 1024);
    assert_eq!(macos.cpu_baseline, CpuBaseline::Armv8A);
    assert_eq!(macos.linux_interpreter, None);
    assert_eq!(macos.pointer_compression, PointerCompression::aarch64());

    // 五个目标的 sysroot 身份必须互不相同。
    let digests = [
        target_sysroot_digest(TargetName::X86_64Linux),
        target_sysroot_digest(TargetName::X86_64Windows),
        target_sysroot_digest(TargetName::X86_64Macos),
        target_sysroot_digest(TargetName::Aarch64Linux),
        target_sysroot_digest(TargetName::Aarch64Macos),
    ];
    for (index, digest) in digests.iter().enumerate() {
        for other in &digests[index + 1..] {
            assert_ne!(digest, other);
        }
    }

    // 宿主目标选择只返回已登记目标（aarch64 宿主即对应 aarch64 目标）。
    assert_ne!(TargetName::host(), Some(TargetName::X86_64Macos));
    if let Some(host) = TargetName::host() {
        assert_ne!(host, TargetName::X86_64Macos);
    }
}

#[test]
fn registered_targets_have_distinct_stable_descriptor_digests() {
    let linux = TargetName::X86_64Linux.descriptor();
    let windows = TargetName::X86_64Windows.descriptor();
    assert_ne!(linux.digest(), windows.digest());
    assert_eq!(
        linux.digest(),
        TargetName::X86_64Linux.descriptor().digest()
    );
    assert_eq!(
        windows.digest(),
        TargetName::X86_64Windows.descriptor().digest()
    );
}

#[test]
fn descriptors_fix_page_size_cpu_baseline_and_interpreter() {
    let linux = TargetName::X86_64Linux.descriptor();
    let windows = TargetName::X86_64Windows.descriptor();
    assert_eq!(linux.page_size, TARGET_PAGE_SIZE);
    assert_eq!(windows.page_size, TARGET_PAGE_SIZE);
    assert_eq!(linux.cpu_baseline, CpuBaseline::X86_64V1);
    assert!(linux.cpu_baseline.allows(CpuFeature::X86_64));
    assert!(linux.cpu_baseline.allows(CpuFeature::Sse2));
    assert!(!linux.cpu_baseline.allows(CpuFeature::Ssse3));
    assert!(!linux.cpu_baseline.allows(CpuFeature::Sse41));
    assert!(!linux.cpu_baseline.allows(CpuFeature::Avx));
    assert_eq!(linux.linux_interpreter, Some("/lib64/ld-linux-x86-64.so.2"));
    assert_eq!(windows.linux_interpreter, None);
    assert_eq!(linux.import_policy_revision, IMPORT_POLICY_REVISION);
    assert_eq!(windows.import_policy_revision, IMPORT_POLICY_REVISION);
}

#[test]
fn sysroot_and_profile_digests_are_bound_to_descriptors() {
    let linux = TargetName::X86_64Linux.descriptor();
    let windows = TargetName::X86_64Windows.descriptor();
    assert_eq!(
        linux.sysroot_digest,
        target_sysroot_digest(TargetName::X86_64Linux)
    );
    assert_eq!(
        windows.sysroot_digest,
        target_sysroot_digest(TargetName::X86_64Windows)
    );
    // 导入库目录与解释器不同：两个 sysroot 身份必须分离。
    assert_ne!(linux.sysroot_digest, windows.sysroot_digest);
    assert_eq!(
        linux.runtime_tuning_profile_digest,
        RUNTIME_TUNING_PROFILE.digest()
    );
    assert_eq!(
        windows.runtime_tuning_profile_digest,
        RUNTIME_TUNING_PROFILE.digest()
    );
    assert_eq!(
        linux.backend_cost_profile_digest,
        linux.cost_profile.digest()
    );
    assert_eq!(
        windows.backend_cost_profile_digest,
        windows.cost_profile.digest()
    );
}

#[test]
fn descriptor_digest_tracks_pointer_compression_and_cost_profile() {
    let mut descriptor = TargetName::X86_64Linux.descriptor();
    let baseline = descriptor.digest();

    descriptor.pointer_compression.canonical_bits = 47;
    let compressed = descriptor.digest();
    assert_ne!(
        compressed, baseline,
        "cage 能力变化必须移动 descriptor 身份"
    );

    descriptor.pointer_compression = TargetName::X86_64Linux.descriptor().pointer_compression;
    assert_eq!(descriptor.digest(), baseline);

    descriptor.cost_profile = BackendCostProfile {
        max_spill_slots: 32,
        ..baseline_cost_profile()
    };
    assert_ne!(
        descriptor.digest(),
        baseline,
        "成本 profile 变化必须移动 descriptor 身份"
    );
}

#[test]
fn cost_profile_digest_is_field_sensitive() {
    let profile = baseline_cost_profile();
    let baseline = profile.digest();

    let mut changed = profile;
    changed.vector_lowering = true;
    assert_ne!(changed.digest(), baseline);
    changed = profile;
    changed.regression_percent = 6;
    assert_ne!(changed.digest(), baseline);
    changed = profile;
    changed.inline_hot_bytes = 128;
    assert_ne!(changed.digest(), baseline);
    assert_eq!(profile.digest(), baseline);
}
