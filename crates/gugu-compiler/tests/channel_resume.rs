//! 检查 LIR 中的阻塞通道恢复边，不执行机器码或依赖调度时序。
use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn buffered_channel_resume_retries_payload_operation() {
    let output =
        Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_to_lir(CompileRequest::single_file(
            "main.gg",
            include_str!("fixtures/channel_resume.gg"),
            TargetName::X86_64Linux,
        ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let lir = output.dump_lir().expect("LIR");
    for (caller, transfer) in [
        ("channel_send", "enqueue_value"),
        ("channel_receive", "dequeue_value"),
    ] {
        assert_resume_reaches(&lir, caller, transfer);
    }
}

fn function<'a>(lir: &'a str, name: &str) -> &'a str {
    lir.split(&format!("fn {name} instance="))
        .nth(1)
        .expect("镜像函数")
        .split("\n}\n")
        .next()
        .unwrap()
}

fn target(lir: &str, name: &str) -> String {
    let key = function(lir, name).split_whitespace().next().unwrap();
    let bytes: Vec<u8> = (0..key.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&key[at..at + 2], 16).unwrap())
        .collect();
    format!("invoke Instance({bytes:?})")
}

fn number(text: &str) -> u32 {
    text.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

fn assert_resume_reaches(lir: &str, caller: &str, transfer: &str) {
    let park = target(lir, "park_on");
    let operation = target(lir, transfer);
    let mut block = 0;
    let mut resumes = Vec::new();
    let mut transfers = BTreeSet::new();
    let mut edges = BTreeMap::new();
    let mut successors: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for line in function(lir, caller).lines().map(str::trim_start) {
        if let Some(rest) = line.strip_prefix("bb") {
            block = number(rest);
        }
        if line.starts_with(&park) {
            resumes.push(number(line.split("normal=e").nth(1).unwrap()));
        }
        if line.starts_with(&operation) {
            transfers.insert(block);
        }
        if line.starts_with('e') && line.contains(" -> bb") && line.ends_with("unwind=false") {
            let edge = number(&line[1..]);
            let from = number(line.split(": bb").nth(1).unwrap());
            let to = number(line.split(" -> bb").nth(1).unwrap());
            edges.insert(edge, to);
            successors.entry(from).or_default().push(to);
        }
    }
    // 会合分支可在唤醒后返回；缓冲分支必须存在重新访问环载荷的恢复路径。
    assert!(
        resumes.into_iter().any(|edge| {
            let mut pending = vec![edges[&edge]];
            let mut seen = BTreeSet::new();
            while let Some(block) = pending.pop() {
                if !seen.insert(block) {
                    continue;
                }
                if transfers.contains(&block) {
                    return true;
                }
                pending.extend(successors.get(&block).into_iter().flatten().copied());
            }
            false
        }),
        "{caller} 的挂起恢复边必须能到达 {transfer}，不能直接返回成功"
    );
}
