use super::*;
use crate::runtime::RuntimeMachineOp;

pub(super) fn setup(h: &mut Harness<'_>, m: &mut Vec<u8>) {
    h.machine_hook = Some(|_, op, args, m| match op {
        RuntimeMachineOp::StartupCall => {
            let target = usize::try_from(word(m, args[1] + 8)).unwrap();
            m[target..target + 5].copy_from_slice(b"/one\0");
            vec![0]
        }
        RuntimeMachineOp::EnvironmentSymbol => vec![2304],
        RuntimeMachineOp::ProcessWorker => vec![200],
        _ => panic!("未登记边界 {op:?}"),
    });
    store(m, 1024, &[2048, 0]);
    m[2048..2051].copy_from_slice(&[255, 65, 0]);
    store(m, 2304, &[2312, 2336, 2352, 0]);
    m[2336..2343].copy_from_slice(b"A=host\0");
    m[2352..2358].copy_from_slice(b"B=old\0");
    h.invoke(CONTEXT, "boot", &[1, 1024], m);
}
fn text(m: &[u8], at: u64) -> &[u8] {
    let rest = &m[usize::try_from(at).unwrap()..];
    &rest[..rest.iter().position(|&byte| byte == 0).unwrap()]
}

#[test]
fn process_snapshots_preserve_arguments_environment_and_virtual_paths() {
    let output = protocol_source(
        TargetName::X86_64Linux,
        include_str!("../../../../gugu-compiler/tests/fixtures/process_snapshot.gg"),
    );
    args_copy_preserves_native_bytes_and_each_returned_vector_is_independent(&output);
    spawn_captures_environment_and_cwd_before_stdio_and_builder_overrides_host(&output);
    virtual_cwd_failure_and_recursive_operations_preserve_captured_paths(&output);
    directory_entry_metadata_keeps_its_original_absolute_root(&output);
}

fn args_copy_preserves_native_bytes_and_each_returned_vector_is_independent(output: &Protocol) {
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    setup(&mut h, &mut m);
    m[2048..2051].copy_from_slice(b"XX\0");
    assert_eq!(h.invoke("main.gg", "args_snapshot", &[], &mut m), [1]);
    h.invoke(CONTEXT, "shutdown", &[], &mut m);
    assert!(h.allocations.is_empty());
}
fn spawn_captures_environment_and_cwd_before_stdio_and_builder_overrides_host(output: &Protocol) {
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    setup(&mut h, &mut m);
    h.bridge_hook = Some(|h, args, m| {
        let packet = args[1];
        assert_eq!(word(m, packet), 200);
        let request = word(m, packet + 16);
        let op = word(m, request);
        if op == 3 && h.now == 1000 {
            h.now = 1001;
            m[2352..2358].copy_from_slice(b"B=new\0");
            store(m, 3000, &[3024, 4]);
            m[3024..3028].copy_from_slice(b"/two");
            h.invoke(CONTEXT, "replace_cwd", &[3000], m);
        }
        if op == 0 {
            let config = word(m, request + 8);
            let (cwd, executable, environment) = if h.now == 1001 {
                (&b"/one"[..], &b"/one/child"[..], &b"B=old"[..])
            } else {
                (&b"/two"[..], &b"/two/child"[..], &b"B=new"[..])
            };
            assert_eq!(text(m, word(m, config)), executable);
            assert_eq!(text(m, word(m, config + 24)), cwd);
            let argv = word(m, config + 8);
            assert_eq!(text(m, word(m, argv)), executable);
            assert_eq!(text(m, word(m, argv + 8)), b"$();");
            assert_eq!(word(m, argv + 16), 0);
            let env = word(m, config + 16);
            assert_eq!(text(m, word(m, env)), environment);
            assert_eq!(text(m, word(m, env + 8)), b"A=builder");
            assert_eq!(word(m, env + 16), 0);
            h.now = 1002;
        }
        let (expected, result, error) = h.script.pop_front().unwrap();
        assert_eq!(op, expected);
        put(m, request + 72, result);
        put(m, request + 80, error);
        vec![0, 0]
    });
    for _ in 0..2 {
        h.script.extend([
            (3, 71, 0),
            (3, 72, 0),
            (3, 73, 0),
            (0, 0, 2),
            (2, 0, 0),
            (2, 0, 0),
            (2, 0, 0),
        ]);
        assert_eq!(h.invoke("main.gg", "spawn_snapshot", &[], &mut m), [1]);
        assert!(h.script.is_empty());
    }
    assert_eq!(
        h.calls.iter().map(|(_, args)| args[1]).collect::<Vec<_>>(),
        [73, 72, 71, 73, 72, 71]
    );
    h.invoke(CONTEXT, "shutdown", &[], &mut m);
    assert!(h.allocations.is_empty());
}

