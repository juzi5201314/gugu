//! 两目标显式 native 配置经过 Compiler、action key 和真实镜像 writer；不执行宿主程序。
use gugu_compiler::{
    CompileRequest, Compiler, EMBEDDED_ARTIFACTS, NativeImport, NativeLink, TargetName,
};

fn verify(target: TargetName) {
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);
    let library = if target == TargetName::X86_64Linux {
        "libprobe.so"
    } else {
        "probe.dll"
    };
    let config = NativeLink {
        imports: vec![NativeImport::function(library, "native_probe")],
        interpreter: (target == TargetName::X86_64Linux).then(|| "/toolchain/ld.so".into()),
    };
    let request = CompileRequest::single_file(
        "main.gg",
        include_str!("../fixtures/native_link.gg"),
        target,
    );
    let output = compiler.compile_full_for_test(request.clone().with_native_link(config.clone()));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let plan = output.image_plan().unwrap();
    let bytes = if target == TargetName::X86_64Linux {
        assert_eq!(plan.linux_image_kind(), "dynamic-pie");
        assert_eq!(plan.linux_interpreter(), "/toolchain/ld.so");
        plan.linux_image()
    } else {
        plan.windows_image()
    };
    assert!(
        bytes
            .windows(library.len())
            .any(|bytes| bytes == library.as_bytes())
    );
    assert!(bytes.windows(13).any(|bytes| bytes == b"native_probe\0"));
    let mut changed = config;
    changed.imports[0].library = if target == TargetName::X86_64Linux {
        "libother.so"
    } else {
        "other.dll"
    }
    .into();
    let second = compiler.compile_full_for_test(request.with_native_link(changed));
    assert!(second.is_success(), "{:?}", second.diagnostics().items());
    assert_ne!(
        output.action_key(),
        second.action_key(),
        "更换导入库必须失效 action"
    );
}

fn ordinary(compiler: &Compiler, target: TargetName) {
    let mut imports = vec![NativeImport::function(
        if target == TargetName::X86_64Linux {
            "libprobe.so"
        } else {
            "probe.dll"
        },
        "native_probe",
    )];
    if target == TargetName::X86_64Linux {
        imports.extend(
            [
                "pthread_create",
                "pthread_tryjoin_np",
                "pthread_attr_init",
                "pthread_attr_setstack",
                "pthread_attr_destroy",
                "__errno_location",
            ]
            .map(|symbol| NativeImport::function("libc.so.6", symbol)),
        );
    }
    let output = compiler.compile_full_for_test(
        CompileRequest::single_file(
            "main.gg",
            include_str!("../fixtures/foreign_bridge.gg"),
            target,
        )
        .with_native_link(NativeLink {
            imports,
            interpreter: (target == TargetName::X86_64Linux).then(|| "/toolchain/ld.so".into()),
        }),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let lir = output.dump_lir().unwrap();
    assert!(lir.contains("fn foreign_dispatch "));
    assert!(lir.contains("fn native_call "));
    assert!(output.image_plan().is_some());
}

fn missing_target(compiler: &Compiler, target: TargetName) {
    let imports = if target == TargetName::X86_64Linux {
        NativeLink::THREAD_SYMBOLS
            .into_iter()
            .map(|name| NativeImport::function("libc.so.6", name))
            .collect()
    } else {
        Vec::new()
    };
    let output = compiler.compile_full_for_test(
        CompileRequest::single_file(
            "main.gg",
            include_str!("../fixtures/foreign_bridge.gg"),
            target,
        )
        .with_native_link(NativeLink {
            imports,
            interpreter: (target == TargetName::X86_64Linux).then(|| "/target/ld.so".into()),
        }),
    );
    assert!(
        !output.is_success(),
        "缺少 native 导入时 bridge 镜像必须失败"
    );
    assert!(output.image_plan().is_none());
    assert!(
        output
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.message().contains("native_probe"))
    );
}

#[test]
fn linux_foreign_bridge_link_contracts() {
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);
    ordinary(&compiler, TargetName::X86_64Linux);
    missing_target(&compiler, TargetName::X86_64Linux);
}

#[test]
fn linux_foreign_bridge_requires_explicit_target_glibc_imports() {
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);
    let output = compiler.compile_full_for_test(CompileRequest::single_file(
        "main.gg",
        include_str!("../fixtures/foreign_bridge.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(!output.is_success());
    assert!(output.image_plan().is_none());
    assert!(
        output
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.message().contains("目标 glibc"))
    );
}

#[test]
fn windows_foreign_bridge_link_contracts() {
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);
    ordinary(&compiler, TargetName::X86_64Windows);
    missing_target(&compiler, TargetName::X86_64Windows);
}

#[test]
fn dirty_foreign_bridge_reaches_windows_dispatcher() {
    let output =
        Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_to_lir(CompileRequest::single_file(
            "main.gg",
            include_str!("../fixtures/dirty_bridge.gg"),
            TargetName::X86_64Windows,
        ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let lir = output.dump_lir().unwrap();
    assert!(lir.contains("fn foreign_dirty_dispatch "));
    assert!(lir.contains("fn dirty_snapshot "));
}

#[test]
fn native_link_configuration_reaches_linux_image() {
    verify(TargetName::X86_64Linux);
}
#[test]
fn native_link_configuration_reaches_windows_image() {
    verify(TargetName::X86_64Windows);
}

#[test]
fn native_link_configuration_rejects_invalid_descriptors() {
    let mut config = NativeLink {
        imports: vec![NativeImport::function("libc.so.6", "probe")],
        interpreter: None,
    };
    assert!(config.validate(TargetName::X86_64Linux).is_err());
    config.interpreter = Some("relative/ld.so".into());
    assert!(config.validate(TargetName::X86_64Linux).is_err());
    config.interpreter = Some("/lib/ld.so\0other".into());
    assert!(config.validate(TargetName::X86_64Linux).is_err());
    config.interpreter = Some("/lib/ld.so".into());
    assert!(config.validate(TargetName::X86_64Windows).is_err());
    assert!(config.validate(TargetName::X86_64Linux).is_ok());
    config
        .imports
        .push(NativeImport::function("other.so", "probe"));
    assert!(config.validate(TargetName::X86_64Linux).is_err());
}
