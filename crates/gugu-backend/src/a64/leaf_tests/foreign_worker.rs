use super::*;
use crate::{
    a64::foreign::Platform,
    runtime::{foreign_packet as packet, foreign_schema as schema},
};
use std::collections::BTreeMap;

fn word(memory: &mut BTreeMap<u64, u8>, at: u64, value: u64) {
    for byte in 0..8 {
        memory.insert(at + byte, (value >> (byte * 8)) as u8);
    }
}
fn read(memory: &BTreeMap<u64, u8>, at: u64) -> u64 {
    (0..8).fold(0, |value, byte| {
        value | u64::from(*memory.get(&(at + byte)).unwrap_or(&0)) << (byte * 8)
    })
}

fn raw(symbol: &str, fragment: fragments::Fragment) -> codegen::LeafFragment {
    let mut fixture = Fixture::new(&[], &[]);
    fixture.ret(&[]);
    let mut result = fixture.compile();
    result.symbol = symbol.into();
    result.bytes = fragment.bytes;
    result.relocations = fragment
        .relocations
        .into_iter()
        .map(|relocation| crate::a64::layout::Relocation {
            offset: relocation.offset.into(),
            relocation: relocation.relocation,
        })
        .collect();
    result
}

fn return_constant(symbol: &str, value: u64) -> codegen::LeafFragment {
    let mut fixture = Fixture::new(&[], &[Type::I64]);
    let value = fixture.constant(Type::I64, value);
    fixture.ret(&[value]);
    let mut result = fixture.compile();
    result.symbol = symbol.into();
    result
}

fn tls(platform: Platform) -> (codegen::LeafFragment, codegen::LeafFragment) {
    let tid = if platform == Platform::Linux {
        return_constant("gettid", 42)
    } else {
        let mut fixture = Fixture::new(&[Type::I64, Type::Ptr], &[Type::I32]);
        let value = fixture.constant(Type::I64, 42);
        fixture.op(
            Op::Store(Access {
                alias: AliasClass::Foreign,
                align: 8,
                volatile: false,
            }),
            &[fixture.param(0, 1), value],
            &[],
        );
        let value = fixture.constant(Type::I32, 0);
        fixture.ret(&[value]);
        let mut result = fixture.compile();
        result.symbol = platform.symbol("pthread_threadid_np");
        result
    };
    let errno = return_constant(
        &platform.symbol(if platform == Platform::Linux {
            "__errno_location"
        } else {
            "__error"
        }),
        0xb000,
    );
    (tid, errno)
}

