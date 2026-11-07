//! 保持目录 control 跨调用存活；仅 getdents/FindNext 使用脚本。
use super::*;
const CONTROL: u64 = 4096;
const SECOND_OUTPUT: u64 = 2048;
fn put(memory: &mut [u8], at: u64, value: u64) {
    memory[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
}
fn word(memory: &[u8], at: u64) -> u64 {
    u64::from_le_bytes(memory[at as usize..at as usize + 8].try_into().unwrap())
}
fn next(
    adapter: &mut Adapter<'_>,
    module: &str,
    name: &str,
    memory: &mut Vec<u8>,
    out: u64,
    capacity: u64,
) -> Vec<u64> {
    let body = runtime_body(adapter.output, module, name);
    let mut args = vec![CONTROL, out, capacity];
    if body.signature.sret.is_some() {
        args.insert(0, 0);
    }
    let result = adapter.evaluate(body, &args, memory);
    if body.signature.sret.is_some() {
        vec![word(memory, 0), word(memory, 8)]
    } else {
        result
    }
}
fn linux_entry(name: &[u8], kind: u8) -> Vec<u8> {
    let size = (20 + name.len()).div_ceil(8) * 8;
    let mut bytes = vec![0; size];
    bytes[16..18].copy_from_slice(&(size as u16).to_le_bytes());
    bytes[18] = kind;
    bytes[19..19 + name.len()].copy_from_slice(name);
    bytes
}
fn windows_entry(name: &str, attributes: u32) -> Vec<u8> {
    let mut bytes = vec![0; 592];
    bytes[..4].copy_from_slice(&attributes.to_le_bytes());
    for (index, unit) in name.encode_utf16().enumerate() {
        bytes[44 + index * 2..46 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[test]
fn linux_directory_retries_same_record_and_copies_names_before_refill() {
    let output = compile(TargetName::X86_64Linux);
    let first = linux_entry(b"one", 8);
    let mut batch = first.clone();
    batch.extend(linux_entry(&[0xff, b'x'], 10));
    let refill = linux_entry(b"dir", 4);
    let mut adapter = Adapter::new(
        &output,
        vec![
            Reply::new(217, batch.len() as u64, 0)
                .argument(1, 77)
                .argument(3, 4000)
                .write(2, batch),
            Reply::new(217, 0, 13),
            Reply::new(217, refill.len() as u64, 0).write(2, refill),
            Reply::new(217, 0, 0),
        ],
    );
    let mut memory = vec![0; 65536];
    put(&mut memory, CONTROL, 77);
    memory[OUTPUT as usize..OUTPUT as usize + 32].fill(0xa5);
    assert_eq!(
        next(
            &mut adapter,
            "fs_platform",
            "linux_dir_next",
            &mut memory,
            OUTPUT,
            18
        ),
        [0, 36]
    );
    assert_eq!(&memory[OUTPUT as usize..OUTPUT as usize + 32], &[0xa5; 32]);
    assert_eq!(word(&memory, CONTROL + 8), 0);
    assert_eq!(
        next(
            &mut adapter,
            "fs_platform",
            "linux_dir_next",
            &mut memory,
            OUTPUT,
            32
        ),
        [3, 0]
    );
    assert_eq!(word(&memory, OUTPUT), 1);
    assert_eq!(word(&memory, CONTROL + 8), first.len() as u64);
    let snapshot = memory[OUTPUT as usize..OUTPUT as usize + 19].to_vec();
    assert_eq!(
        next(
            &mut adapter,
            "fs_platform",
            "linux_dir_next",
            &mut memory,
            SECOND_OUTPUT,
            32
        ),
        [2, 0]
    );
    assert_eq!(word(&memory, SECOND_OUTPUT), 3);
    assert_eq!(
        &memory[SECOND_OUTPUT as usize + 16..SECOND_OUTPUT as usize + 18],
        &[0xff, b'x']
    );
    let previous = memory[SECOND_OUTPUT as usize..SECOND_OUTPUT as usize + 18].to_vec();
    assert_eq!(
        next(
            &mut adapter,
            "fs_platform",
            "linux_dir_next",
            &mut memory,
            SECOND_OUTPUT,
            32
        ),
        [0, 13]
    );
    assert_eq!(
        &memory[SECOND_OUTPUT as usize..SECOND_OUTPUT as usize + 18],
        previous
    );
    assert_eq!(
        next(
            &mut adapter,
            "fs_platform",
            "linux_dir_next",
            &mut memory,
            SECOND_OUTPUT,
            32
        ),
        [3, 0]
    );
    assert_eq!(word(&memory, SECOND_OUTPUT), 2);
    assert_eq!(&memory[OUTPUT as usize..OUTPUT as usize + 19], snapshot);
    assert_eq!(
        next(
            &mut adapter,
            "fs_platform",
            "linux_dir_next",
            &mut memory,
            SECOND_OUTPUT,
            32
        ),
        [0, 0]
    );
    assert!(adapter.replies.is_empty() && adapter.live.is_empty());
}

#[test]
fn windows_directory_keeps_unconsumed_entry_after_small_buffer() {
    let output = compile(TargetName::X86_64Windows);
    let first = windows_entry("one", 0);
    let second = windows_entry("目录", 1024 | 16);
    let mut adapter = Adapter::new(
        &output,
        vec![
            Reply::new(10, 1, 0).argument(1, 77).write(2, second),
            Reply::new(10, 0, 5),
            Reply::new(10, 0, 18),
        ],
    );
    let mut memory = vec![0; 65536];
    put(&mut memory, CONTROL, 77);
    put(&mut memory, CONTROL + 8, 1);
    memory[CONTROL as usize + 64..CONTROL as usize + 64 + first.len()].copy_from_slice(&first);
    memory[OUTPUT as usize..OUTPUT as usize + 32].fill(0xa5);
    assert_eq!(
        next(
            &mut adapter,
            "fs_windows",
            "dir_next",
            &mut memory,
            OUTPUT,
            20
        ),
        [0, 122]
    );
    assert_eq!(&memory[OUTPUT as usize..OUTPUT as usize + 32], &[0xa5; 32]);
    assert_eq!(word(&memory, CONTROL + 8), 1, "缓冲不足不得消耗首项");
    assert_eq!(
        next(
            &mut adapter,
            "fs_windows",
            "dir_next",
            &mut memory,
            OUTPUT,
            32
        ),
        [6, 0]
    );
    assert_eq!(
        &memory[OUTPUT as usize + 16..OUTPUT as usize + 22],
        &[b'o', 0, b'n', 0, b'e', 0]
    );
    let snapshot = memory[OUTPUT as usize..OUTPUT as usize + 22].to_vec();
    assert_eq!(
        next(
            &mut adapter,
            "fs_windows",
            "dir_next",
            &mut memory,
            SECOND_OUTPUT,
            18
        ),
        [0, 122]
    );
    assert_eq!(word(&memory, CONTROL + 8), 1, "后续项同样保留供重试");
    assert_eq!(
        next(
            &mut adapter,
            "fs_windows",
            "dir_next",
            &mut memory,
            SECOND_OUTPUT,
            32
        ),
        [4, 0]
    );
    assert_eq!(
        word(&memory, SECOND_OUTPUT),
        3,
        "reparse 优先于 directory 属性"
    );
    assert_eq!(&memory[OUTPUT as usize..OUTPUT as usize + 22], snapshot);
    let previous = memory[SECOND_OUTPUT as usize..SECOND_OUTPUT as usize + 20].to_vec();
    assert_eq!(
        next(
            &mut adapter,
            "fs_windows",
            "dir_next",
            &mut memory,
            SECOND_OUTPUT,
            32
        ),
        [0, 5]
    );
    assert_eq!(
        &memory[SECOND_OUTPUT as usize..SECOND_OUTPUT as usize + 20],
        previous
    );
    assert_eq!(
        next(
            &mut adapter,
            "fs_windows",
            "dir_next",
            &mut memory,
            SECOND_OUTPUT,
            32
        ),
        [0, 0]
    );
    assert!(adapter.replies.is_empty() && adapter.live.is_empty());
}
