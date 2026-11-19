//! Manual libc resolver verification; intentionally outside the test suite.
#[path = "support/a64_linux_probe.rs"]
pub mod probe;

use gugu_backend::{
    NativeImport,
    a64::{
        elf::{DynamicLink, Import, Location},
        foreign::Platform,
        fragments::{
            foreign_native_worker, linux_import_slot_symbol, linux_import_stub,
            linux_resolver_imports, linux_resolver_symbol,
        },
    },
};
use gugu_runtime_abi::{foreign_packet as packet, foreign_schema as schema};
use probe::{Arg, Arg::*, Probe};
use std::collections::BTreeMap;

fn imports(p: &mut Probe) -> BTreeMap<String, Location> {
    let imports: Vec<_> = linux_resolver_imports("libc.so")
        .into_iter()
        .chain(["gettid", "__errno_location"].map(|name| NativeImport::function("libc.so", name)))
        .map(|native| Import {
            native,
            slot: p.data(vec![0; 8]),
        })
        .collect();
    let mut symbols: BTreeMap<_, _> = imports
        .iter()
        .map(|import| (linux_import_slot_symbol(&import.native.name), import.slot))
        .collect();
    for import in &imports {
        let name = &import.native.name;
        let stub = p.symbolic_fragment(linux_import_stub(name), &symbols);
        symbols.insert(name.clone(), stub);
    }
    p.dynamic(DynamicLink {
        interpreter: "/lib/ld-musl-aarch64.so.1".into(),
        imports,
    });
    symbols
}

fn call(p: &mut Probe, resolver: Location, worker: Location, op: u64, args: [Arg; 4]) -> Location {
    p.number(0, op);
    p.call(resolver);
    let packet = p.record(
        packet::A64_PACKET_BYTES,
        &[
            (packet::FOREIGN_PACKET_TARGET, Register(0)),
            (16, args[0]),
            (24, args[1]),
            (32, args[2]),
            (40, args[3]),
        ],
    );
    let request = p.record(
        schema::NATIVE_REQUEST_BYTES,
        &[
            (schema::NATIVE_REQUEST_OPERATION, Number(u64::MAX)),
            (schema::NATIVE_REQUEST_ARGUMENTS, Pointer(packet)),
        ],
    );
    p.address(0, request);
    p.call(worker);
    p.address(9, request);
    p.load(10, 9, schema::NATIVE_REQUEST_ERROR);
    p.expect(10, 0);
    packet
}

fn resolve(p: &mut Probe, resolver: Location, worker: Location, host: &str, address: u64) {
    let host = p.data([host.as_bytes(), &[0]].concat());
    let service = p.data(b"80\0".to_vec());
    let hints = p.record(48, &[(0, Number(2 << 32)), (8, Number(1))]);
    let result = p.data(vec![0; 8]);
    let packet = call(
        p,
        resolver,
        worker,
        0,
        [
            Pointer(host),
            Pointer(service),
            Pointer(hints),
            Pointer(result),
        ],
    );
    p.address(9, packet);
    p.load(10, 9, packet::FOREIGN_PACKET_RESULTS);
    p.expect(10, 0);
    p.address(9, result);
    p.load(21, 9, 0);
    p.expect_nonzero(21);
    p.load(9, 21, 24);
    p.expect_nonzero(9);
    p.load32(10, 9, 4);
    p.expect(10, address);
    call(
        p,
        resolver,
        worker,
        1,
        [Register(21), Number(0), Number(0), Number(0)],
    );
}

fn error(p: &mut Probe, resolver: Location, worker: Location) {
    let host = p.data(b"not-an-ip\0".to_vec());
    let hints = p.record(48, &[(0, Number((2 << 32) | 4)), (8, Number(1))]);
    let result = p.data(vec![0; 8]);
    let packet = call(
        p,
        resolver,
        worker,
        0,
        [Pointer(host), Number(0), Pointer(hints), Pointer(result)],
    );
    p.address(9, packet);
    p.load32(10, 9, packet::FOREIGN_PACKET_RESULTS);
    p.expect(10, 0xffff_fffe);
    p.address(9, result);
    p.load(10, 9, 0);
    p.expect(10, 0);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: a64_linux_resolver_image <output>")?;
    let mut p = Probe::new();
    let symbols = imports(&mut p);
    let resolver = p.symbolic_fragment(linux_resolver_symbol(), &symbols);
    let worker = p.symbolic_fragment(foreign_native_worker(Platform::Linux), &symbols);
    resolve(&mut p, resolver, worker, "localhost", 0x0100_007f);
    p.output(b"getaddrinfo localhost: 127.0.0.1; freeaddrinfo: ok\n");
    resolve(&mut p, resolver, worker, "resolver.test", 0x0764_33c6);
    p.output(b"getaddrinfo /etc/hosts resolver.test: 198.51.100.7; freeaddrinfo: ok\n");
    error(&mut p, resolver, worker);
    p.output(b"getaddrinfo AI_NUMERICHOST not-an-ip: EAI_NONAME=-2, null result\n");
    p.finish(&path)
}