#[test]
fn worker_reads_x6_x7_x8_and_stack_arguments_and_captures_thread_errno() {
    for platform in [Platform::Linux, Platform::Darwin] {
        let worker = raw("worker", fragments::foreign_native_worker(platform));
        let (tid, errno) = tls(platform);
        let mut fixture = Fixture::new(&[Type::I64; 10], &[Type::I64]);
        let mut total = fixture.param(0, 6);
        for index in 7..10 {
            total = fixture.op(
                Op::Integer(IntOp::Add),
                &[total, fixture.param(0, index)],
                &[Type::I64],
            )[0];
        }
        fixture.ret(&[total]);
        let mut native = fixture.compile();
        native.symbol = "native".into();
        let mut prefix = Vec::new();
        for (key, value, base, offset) in [
            ("str.X.Unsigned", 8, 0, 0),
            ("ldr.X.Unsigned", 13, 2, 0),
            ("str.X.Unsigned", 13, 0, 8),
        ] {
            let instruction = machine::inst(
                key,
                vec![
                    inst::Operand::Reg(reg::Reg::x(value)),
                    inst::Operand::Address(inst::Address {
                        base: reg::Reg::x(base),
                        mode: inst::AddressMode::Unsigned(offset),
                    }),
                ],
            )
            .unwrap();
            prefix.extend(encode::encode(&instruction).unwrap().bytes());
        }
        native.bytes.splice(..0, prefix);
        let mut memory = BTreeMap::new();
        word(
            &mut memory,
            0x4000 + schema::NATIVE_REQUEST_ARGUMENTS as u64,
            0x5000,
        );
        word(
            &mut memory,
            0x5000,
            (worker.bytes.len() + tid.bytes.len() + errno.bytes.len()) as u64,
        );
        word(&mut memory, 0x5008, 16);
        word(
            &mut memory,
            0x4000 + schema::NATIVE_REQUEST_THREAD_HANDLE as u64,
            0x1234,
        );
        for index in 0..8 {
            word(
                &mut memory,
                0x5000 + u64::from(packet::a64_integer_lane(index).unwrap()),
                match index {
                    0 => 0x9000,
                    2 => 0x4000,
                    _ => index.into(),
                },
            );
        }
        word(&mut memory, 0x5000 + packet::A64_PACKET_SRET as u64, 0xa000);
        word(&mut memory, 0x5000 + packet::A64_PACKET_BYTES as u64, 8);
        word(&mut memory, 0x5000 + packet::A64_PACKET_BYTES as u64 + 8, 9);
        word(&mut memory, 0xb000, 73);
        let (result, _, memory) =
            vm::execute_seeded(&worker, &[&tid, &errno, &native], &[0x4000], memory);
        assert_eq!(result[0], 0);
        assert_eq!(read(&memory, 0x5080), 30);
        assert_eq!(read(&memory, 0x50a0), 73);
        assert_eq!(read(&memory, 0x9000), 0xa000);
        assert_eq!(read(&memory, 0x9008), 42);
        assert_eq!(read(&memory, 0x4000), 0);
        assert_eq!(
            read(&memory, 0x4000 + schema::NATIVE_REQUEST_ERROR as u64),
            0
        );
        assert_eq!(
            read(
                &memory,
                0x4000 + schema::NATIVE_REQUEST_THREAD_HANDLE as u64
            ),
            0x1234
        );
    }
}

#[test]
fn worker_preserves_all_four_hfa_results_and_rejects_invalid_native_stack_sizes() {
    for platform in [Platform::Linux, Platform::Darwin] {
        let worker = raw("worker", fragments::foreign_native_worker(platform));
        let (tid, errno) = tls(platform);
        let mut fixture = Fixture::new(&[Type::F64; 8], &[Type::F64; 4]);
        fixture.ret(&[
            fixture.param(0, 0),
            fixture.param(0, 3),
            fixture.param(0, 6),
            fixture.param(0, 7),
        ]);
        let mut native = fixture.compile();
        native.symbol = "native".into();
        for stack in [0, 7, 65536] {
            let mut memory = BTreeMap::new();
            word(
                &mut memory,
                0x4000 + schema::NATIVE_REQUEST_ARGUMENTS as u64,
                0x5000,
            );
            word(
                &mut memory,
                0x5000,
                (worker.bytes.len() + tid.bytes.len() + errno.bytes.len()) as u64,
            );
            word(&mut memory, 0x5008, stack);
            word(
                &mut memory,
                0x5000 + packet::A64_PACKET_EXTRA_RESULTS as u64,
                0xc000,
            );
            for index in 0..8 {
                word(
                    &mut memory,
                    0x5040 + index * 8,
                    (index as f64 + 0.5).to_bits(),
                );
            }
            let (_, _, memory) =
                vm::execute_seeded(&worker, &[&tid, &errno, &native], &[0x4000], memory);
            if stack == 0 {
                assert_eq!(
                    [
                        read(&memory, 0x5090),
                        read(&memory, 0x5098),
                        read(&memory, 0xc000),
                        read(&memory, 0xc008)
                    ],
                    [0.5_f64, 3.5, 6.5, 7.5].map(f64::to_bits)
                );
            } else {
                assert_eq!(
                    read(&memory, 0x4000 + schema::NATIVE_REQUEST_ERROR as u64),
                    22
                );
                assert!(!memory.contains_key(&0xc000));
            }
        }
    }
}
