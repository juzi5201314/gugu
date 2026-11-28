//! 重复帧内的判别值必须按元素地址读取；旧实现从对象起点取 tag，
//! 会把 `None` 元素的 payload 当作直接指针交给搬运器。
use super::harness::{put, runtime_bodies};
use super::resource::ResourceHarness;

const TRACE: &str = "trace";
const DESCRIPTOR: u64 = 64;
const PAYLOAD: u64 = 1024;
const CURSOR: u64 = 8192;

fn program() -> Vec<u8> {
    // Repeat(base=1 word, count=2, stride=3 words) 包住 Option 的 Switch：
    // 只有 Some（tag=0）的 payload 字是直接指针。
    let mut body = vec![5, 0, 1, 2];
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&4u32.to_le_bytes());
    body.extend_from_slice(&[1, 1, 1, 0]);
    body.extend_from_slice(&1u64.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.push(0);
    body.extend_from_slice(&1u32.to_le_bytes());
    body.push(0);
    body.push(0);
    let mut program = vec![3, 1, 2, 3];
    program.extend_from_slice(
        &u32::try_from(body.len())
            .expect("descriptor 长度适配 u32")
            .to_le_bytes(),
    );
    program.extend_from_slice(&body);
    program.push(0);
    program
}

#[test]
fn nested_switch_reads_element_tag() {
    let (hir, bodies) = runtime_bodies();
    let mut harness = ResourceHarness::from_bodies(hir, bodies);
    let mut memory = vec![0u8; 65536];
    let program = program();
    let mut descriptor = vec![2];
    let program_len = u32::try_from(program.len()).expect("descriptor 长度适配 u32");
    descriptor.extend_from_slice(&program_len.to_le_bytes());
    descriptor.extend_from_slice(&program);
    let descriptor_at = usize::try_from(DESCRIPTOR).expect("descriptor 地址适配 usize");
    memory[descriptor_at..descriptor_at + descriptor.len()].copy_from_slice(&descriptor);
    // 元素 0 是 Some，payload 字是直接指针；元素 1 是 None，payload 里的 8 不是指针。
    put(&mut memory, PAYLOAD + 16, 0x1234);
    memory[usize::try_from(PAYLOAD + 32).expect("payload 地址适配 usize")] = 1;
    put(&mut memory, PAYLOAD + 40, 8);
    harness.run(TRACE, "trace_begin", &[CURSOR, 64], &mut memory);
    let descriptor_len = u64::try_from(descriptor.len()).expect("descriptor 长度适配 u64");
    assert_eq!(
        harness.run(
            TRACE,
            "trace_open",
            &[CURSOR, DESCRIPTOR, descriptor_len],
            &mut memory
        )[0],
        0
    );
    let mut visits = vec![];
    for _ in 0..8 {
        let status = harness.run(
            TRACE,
            "trace_advance",
            &[CURSOR, DESCRIPTOR, descriptor_len, PAYLOAD, 48],
            &mut memory,
        )[0];
        if status == 2 {
            break;
        }
        assert_eq!(status, 0, "固定 descriptor 不应耗尽额度");
        visits.push((
            harness.run(TRACE, "trace_last_word", &[CURSOR], &mut memory)[0],
            harness.run(TRACE, "trace_last_op", &[CURSOR], &mut memory)[0],
        ));
    }
    assert_eq!(visits, vec![(2, 1)], "只有 Some 元素的 word 1 是直接指针");
}
