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
        for (index, spec) in ["", "?", "e", "E", ".2", ".0?", ".20", ".2e", ".2E", ".2?", "*>12.2", "+08.2"].iter().enumerate() {
            source.push_str(&format!("fn render{width}_{index}(bits: uint) string {{\nlet v = unsafe {{ {cast} }}\nf\"{{v:{spec}}}\"\n}}\n"));
            main.push_str(&format!("_ = render{width}_{index}(0)\n"));
        }
        source.push_str(&format!("fn dynamic(bits: uint, width: int, precision: int) string {{\nlet v = unsafe {{ {cast} }}\nf\"{{v:width$.precision$}}|{{v:width$.precision$e}}\"\n}}\n"));
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

fn check(name: &str, bits: u64, expected: &[String]) {
    let output = compiled(name);
    for (index, expected) in expected.iter().enumerate() {
        let name = format!("{name}_{index}");
        let mut harness = Harness::new(output);
        let mut memory = vec![0; 262144];
        let result = harness.invoke("main.gg", &name, &[bits], &mut memory);
        let start = result[0] as usize;
        let actual = std::str::from_utf8(&memory[start..start + result[1] as usize]).unwrap();
        assert_eq!(actual, expected, "{name} bits={bits:x}");
    }
}

macro_rules! cases {
    ($name:ident, $render:literal, $ty:ty, $($value:expr),+ $(,)?) => {
        #[test]
        fn $name() {
            for v in [$($value as $ty),+] {
                // Gugu suppresses zero padding for non-finite values.
                let padded = if v.is_finite() { format!("{v:+08.2}") } else { format!("{v:+8.2}") };
                check($render, v.to_bits() as u64, &[
                    format!("{v}"), format!("{v:?}"), format!("{v:e}"), format!("{v:E}"),
                    format!("{v:.2}"), format!("{v:.0?}"), format!("{v:.20}"),
                    format!("{v:.2e}"), format!("{v:.2E}"), format!("{v:.2?}"),
                    format!("{v:*>12.2}"), padded]);
            }
        }
    }
}

#[test]
fn float_dynamic_counts() {
    for (name, bits, expected) in [
        (
            "render32",
            0.1f32.to_bits() as u64,
            format!("{0:12.20}|{0:12.20e}", 0.1f32),
        ),
        (
            "render64",
            0.1f64.to_bits(),
            format!("{0:12.20}|{0:12.20e}", 0.1f64),
        ),
    ] {
        let mut harness = Harness::new(compiled(name));
        let mut memory = vec![0; 262144];
        let result = harness.invoke("main.gg", "dynamic", &[bits, 12, 20], &mut memory);
        let actual =
            std::str::from_utf8(&memory[result[0] as usize..(result[0] + result[1]) as usize])
                .unwrap();
        assert_eq!(actual, expected);
    }
}

#[test]
#[should_panic(expected = "Runtime(Panic)")]
fn float_negative_precision_panics() {
    let mut harness = Harness::new(compiled("render64"));
    harness.invoke(
        "main.gg",
        "dynamic",
        &[0, 0, u64::MAX],
        &mut vec![0; 262144],
    );
}

#[test]
#[should_panic(expected = "Runtime(Panic)")]
fn float_overflowing_precision_panics() {
    let mut harness = Harness::new(compiled("render64"));
    harness.invoke(
        "main.gg",
        "dynamic",
        &[0, 0, i64::MAX as u64],
        &mut vec![0; 262144],
    );
}

cases!(float64_zero, "render64", f64, 0.0, -0.0);
cases!(float64_fractions, "render64", f64, 1.0, 0.1, 0.3, 1.0 / 3.0);
cases!(
    float64_decimal_boundaries,
    "render64",
    f64,
    1e15,
    1e16,
    1e17,
    1e21,
    1e-7
);
cases!(float64_min_normal, "render64", f64, f64::MIN_POSITIVE);
cases!(float64_subnormal, "render64", f64, f64::from_bits(1));
cases!(float64_extreme_max, "render64", f64, f64::MAX);
cases!(float64_extreme_min, "render64", f64, f64::MIN);
cases!(float64_epsilon, "render64", f64, f64::EPSILON);
cases!(
    float64_special,
    "render64",
    f64,
    f64::NAN,
    -f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY
);
cases!(float64_rounding, "render64", f64, 0.125, 2.5, 3.5, 9.999);
cases!(
    float64_powers,
    "render64",
    f64,
    2.0,
    1024.0,
    0.0001,
    0.00001,
    1e23
);
cases!(
    float64_neighbors_15,
    "render64",
    f64,
    f64::from_bits(1e15f64.to_bits() - 1),
    f64::from_bits(1e15f64.to_bits() + 1)
);
cases!(
    float64_neighbors_16,
    "render64",
    f64,
    f64::from_bits(1e16f64.to_bits() - 1),
    f64::from_bits(1e16f64.to_bits() + 1)
);
cases!(
    float64_neighbors_17,
    "render64",
    f64,
    f64::from_bits(1e17f64.to_bits() - 1),
    f64::from_bits(1e17f64.to_bits() + 1)
);
cases!(float32_zero, "render32", f32, 0.0, -0.0);
cases!(float32_fractions, "render32", f32, 1.0, 0.1, 0.3, 1.0 / 3.0);
cases!(
    float32_decimal_boundaries,
    "render32",
    f32,
    1e15,
    1e16,
    1e17,
    1e21,
    1e-7
);
cases!(float32_min_normal, "render32", f32, f32::MIN_POSITIVE);
cases!(float32_subnormal, "render32", f32, f32::from_bits(1));
cases!(float32_extremes, "render32", f32, f32::MAX, f32::MIN);
cases!(float32_epsilon, "render32", f32, f32::EPSILON);
cases!(
    float32_special,
    "render32",
    f32,
    f32::NAN,
    -f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY
);
cases!(float32_rounding, "render32", f32, 0.125, 2.5, 3.5, 9.999);
cases!(
    float32_powers,
    "render32",
    f32,
    2.0,
    1024.0,
    0.0001,
    0.00001,
    1e23
);
cases!(
    float32_neighbors,
    "render32",
    f32,
    f32::from_bits(1.0f32.to_bits() - 1),
    f32::from_bits(1.0f32.to_bits() + 1)
);
