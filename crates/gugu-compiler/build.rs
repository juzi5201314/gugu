use std::{env, fs, path::PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo 提供 OUT_DIR"));
    for path in [
        "../gugu-compiler/resources/std",
        "../gugu-compiler/resources/runtime",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-changed=../gugu-compiler-core/src/lib.rs");
    println!("cargo:rerun-if-changed=../gugu-runtime-builder/src/lib.rs");

    let artifacts = gugu_runtime_builder::build_all().expect("runtime artifact build/verify");
    for (target, artifact) in artifacts {
        let bytes = artifact.encode().expect("runtime artifact encode");
        let decoded =
            gugu_compiler_core::RuntimeArtifactV1::decode(&bytes).expect("runtime artifact decode");
        decoded.for_target(target).expect("runtime artifact target");
        let name = match target {
            gugu_compiler_core::TargetName::X86_64Linux => "runtime-x86_64-linux.gbc",
            gugu_compiler_core::TargetName::X86_64Windows => "runtime-x86_64-windows.gbc",
        };
        fs::write(out_dir.join(name), bytes).expect("write runtime artifact");
    }
}
