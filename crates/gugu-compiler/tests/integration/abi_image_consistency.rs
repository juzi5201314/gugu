//! 双目标 ABI 与镜像一致性：同一组 FFI fixture 在 SysV 与 MS x64 后端
//! 各生成结构完整的 ELF/PE 镜像；导入符号、解释器、导出符号与 TLS/bridge
//! 模式在两个目标上按各自格式就位；strip 保留入口与运行时 metadata。

use gugu_compiler::{
    Compilation, CompileRequest, Compiler, EMBEDDED_ARTIFACTS, NativeImport, NativeLink, TargetName,
};

fn compile(source: &str, target: TargetName, symbols: &[&str]) -> (Compilation, Vec<u8>) {
    let library = if target == TargetName::X86_64Linux {
        "libprobe.so"
    } else {
        "probe.dll"
    };
    let mut imports: Vec<_> = symbols
        .iter()
        .map(|name| NativeImport::function(library, *name))
        .collect();
    if target == TargetName::X86_64Linux {
        // 普通 ForeignBridge 走宿主 runtime 的外部线程接入路径，需要 glibc 符号。
        imports.extend(
            NativeLink::THREAD_SYMBOLS
                .into_iter()
                .map(|name| NativeImport::function("libc.so.6", name)),
        );
    }
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_full_for_test(
        CompileRequest::single_file("main.gg", source, target).with_native_link(NativeLink {
            imports,
            interpreter: (target == TargetName::X86_64Linux).then(|| "/toolchain/ld.so".into()),
        }),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let bytes = {
        let plan = output.image_plan().unwrap();
        if target == TargetName::X86_64Linux {
            plan.linux_image().to_vec()
        } else {
            plan.windows_image().to_vec()
        }
    };
    (output, bytes)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn assert_elf(bytes: &[u8]) {
    assert_eq!(&bytes[0..4], b"\x7fELF", "ELF magic");
    assert_eq!(bytes[4], 2, "ELFCLASS64");
    assert_eq!(bytes[5], 1, "little-endian");
    let e_type = u16::from_le_bytes([bytes[16], bytes[17]]);
    assert!(e_type == 2 || e_type == 3, "ELF e_type={e_type}");
    let entry = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    assert_ne!(entry, 0, "ELF 入口必须非零");
    let phnum = u16::from_le_bytes([bytes[56], bytes[57]]);
    assert!(phnum > 0, "ELF 缺少 program header");
}

fn assert_pe(bytes: &[u8]) {
    assert_eq!(&bytes[0..2], b"MZ", "PE DOS magic");
    let lfanew = u32::from_le_bytes(bytes[60..64].try_into().unwrap()) as usize;
    assert_eq!(&bytes[lfanew..lfanew + 4], b"PE\0\0", "PE signature");
    // COFF header：machine 必须 AMD64。
    let machine = u16::from_le_bytes([bytes[lfanew + 4], bytes[lfanew + 5]]);
    assert_eq!(machine, 0x8664, "PE machine");
    // Optional header magic PE32+。
    let optional = lfanew + 24;
    let magic = u16::from_le_bytes([bytes[optional], bytes[optional + 1]]);
    assert_eq!(magic, 0x20b, "PE32+ magic");
    let entry = u32::from_le_bytes(bytes[optional + 16..optional + 20].try_into().unwrap());
    assert_ne!(entry, 0, "PE AddressOfEntryPoint 必须非零");
}

/// pin/bridge/leaf/dirty 四种调用模式与 TLS 槽在双目标上均产出合法镜像，
/// 导入库名、导入符号与导出符号按目标格式登记；strip 保留入口与运行时 metadata；
/// 双目标输出具有不同的 action key 与镜像格式字节。
#[test]
fn ffi_modes_images_strip_and_dual_target_distinct() {
    let symbols = ["leaf_probe", "dirty_probe", "native_probe"];
    let source = include_str!("../fixtures/ffi_modes.gg");
    let ((linux, linux_bytes), (windows, windows_bytes)) = std::thread::scope(|scope| {
        let linux = scope.spawn(|| compile(source, TargetName::X86_64Linux, &symbols));
        let windows = compile(source, TargetName::X86_64Windows, &symbols);
        (linux.join().expect("linux compile thread"), windows)
    });

    assert_ne!(linux.action_key(), windows.action_key());
    assert_eq!(&linux_bytes[0..4], b"\x7fELF");
    assert_eq!(&windows_bytes[0..2], b"MZ");
    assert!(!contains(&linux_bytes, b"probe.dll\0"));
    assert!(!contains(&windows_bytes, b"libprobe.so"));

    for (target, bytes) in [
        (TargetName::X86_64Linux, &linux_bytes),
        (TargetName::X86_64Windows, &windows_bytes),
    ] {
        match target {
            TargetName::X86_64Linux => {
                assert_elf(bytes);
                assert!(contains(bytes, b"/toolchain/ld.so"), "PT_INTERP");
                assert!(contains(bytes, b"libprobe.so"), "DT_NEEDED");
            }
            _ => {
                assert_pe(bytes);
                assert!(contains(bytes, b"probe.dll"), "import dll");
            }
        }
        for symbol in symbols {
            let mut name = symbol.as_bytes().to_vec();
            name.push(0);
            assert!(contains(bytes, &name), "{target:?} 缺导入 {symbol}");
        }
        assert!(
            contains(bytes, b"abi_probe_export"),
            "{target:?} 缺导出符号"
        );
        assert!(
            contains(bytes, b"tls corrupt"),
            "{target:?} 缺 panic 字符串"
        );

        let backend_target =
            gugu_base::target::TargetName::parse(target.name()).expect("目标名必须可映射");
        let stripped =
            gugu_backend::x64::strip::strip_image(backend_target, bytes).expect("strip 必须成功");
        match target {
            TargetName::X86_64Linux => assert_elf(&stripped.bytes),
            _ => assert_pe(&stripped.bytes),
        }
        let library = if target == TargetName::X86_64Linux {
            &b"libprobe.so"[..]
        } else {
            b"probe.dll"
        };
        assert!(
            contains(&stripped.bytes, library),
            "{target:?} strip 丢库名"
        );
        for symbol in symbols {
            let mut name = symbol.as_bytes().to_vec();
            name.push(0);
            assert!(
                contains(&stripped.bytes, &name),
                "{target:?} strip 丢导入 {symbol}"
            );
        }
        assert!(
            contains(&stripped.bytes, b"tls corrupt"),
            "{target:?} strip 丢运行时字符串"
        );
    }
}

fn assert_aggregate_by_value_abi(target: TargetName) {
    let symbols = ["native_pair", "native_wide", "native_bits"];
    let (_output, bytes) = compile(include_str!("../fixtures/ffi_abi.gg"), target, &symbols);
    match target {
        TargetName::X86_64Linux => assert_elf(&bytes),
        _ => assert_pe(&bytes),
    }
    for symbol in symbols {
        let mut name = symbol.as_bytes().to_vec();
        name.push(0);
        assert!(contains(&bytes, &name), "{target:?} 缺导入 {symbol}");
    }
}

/// repr(C)/packed/transparent/enum 聚合按值穿越 extern 边界：两目标均生成镜像。
#[test]
fn aggregate_by_value_abi_compiles_for_linux() {
    assert_aggregate_by_value_abi(TargetName::X86_64Linux);
}

#[test]
fn aggregate_by_value_abi_compiles_for_windows() {
    assert_aggregate_by_value_abi(TargetName::X86_64Windows);
}
