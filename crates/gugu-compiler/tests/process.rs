use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};

fn compile(source: &str, target: TargetName) -> gugu_compiler::Compilation {
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS)
        .compile_full_for_test(CompileRequest::single_file("main.gg", source, target));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

#[test]
fn macos_process_image_imports_spawn_and_wait_from_libsystem() {
    let output = compile(
        include_str!("fixtures/process_output.gg"),
        TargetName::X86_64Macos,
    );
    let image = output.image_plan().unwrap().macos_image();
    for name in [b"_posix_spawn\0".as_slice(), b"_waitpid\0", b"_pipe\0"] {
        assert!(image.windows(name.len()).any(|value| value == name));
    }
}

#[test]
fn linux_process_image_imports_environment_as_data_not_a_plt_function() {
    let output = compile(
        include_str!("fixtures/process_env.gg"),
        TargetName::X86_64Linux,
    );
    let image = output.image_plan().unwrap();
    let bytes = image.linux_image();
    let phoff = usize::try_from(u64::from_le_bytes(bytes[32..40].try_into().unwrap())).unwrap();
    let stride = usize::from(u16::from_le_bytes(bytes[54..56].try_into().unwrap()));
    let count = usize::from(u16::from_le_bytes(bytes[56..58].try_into().unwrap()));
    let headers: Vec<_> = (0..count)
        .map(|index| &bytes[phoff + index * stride..][..stride])
        .collect();
    let dynamic = headers
        .iter()
        .find(|header| u32::from_le_bytes(header[..4].try_into().unwrap()) == 2)
        .unwrap();
    let offset = |value: u64| {
        let header = headers
            .iter()
            .find(|header| {
                let address = u64::from_le_bytes(header[16..24].try_into().unwrap());
                let size = u64::from_le_bytes(header[32..40].try_into().unwrap());
                u32::from_le_bytes(header[..4].try_into().unwrap()) == 1
                    && value >= address
                    && value - address < size
            })
            .unwrap();
        usize::try_from(
            value - u64::from_le_bytes(header[16..24].try_into().unwrap())
                + u64::from_le_bytes(header[8..16].try_into().unwrap()),
        )
        .unwrap()
    };
    let start = usize::try_from(u64::from_le_bytes(dynamic[8..16].try_into().unwrap())).unwrap();
    let entries: Vec<_> = bytes[start..]
        .as_chunks::<16>()
        .0
        .iter()
        .take_while(|entry| entry[..8] != [0; 8])
        .collect();
    let tag = |key: u64| {
        u64::from_le_bytes(
            entries
                .iter()
                .find(|entry| entry[..8] == key.to_le_bytes())
                .unwrap()[8..16]
                .try_into()
                .unwrap(),
        )
    };
    let symbol_count = usize::try_from(u32::from_le_bytes(
        bytes[offset(tag(4)) + 4..offset(tag(4)) + 8]
            .try_into()
            .unwrap(),
    ))
    .unwrap();
    let symbols = &bytes[offset(tag(6))..][..symbol_count * 24];
    let strings = &bytes[offset(tag(5))..];
    let index = symbols
        .as_chunks::<24>()
        .0
        .iter()
        .position(|symbol| {
            let name =
                usize::try_from(u32::from_le_bytes(symbol[..4].try_into().unwrap())).unwrap();
            strings[name..].starts_with(b"environ\0")
        })
        .expect("宿主 environ 数据符号");
    assert_eq!(symbols[index * 24 + 4] & 15, 1, "environ 必须是 STT_OBJECT");
    let relocations: Vec<_> = bytes[offset(tag(7))..][..usize::try_from(tag(8)).unwrap()]
        .as_chunks::<24>()
        .0
        .iter()
        .map(|entry| u64::from_le_bytes(entry[8..16].try_into().unwrap()))
        .filter(|info| *info >> 32 == u64::try_from(index).unwrap())
        .collect();
    assert_eq!(
        relocations,
        [u64::try_from(index).unwrap() << 32 | 6],
        "environ 只能使用 R_X86_64_GLOB_DAT"
    );
    assert!(
        bytes
            .windows(b"libc.so.6\0".len())
            .any(|value| value == b"libc.so.6\0")
    );
}

#[test]
fn windows_process_image_uses_explicit_system_imports_without_crt() {
    let output = compile(
        include_str!("fixtures/process_protocol.gg"),
        TargetName::X86_64Windows,
    );
    let bytes = output.image_plan().unwrap().windows_image();
    for name in [
        "CreateProcessW",
        "CreateNamedPipeW",
        "GetOverlappedResult",
        "GetEnvironmentStringsW",
    ] {
        assert!(
            bytes
                .windows(name.len())
                .any(|value| value == name.as_bytes()),
            "{name}"
        );
    }
    for crt in ["msvcrt", "ucrtbase"] {
        assert!(
            !bytes
                .windows(crt.len())
                .any(|value| value == crt.as_bytes())
        );
    }
}

#[test]
fn startup_context_does_not_make_linux_images_dynamic() {
    let output = compile(
        "use std.env\nfn main() { _ = env.args()\n _ = env.current_dir() }",
        TargetName::X86_64Linux,
    );
    let bytes = output.image_plan().unwrap().linux_image();
    let phoff = usize::try_from(u64::from_le_bytes(bytes[32..40].try_into().unwrap())).unwrap();
    let stride = usize::from(u16::from_le_bytes(bytes[54..56].try_into().unwrap()));
    let count = usize::from(u16::from_le_bytes(bytes[56..58].try_into().unwrap()));
    for index in 0..count {
        let at = phoff + index * stride;
        assert_ne!(
            u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
            3,
            "args/cwd-only 不得引入 PT_INTERP"
        );
    }
}
