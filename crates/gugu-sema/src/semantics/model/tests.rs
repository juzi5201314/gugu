use super::Model;
use crate::{Diagnostic, DiagnosticCode, SourceMap, TargetName, source::SourceSnapshot};

fn diagnostics(source: &str) -> Vec<Diagnostic> {
    let sources =
        SourceMap::new(vec![SourceSnapshot::from_str("main.gg", source).unwrap()]).unwrap();
    let cfg = crate::cfg::CfgContext::target_only(TargetName::X86_64Linux);
    let modules = crate::modules::parse_modules(&sources, "", &cfg, &Default::default()).unwrap();
    let names = crate::names::analyze_with_prefix(
        "tests/diagnostic-spans@1",
        &Default::default(),
        &modules,
        None,
    )
    .unwrap();
    let paths = Default::default();
    let model = match Model::new(&modules, &names, &paths) {
        Ok(model) => model,
        Err(errors) => return errors,
    };
    super::super::checker::check(&model, &Default::default(), None, cfg.target())
        .err()
        .unwrap_or_default()
}

#[test]
fn type_diagnostics_point_to_the_innermost_invalid_source() {
    let cases = [
        ("泛型实参数量不符", "Cell"),
        ("别名实参数量不符", "Alias[int, bool]"),
        ("类型与 comptime 泛型实参种类不符", "Cell[3]"),
        ("未解析名称 `Missing`", "Missing"),
        ("comptime 泛型实参或数组长度必须非负且不超过 u64", "-1"),
        ("comptime 泛型参数目前必须声明为 int", "bool"),
        ("`Missing` 不是 trait", "Missing"),
        ("trait 实参数量不符", "Mark"),
        ("关联路径的类型实参数量不符", "Cell::Item"),
        ("类型别名实参数量不符", "Alias[int, bool]::Item"),
        ("关联路径的 trait 实参数量不符", "Mark::Item"),
        ("trait Mark 不满足对象安全条件", "Mark"),
        ("无法推断空数组常量元素类型", "[]"),
        ("常量数组元素类型不一致", "true"),
        ("声明不是值", "Mark"),
    ];
    let sources: Vec<_> = include_str!("fixtures/diagnostic_spans.gg")
        .split("\n// ---\n")
        .collect();
    assert_eq!(sources.len(), cases.len(), "每个 fixture 必须声明期望诊断");
    for (source, (message, selected)) in sources.into_iter().zip(cases) {
        let errors = diagnostics(source);
        let error = errors
            .iter()
            .find(|error| error.message() == message)
            .unwrap_or_else(|| panic!("应产生 `{message}`：{errors:?}"));
        assert_eq!(error.code(), DiagnosticCode::InvalidType);
        let span = error.span().expect("源码错误必须有主范围");
        let start = source.rfind(selected).unwrap();
        assert_eq!(span.start() as usize, start, "{source}\n{errors:?}");
        assert_eq!(
            span.end() as usize,
            start + selected.len(),
            "{source}\n{errors:?}"
        );
    }
}
