use super::{
    CompileRequest, Compiler, TargetName,
    harness::{Harness, put},
};
use gugu_runtime_world::hash::{HashFamily, HashOutput, Hasher};

fn compile(target: TargetName, source: &str) -> super::Compilation {
    let compiled =
        Compiler::shared().compile_to_lir(CompileRequest::single_file("main.gg", source, target));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    compiled
}

fn expected(text: &str, key: [u64; 2]) -> u64 {
    let mut hasher = Hasher::new();
    hasher.write_str(text);
    let HashOutput::Bits64(value) = hasher.finish(HashFamily::SipHash13 { key }) else {
        unreachable!()
    };
    value
}

#[test]
fn siphash13_matches_reference_at_word_and_tail_boundaries() {
    let compiled = compile(
        TargetName::X86_64Linux,
        include_str!("fixtures/secure_hashing.gg"),
    );
    let mut harness = Harness::new(&compiled);
    let mut memory = vec![0; 262144];
    for length in [0, 1, 7, 8, 9, 15, 16, 31, 64] {
        let text = "x".repeat(length);
        let key = [0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908];
        let at = harness.allocate(&mut memory, length);
        memory[at as usize..at as usize + length].copy_from_slice(text.as_bytes());
        let string = harness.allocate(&mut memory, 16);
        put(&mut memory, string, at);
        put(&mut memory, string + 8, length as u64);
        assert_eq!(
            harness.invoke("main.gg", "digest", &[key[0], key[1], string], &mut memory),
            [expected(&text, key)],
            "长度 {length}"
        );
    }
}

#[expect(clippy::ptr_arg, reason = "签名与原生边界测试钩子一致")]
fn entropy(harness: &mut Harness<'_>, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
    let (operation, bytes, error) = harness.script.pop_front().expect("明确声明熵请求");
    assert_eq!(args[0], operation);
    let (buffer, capacity) = match operation {
        318 => {
            assert_eq!(args[3], 0);
            (args[1], args[2])
        }
        22 => (args[1], args[2]),
        23 => {
            assert_eq!((args[1], args[4]), (0, 2));
            (args[2], args[3])
        }
        _ => panic!("未知熵协议"),
    };
    assert!(bytes <= capacity && capacity <= 16);
    harness.calls.push((operation, vec![buffer, capacity]));
    if error == 0 {
        memory[buffer as usize..(buffer + bytes) as usize].fill(7);
    }
    vec![
        if operation == 318 && error == 0 {
            bytes
        } else {
            0
        },
        error,
    ]
}

#[test]
fn secure_collections_share_identity_and_snapshot_their_contents() {
    let compiled = compile(
        TargetName::X86_64Linux,
        include_str!("fixtures/secure_collections.gg"),
    );
    let mut harness = Harness::new(&compiled);
    harness.bridge_hook = Some(entropy);
    harness.script.extend([(318, 16, 0), (318, 16, 0)]);
    let mut memory = vec![0; 262144];
    assert_eq!(
        harness.invoke("main.gg", "collections", &[], &mut memory),
        [1]
    );
    assert!(harness.script.is_empty());
    assert!(harness.allocations.is_empty());
    assert_eq!(harness.freed.len(), 2);
    for (buffer, bytes) in harness.freed {
        assert_eq!(bytes, 16);
        assert!(
            memory[buffer as usize..(buffer + bytes) as usize]
                .iter()
                .all(|byte| *byte == 0)
        );
    }
}

fn platform_entropy(target: TargetName, operation: u64) {
    let compiled = compile(target, include_str!("fixtures/secure_hashing.gg"));
    let mut harness = Harness::new(&compiled);
    harness.bridge_hook = Some(entropy);
    if operation != 23 {
        harness.script.push_back((operation, 16, 4));
    }
    harness.script.push_back((operation, 16, 0));
    let mut memory = vec![0; 262144];
    assert_eq!(
        harness.invoke("main.gg", "random_digest", &[], &mut memory),
        [expected("secure", [u64::from_le_bytes([7; 8]); 2])]
    );
    assert!(harness.script.is_empty());
    assert!(harness.allocations.is_empty());
}

#[test]
fn secure_hasher_reads_linux_entropy_and_retries_interruption() {
    platform_entropy(TargetName::X86_64Linux, 318);
}

#[test]
fn secure_hasher_reads_windows_entropy() {
    platform_entropy(TargetName::X86_64Windows, 23);
}

#[test]
fn secure_hasher_reads_darwin_entropy_and_retries_interruption() {
    platform_entropy(TargetName::X86_64Macos, 22);
}

#[test]
fn secure_hasher_completes_short_reads_and_rejects_os_failures() {
    let compiled = compile(
        TargetName::X86_64Linux,
        include_str!("fixtures/secure_hashing.gg"),
    );
    let mut harness = Harness::new(&compiled);
    harness.bridge_hook = Some(entropy);
    harness.script.extend([(318, 8, 0), (318, 8, 0)]);
    let mut memory = vec![0; 262144];
    assert_eq!(
        harness.invoke("main.gg", "random_digest", &[], &mut memory),
        [expected("secure", [u64::from_le_bytes([7; 8]); 2])]
    );
    assert_eq!(harness.calls[0].1[1], 16);
    assert_eq!(harness.calls[1].1, [harness.calls[0].1[0] + 8, 8]);
    assert!(harness.allocations.is_empty());

    let mut harness = Harness::new(&compiled);
    harness.bridge_hook = Some(entropy);
    harness.script.push_back((318, 16, 5));
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness.invoke("main.gg", "random_digest", &[], &mut memory)
    }))
    .expect_err("系统熵失败不得生成可用的固定密钥");
    let message = panic.downcast_ref::<String>().expect("解释器 panic 文本");
    assert!(message.contains("系统随机源不可用"), "{message}");
    assert!(harness.script.is_empty());
}