fn virtual_cwd_failure_and_recursive_operations_preserve_captured_paths(output: &Protocol) {
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    setup(&mut h, &mut m);
    h.bridge_hook = Some(|h, args, m| {
        h.calls.push((args[0], args.to_vec()));
        match args[0] {
            262 if text(m, args[2]) == b"/one/missing" => vec![0, 2],
            262 if text(m, args[2]) == b"/one/file" => {
                put(m, args[3] + 24, 32768);
                vec![0, 0]
            }
            88 => {
                assert_eq!(text(m, args[1]), b"../target");
                assert_eq!(text(m, args[2]), b"/one/link");
                vec![0, 0]
            }
            262 => {
                assert_eq!(text(m, args[2]), b"/one/tree");
                put(m, args[3] + 24, 16384);
                store(m, 3000, &[3024, 4]);
                m[3024..3028].copy_from_slice(b"/two");
                h.invoke(CONTEXT, "replace_cwd", &[3000], m);
                vec![0, 0]
            }
            2 => {
                assert_eq!(text(m, args[1]), b"/one/tree");
                vec![0, 13]
            }
            other => panic!("不应调用额外平台操作 {other}"),
        }
    });
    assert_eq!(h.invoke("main.gg", "invalid_cwd", &[], &mut m), [1]);
    assert_eq!(h.invoke("main.gg", "symlink_snapshot", &[], &mut m), [1]);
    assert_eq!(h.invoke("main.gg", "recursive_snapshot", &[], &mut m), [1]);
    assert_eq!(
        h.calls.iter().map(|(op, _)| *op).collect::<Vec<_>>(),
        [262, 262, 88, 262, 2]
    );
    h.invoke(CONTEXT, "shutdown", &[], &mut m);
    assert!(h.allocations.is_empty());
}

fn directory_entry_metadata_keeps_its_original_absolute_root(output: &Protocol) {
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    setup(&mut h, &mut m);
    h.bridge_hook = Some(|h, args, m| {
        h.calls.push((args[0], args.to_vec()));
        match args[0] {
            2 => {
                assert_eq!(text(m, args[1]), b"/one/directory");
                vec![71, 0]
            }
            217 => {
                assert_eq!(args[1], 71);
                let at = usize::try_from(args[2]).unwrap();
                m[at..at + 24].fill(0);
                m[at + 16..at + 18].copy_from_slice(&24u16.to_le_bytes());
                m[at + 18] = 8;
                m[at + 19..at + 24].copy_from_slice(b"item\0");
                store(m, 3000, &[3024, 4]);
                m[3024..3028].copy_from_slice(b"/two");
                h.invoke(CONTEXT, "replace_cwd", &[3000], m);
                vec![24, 0]
            }
            262 => {
                assert_eq!(text(m, args[2]), b"/one/directory/item");
                vec![0, 2]
            }
            other => panic!("未登记目录 syscall {other}"),
        }
    });
    assert_eq!(h.invoke("main.gg", "directory_snapshot", &[], &mut m), [1]);
    assert_eq!(
        h.calls.iter().map(|(op, _)| *op).collect::<Vec<_>>(),
        [2, 217, 262]
    );
}
