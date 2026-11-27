use super::{Compilation, CompileRequest, Compiler, TargetName, harness::Harness};
use std::sync::OnceLock;

fn compiled(name: &str) -> &'static Compilation {
    static WIDE: OnceLock<Compilation> = OnceLock::new();
    static NARROW: OnceLock<Compilation> = OnceLock::new();
    let (width, cast, output) = if name == "render64" {
        (64, "std.mem.transmute[uint, f64](bits)", &WIDE)
    } else {
        (32, "std.mem.transmute[u32, f32](u32(bits))", &NARROW)
    };
    output.get_or_init(|| {
        let mut source = String::new();
        let mut main = String::from("fn main() {\n");
        for (index, spec) in [
            "", "?", "e", "E", ".2", ".0?", ".20", ".2e", ".2E", ".2?", "*>12.2", "+08.2",
        ]
        .iter()
        .enumerate()
        {
            source.push_str(&format!(
                "fn render{width}_{index}(bits: uint) string {{\nlet v = unsafe {{ {cast} }}\nf\"{{v:{spec}}}\"\n}}\n"
            ));
            main.push_str(&format!("_ = render{width}_{index}(0)\n"));
        }
        source.push_str(&format!(
            "fn dynamic(bits: uint, width: int, precision: int) string {{\nlet v = unsafe {{ {cast} }}\nf\"{{v:width$.precision$}}|{{v:width$.precision$e}}\"\n}}\n"
        ));
        main.push_str("_ = dynamic(0, 0, 0)\n");
        source.push_str(&main);
        source.push_str("}\n");
        let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
            "main.gg",
            &source,
            TargetName::X86_64Linux,
        ));
        assert!(output.is_success(), "{:?}", output.diagnostics().items());
        output
    })
}

fn check_with(
    harness: &mut Harness<'_>,
    memory: &mut Vec<u8>,
    name: &str,
    bits: u64,
    expected: &[String],
) {
    for (index, expected) in expected.iter().enumerate() {
        let fn_name = format!("{name}_{index}");
        harness.reset_for_next_call(memory);
        let result = harness.invoke("main.gg", &fn_name, &[bits], memory);
        let start = result[0] as usize;
        let actual = std::str::from_utf8(&memory[start..start + result[1] as usize]).unwrap();
        assert_eq!(actual, expected, "{fn_name} bits={bits:x}");
    }
}

fn check_f64_values(values: &[f64]) {
    let output = compiled("render64");
    let mut harness = Harness::new(output);
    let mut memory = vec![0; 262144];
    for &v in values {
        // Gugu suppresses zero padding for non-finite values.
        let padded = if v.is_finite() {
            format!("{v:+08.2}")
        } else {
            format!("{v:+8.2}")
        };
        check_with(
            &mut harness,
            &mut memory,
            "render64",
            v.to_bits(),
            &[
                format!("{v}"),
                format!("{v:?}"),
                format!("{v:e}"),
                format!("{v:E}"),
                format!("{v:.2}"),
                format!("{v:.0?}"),
                format!("{v:.20}"),
                format!("{v:.2e}"),
                format!("{v:.2E}"),
                format!("{v:.2?}"),
                format!("{v:*>12.2}"),
                padded,
            ],
        );
    }
}

fn check_f32_values(values: &[f32]) {
    let output = compiled("render32");
    let mut harness = Harness::new(output);
    let mut memory = vec![0; 262144];
    for &v in values {
        let padded = if v.is_finite() {
            format!("{v:+08.2}")
        } else {
            format!("{v:+8.2}")
        };
        check_with(
            &mut harness,
            &mut memory,
            "render32",
            v.to_bits() as u64,
            &[
                format!("{v}"),
                format!("{v:?}"),
                format!("{v:e}"),
                format!("{v:E}"),
                format!("{v:.2}"),
                format!("{v:.0?}"),
                format!("{v:.20}"),
                format!("{v:.2e}"),
                format!("{v:.2E}"),
                format!("{v:.2?}"),
                format!("{v:*>12.2}"),
                padded,
            ],
        );
    }
}

fn assert_dynamic_panic(precision: u64) {
    let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut harness = Harness::new(compiled("render64"));
        harness.invoke(
            "main.gg",
            "dynamic",
            &[0, 0, precision],
            &mut vec![0; 262144],
        );
    }))
    .expect_err("非法动态精度必须触发 Runtime(Panic)");
    let message = error
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| error.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        message.contains("Runtime(Panic)"),
        "意外 panic 消息：{message}"
    );
}

#[test]
fn float64_formatting_contracts() {
    check_f64_values(&[
        0.0,
        -0.0,
        1.0,
        0.1,
        0.3,
        1.0 / 3.0,
        1e15,
        1e16,
        1e17,
        1e21,
        1e-7,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::EPSILON,
        f64::NAN,
        -f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.125,
        2.5,
        3.5,
        9.999,
        2.0,
        1024.0,
        0.0001,
        0.00001,
        1e23,
        f64::from_bits(1e15f64.to_bits() - 1),
        f64::from_bits(1e15f64.to_bits() + 1),
        f64::from_bits(1e16f64.to_bits() - 1),
        f64::from_bits(1e16f64.to_bits() + 1),
        f64::from_bits(1e17f64.to_bits() - 1),
        f64::from_bits(1e17f64.to_bits() + 1),
    ]);
    let mut harness = Harness::new(compiled("render64"));
    let mut memory = vec![0; 262144];
    let result = harness.invoke(
        "main.gg",
        "dynamic",
        &[0.1f64.to_bits(), 12, 20],
        &mut memory,
    );
    let actual =
        std::str::from_utf8(&memory[result[0] as usize..(result[0] + result[1]) as usize]).unwrap();
    assert_eq!(actual, format!("{0:12.20}|{0:12.20e}", 0.1f64));
    assert_dynamic_panic(u64::MAX);
    assert_dynamic_panic(i64::MAX as u64);
}

#[test]
fn float64_max_formatting() {
    check_f64_values(&[f64::MAX]);
}

#[test]
fn float64_min_formatting() {
    check_f64_values(&[f64::MIN]);
}

#[test]
fn float32_formatting_contracts() {
    check_f32_values(&[
        0.0,
        -0.0,
        1.0,
        0.1,
        0.3,
        1.0 / 3.0,
        1e15,
        1e16,
        1e17,
        1e21,
        1e-7,
        f32::MIN_POSITIVE,
        f32::from_bits(1),
        f32::MAX,
        f32::MIN,
        f32::EPSILON,
        f32::NAN,
        -f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        0.125,
        2.5,
        3.5,
        9.999,
        2.0,
        1024.0,
        0.0001,
        0.00001,
        1e23,
        f32::from_bits(1.0f32.to_bits() - 1),
        f32::from_bits(1.0f32.to_bits() + 1),
    ]);
    let mut harness = Harness::new(compiled("render32"));
    let mut memory = vec![0; 262144];
    let result = harness.invoke(
        "main.gg",
        "dynamic",
        &[0.1f32.to_bits() as u64, 12, 20],
        &mut memory,
    );
    let actual =
        std::str::from_utf8(&memory[result[0] as usize..(result[0] + result[1]) as usize]).unwrap();
    assert_eq!(actual, format!("{0:12.20}|{0:12.20e}", 0.1f32));
}
