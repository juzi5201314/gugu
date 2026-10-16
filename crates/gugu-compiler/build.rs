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
    fs::create_dir_all(&prebuilt_dir).expect("创建 prebuilt 目录");
    for (target, artifact) in artifacts {
        let bytes = artifact.encode().expect("runtime artifact encode");
        let decoded =
            gugu_compiler_core::RuntimeArtifactV1::decode(&bytes).expect("runtime artifact decode");
        decoded.for_target(target).expect("runtime artifact target");
        let name = match target {
            gugu_compiler_core::TargetName::X86_64Linux => "runtime-x86_64-linux.gbc",
            gugu_compiler_core::TargetName::X86_64Windows => "runtime-x86_64-windows.gbc",
        };
        fs::write(out_dir.join(name), &bytes).expect("write runtime artifact");
        fs::write(prebuilt_dir.join(name), &bytes).expect("write prebuilt runtime artifact");
    }
}
