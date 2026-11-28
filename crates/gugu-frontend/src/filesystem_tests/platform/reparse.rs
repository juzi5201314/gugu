//! 固定 reparse 记录；真实解析器必须以返回字节数和记录长度共同约束名称。
use super::*;

fn record(tag: u32, data_bytes: u16, offset: u16, length: u16) -> Vec<u8> {
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(&tag.to_le_bytes());
    bytes[4..6].copy_from_slice(&data_bytes.to_le_bytes());
    bytes[8..10].copy_from_slice(&offset.to_le_bytes());
    bytes[10..12].copy_from_slice(&length.to_le_bytes());
    let base = if tag == 0xa0000003 { 16 } else { 20 };
    bytes[base + 2..base + 6].copy_from_slice(&[b'a', 0, b'b', 0]);
    bytes
}

pub(super) fn windows_read_link_bounds_native_record_and_preserves_output_on_failure(
    output: &Compilation,
) {
    // 标签、数据长度、名称偏移/长度、实际返回字节数、输出容量、原生错误、预期错误。
    for (tag, data, offset, length, returned, capacity, native_error, error) in [
        (0xa000000c, 18, 2, 4, 26u32, 64, 0, 0),
        (0xa0000003, 14, 2, 4, 22, 64, 0, 0),
        (0xa000000c, 18, 2, 4, 26, 2, 0, 0),
        (0xa000000c, 18, 2, 4, 7, 64, 0, 13),
        (0xa000000c, 18, 2, 4, 16385, 64, 0, 13),
        (0xa000000c, 18, 2, 4, 24, 64, 0, 13),
        (0xa000000c, 10, 0, 0, 18, 64, 0, 13),
        (0xa0000003, 6, 0, 0, 14, 64, 0, 13),
        (0xa000000c, 16, 2, 4, 26, 64, 0, 13),
        (0xa000000c, 18, 1, 4, 26, 64, 0, 13),
        (0xa000000c, 18, 2, 3, 26, 64, 0, 13),
        (0xa000000c, 18, 65534, 4, 26, 64, 0, 13),
        (0xa000000c, 18, 2, 4, 0, 64, 5, 5),
        (0x12345678, 18, 2, 4, 26, 64, 0, 87),
    ] {
        let mut adapter = Adapter::new(
            output,
            vec![
                Reply::new(0, 42, 0)
                    .argument(1, INPUT)
                    .argument(6, 35651584),
                Reply::new(18, 1, native_error)
                    .argument(1, 42)
                    .argument(2, 589992)
                    .argument(6, 16384)
                    .write(5, record(tag, data, offset, length))
                    .write(7, returned.to_le_bytes().to_vec()),
                Reply::new(1, 1, 0).argument(1, 42),
            ],
        );
        let (result, memory) = adapter.run("fs_windows", "read_link", &[INPUT, OUTPUT, capacity]);
        assert_eq!(
            result,
            if error == 0 {
                vec![4, 0]
            } else {
                vec![if native_error != 0 { 1 } else { 0 }, error]
            },
            "tag={tag:x}, data={data}, offset={offset}, length={length}, returned={returned}"
        );
        if error == 0 && capacity >= 4 {
            assert_eq!(
                &memory[OUTPUT as usize..OUTPUT as usize + 4],
                &[b'a', 0, b'b', 0]
            );
            assert_eq!(memory[OUTPUT as usize + 4], 0xa5);
        } else {
            assert_eq!(&memory[OUTPUT as usize..OUTPUT as usize + 64], &[0xa5; 64]);
        }
    }
}
