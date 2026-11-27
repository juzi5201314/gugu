use std::{env, fs, path::PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo 提供 OUT_DIR"));
    // 前端不嵌入 artifact，它的测试要读同一份字节做端到端验证；落盘位置用仓库相对的
    // `target/prebuilt`，与前端测试 helper 的路径一致，不依赖 cargo 的 build 目录层级。
    let prebuilt_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo 提供 CARGO_MANIFEST_DIR"))
            .join("../../target/prebuilt");
    for path in [
        "../gugu-frontend/resources/std",
        "../gugu-frontend/resources/runtime",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-changed=../gugu-compiler-core/src/lib.rs");
    println!("cargo:rerun-if-changed=../gugu-runtime-builder/src/lib.rs");

    let artifacts = gugu_runtime_builder::build_all().expect("runtime artifact build/verify");
    let artifacts = artifacts.into_iter().chain(
        [
            gugu_compiler_core::TargetName::X86_64Macos,
            gugu_compiler_core::TargetName::Aarch64Linux,
            gugu_compiler_core::TargetName::Aarch64Macos,
        ]
        .into_iter()
        .map(|target| {
            (
                target,
                gugu_runtime_builder::build(target).expect("internal artifact build/verify"),
            )
        }),
    );
    fs::create_dir_all(&prebuilt_dir).expect("创建 prebuilt 目录");
    // 探针源只随 `runtime-gc-probes` 进入 artifact：文件名与嵌入路径都带上风味，两种构建互不覆盖。
    let flavor = if cfg!(feature = "runtime-gc-probes") {
        "-probes"
    } else {
        ""
    };
    println!("cargo:rustc-env=GUGU_PREBUILT_FLAVOR={flavor}");
    for (target, artifact) in artifacts {
        let artifact =
            gugu_frontend::materialize(frontend_target(target), artifact).expect("内建块物化");
        let bytes = bytes::Bytes::from(artifact.encode().expect("runtime artifact encode"));
        let decoded = gugu_compiler_core::RuntimeArtifactV1::decode(bytes.clone())
            .expect("runtime artifact decode");
        decoded.for_target(target).expect("runtime artifact target");
        let name = match target {
            gugu_compiler_core::TargetName::X86_64Linux => "runtime-x86_64-linux",
            gugu_compiler_core::TargetName::X86_64Windows => "runtime-x86_64-windows",
            gugu_compiler_core::TargetName::X86_64Macos => "runtime-x86_64-macos",
            gugu_compiler_core::TargetName::Aarch64Linux => "runtime-aarch64-linux",
            gugu_compiler_core::TargetName::Aarch64Macos => "runtime-aarch64-macos",
        };
        let name = format!("{name}{flavor}.gbc");
        fs::write(out_dir.join(&name), &bytes).expect("write runtime artifact");
        fs::write(prebuilt_dir.join(&name), &bytes).expect("write prebuilt runtime artifact");
    }
}

/// build-time 目标在 frontend 侧的名字。
fn frontend_target(target: gugu_compiler_core::TargetName) -> gugu_frontend::TargetName {
    match target {
        gugu_compiler_core::TargetName::X86_64Linux => gugu_frontend::TargetName::X86_64Linux,
        gugu_compiler_core::TargetName::X86_64Windows => gugu_frontend::TargetName::X86_64Windows,
        gugu_compiler_core::TargetName::X86_64Macos => gugu_frontend::TargetName::X86_64Macos,
        gugu_compiler_core::TargetName::Aarch64Linux => gugu_frontend::TargetName::Aarch64Linux,
        gugu_compiler_core::TargetName::Aarch64Macos => gugu_frontend::TargetName::Aarch64Macos,
    }
}
