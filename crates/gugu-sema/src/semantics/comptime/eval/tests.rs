use super::*;
use crate::{ParsedModule, cfg::CfgContext, source::SourceSnapshot, target::TargetName};

fn evaluate_last_constant(source: &str) -> Result<ConstantValue, Diagnostic> {
    let sources = SourceMap::new(vec![
        SourceSnapshot::from_str("main.gg", source).expect("合法源码快照"),
    ])
    .expect("合法源码表");
    let file = sources.file_id("main.gg").expect("源码文件存在");
    let snapshot = sources.snapshot(file).expect("源码快照存在");
    let mut lexed = crate::lex(snapshot, &sources, file);
    assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
    let parsed = crate::parse(source, &sources, file, &mut lexed.buffer);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let configured = crate::cfg::configure(
        snapshot,
        &sources,
        &parsed.file,
        &parsed.arena,
        &lexed.buffer,
        &CfgContext::target_only(TargetName::X86_64Linux),
    )
    .expect("cfg 配置成功");
    let modules = [ParsedModule {
        path: "main.gg".to_owned(),
        file: parsed.file,
        arena: parsed.arena,
        tokens: lexed.buffer,
        configured,
        builtin: false,
    }];
    let names = crate::names::analyze_with_prefix(
        "tests/comptime@1.0.0",
        &Default::default(),
        &modules,
        None,
    )
    .expect("名称解析成功");
    let model = Model::new(&modules, &names, &Default::default()).expect("语义模型形成成功");
    let value = modules[0]
        .arena
        .items
        .iter()
        .rev()
        .find_map(|item| match item.kind {
            ItemKind::Const { value, .. } => value,
            _ => None,
        })
        .expect("存在待求值常量");
    model.eval_early_const(0, value, &Ty::int())
}

#[test]
fn repeat_array_len_infers_int() {
    for source in [
        "const N = [1; 3].len()\nconst OUTPUT: int = N",
        "const N: int = [1; 3].len()\nconst OUTPUT: int = N",
        "const N = [[1; 2]; 3].len()\nconst OUTPUT: int = N",
    ] {
        assert_eq!(
            evaluate_last_constant(source).unwrap(),
            ConstantValue::Int(3)
        );
    }
}

#[test]
fn repeat_array_len_preserves_constant_length_dependencies() {
    let source = "const COUNT: int = 1 + 2\nconst N = [0; COUNT + 1].len()\nconst OUTPUT: int = N";
    assert_eq!(
        evaluate_last_constant(source).unwrap(),
        ConstantValue::Int(4)
    );
}

#[test]
fn repeat_array_len_rejects_invalid_length_dependencies() {
    for source in [
        "const N = [0; N].len()\nconst OUTPUT: int = N",
        "const N = [0; -1].len()\nconst OUTPUT: int = N",
    ] {
        assert!(evaluate_last_constant(source).is_err());
    }
}
