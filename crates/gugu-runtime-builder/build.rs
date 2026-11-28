//! 从固定的 Unicode 17 数据生成目标代码使用的只读索引。
use std::{collections::BTreeMap, fmt::Write, fs, path::PathBuf};

const DATA: &str = "../gugu-frontend/resources/unicode/17.0.0";

fn input(name: &str) -> String {
    let path = format!("{DATA}/{name}");
    println!("cargo:rerun-if-changed={path}");
    fs::read_to_string(path).expect("固定 Unicode 数据")
}

fn rows(text: &str) -> impl Iterator<Item = Vec<&str>> {
    text.lines().filter_map(|line| {
        let line = line.split('#').next().unwrap().trim();
        (!line.is_empty()).then(|| line.split(';').map(str::trim).collect())
    })
}

fn hex(text: &str) -> u32 {
    u32::from_str_radix(text, 16).expect("Unicode 十六进制标量")
}

fn sequence(text: &str) -> Vec<u32> {
    text.split_whitespace().map(hex).collect()
}

fn number(output: &mut Vec<u8>, value: u32) {
    assert!(value < 1 << 21, "三字节索引必须能表示标量和偏移");
    output.extend([
        value as u8 & 127,
        (value >> 7) as u8 & 127,
        (value >> 14) as u8,
    ]);
}

fn table(output: &mut String, name: &str, bytes: &[u8]) {
    write!(output, "pub fn {name}() string = \"").unwrap();
    for &byte in bytes {
        assert!(byte < 128, "表编码只使用合法 UTF-8 单字节");
        write!(output, "\\u{{{byte:x}}}").unwrap();
    }
    output.push_str("\"\n");
}

fn mappings(output: &mut String, name: &str, values: BTreeMap<u32, Vec<u32>>) {
    let mut index = Vec::new();
    let mut payload = Vec::new();
    for (scalar, mapped) in values {
        number(&mut index, scalar);
        number(&mut index, payload.len() as u32 / 3);
        assert!(mapped.len() < 128);
        index.push(mapped.len() as u8);
        for scalar in mapped {
            number(&mut payload, scalar);
        }
    }
    table(output, &format!("{name}_index"), &index);
    table(output, &format!("{name}_values"), &payload);
}

fn casing(output: &mut String) {
    let mut lower = BTreeMap::new();
    let mut upper = BTreeMap::new();
    for row in rows(&input("UnicodeData.txt")) {
        let scalar = hex(row[0]);
        if !row[12].is_empty() {
            upper.insert(scalar, sequence(row[12]));
        }
        if !row[13].is_empty() {
            lower.insert(scalar, sequence(row[13]));
        }
    }
    for row in rows(&input("SpecialCasing.txt")) {
        if row[4].is_empty() {
            lower.insert(hex(row[0]), sequence(row[1]));
            upper.insert(hex(row[0]), sequence(row[3]));
        }
    }
    let mut fold = BTreeMap::new();
    for row in rows(&input("CaseFolding.txt")) {
        if matches!(row[1], "C" | "F") {
            fold.insert(hex(row[0]), sequence(row[2]));
        }
    }
    mappings(output, "lower", lower);
    mappings(output, "upper", upper);
    mappings(output, "fold", fold);
}

fn ranges(output: &mut String, name: &str, input: &str, properties: &[&str]) {
    let mut ranges = Vec::new();
    for row in rows(input) {
        let property = if row[1] == "InCB" { row[2] } else { row[1] };
        let Some(value) = properties
            .iter()
            .position(|&candidate| candidate == property)
        else {
            continue;
        };
        let (first, last) = row[0].split_once("..").unwrap_or((row[0], row[0]));
        ranges.push((hex(first), hex(last), value as u8 + 1));
    }
    ranges.sort_unstable();
    let mut bytes = Vec::new();
    for (first, last, value) in ranges {
        number(&mut bytes, first);
        number(&mut bytes, last);
        bytes.push(value);
    }
    table(output, name, &bytes);
}

fn main() {
    let mut output = String::from("// Unicode 17.0.0；由 runtime-builder/build.rs 确定性生成。\n");
    casing(&mut output);
    let core = input("DerivedCoreProperties.txt");
    ranges(&mut output, "cased", &core, &["Cased"]);
    ranges(&mut output, "case_ignorable", &core, &["Case_Ignorable"]);
    ranges(
        &mut output,
        "incb",
        &core,
        &["Consonant", "Extend", "Linker"],
    );
    ranges(
        &mut output,
        "pictographic",
        &input("emoji-data.txt"),
        &["Extended_Pictographic"],
    );
    ranges(
        &mut output,
        "grapheme",
        &input("GraphemeBreakProperty.txt"),
        &[
            "CR",
            "LF",
            "Control",
            "L",
            "V",
            "T",
            "LV",
            "LVT",
            "Extend",
            "ZWJ",
            "SpacingMark",
            "Prepend",
            "Regional_Indicator",
        ],
    );
    ranges(
        &mut output,
        "word",
        &input("WordBreakProperty.txt"),
        &[
            "CR",
            "LF",
            "Newline",
            "Extend",
            "Format",
            "ZWJ",
            "WSegSpace",
            "ALetter",
            "Hebrew_Letter",
            "Numeric",
            "Katakana",
            "ExtendNumLet",
            "MidLetter",
            "MidNum",
            "MidNumLet",
            "Single_Quote",
            "Double_Quote",
            "Regional_Indicator",
        ],
    );
    fs::write(
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("unicode_tables.gg"),
        output,
    )
    .expect("写入构建目录中的 Unicode 表");
}
