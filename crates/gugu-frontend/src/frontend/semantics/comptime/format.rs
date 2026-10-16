//! comptime f-string 的静态格式化：把常量按已解析的格式说明交给 `std.fmt` 参照模型。
//!
//! 只有标量、string、Option/Result 形状、固定数组和元组有内建格式 trait 实现；其它常量
//! 不能在编译期格式化。标志与值类型的兼容规则与类型检查器共用 `flag_conflict`。

use super::eval::ConstantValue;
use crate::frontend::string::{FormatKind, ValueClass, flag_conflict};
use crate::runtime::fmt::{
    Formatter, ResolvedSpec, format_bool, format_float, format_int, format_str,
};

/// 常量在标志兼容规则里的分类。
pub(super) fn value_class(value: &ConstantValue) -> ValueClass {
    match value {
        ConstantValue::Int(_) => ValueClass::Int,
        ConstantValue::Float(_) => ValueClass::Float,
        ConstantValue::String(_) => ValueClass::Str,
        _ => ValueClass::Other,
    }
}

/// 按格式说明渲染常量；失败文本说明格式码或标志与该值不兼容。
pub(super) fn render(value: &ConstantValue, spec: &ResolvedSpec) -> Result<String, &'static str> {
    if let Some(conflict) = flag_conflict(spec, value_class(value)) {
        return Err(conflict);
    }
    if !kind_applies(value, spec.kind) {
        return Err("格式码没有适用于该值的格式 trait 实现");
    }
    let mut out = Formatter::new(spec.clone());
    write_value(&mut out, value)?;
    Ok(out.finish())
}

/// 值是否具有该格式码对应的内建 trait 实现。
fn kind_applies(value: &ConstantValue, kind: FormatKind) -> bool {
    match kind {
        FormatKind::Print => printable(value),
        FormatKind::Debug => debuggable(value),
        FormatKind::Binary | FormatKind::Octal | FormatKind::LowerHex | FormatKind::UpperHex => {
            matches!(value, ConstantValue::Int(_))
        }
        FormatKind::LowerExponent | FormatKind::UpperExponent => {
            matches!(value, ConstantValue::Float(_))
        }
    }
}

fn printable(value: &ConstantValue) -> bool {
    match value {
        ConstantValue::Unit
        | ConstantValue::Int(_)
        | ConstantValue::Float(_)
        | ConstantValue::Bool(_)
        | ConstantValue::String(_) => true,
        ConstantValue::Array(items) | ConstantValue::Tuple(items) => items.iter().all(printable),
        ConstantValue::ResultOk(inner) | ConstantValue::ResultErr(inner) => printable(inner),
        ConstantValue::Struct(_) | ConstantValue::Type(_) | ConstantValue::ParsedSource(_) => false,
    }
}

fn debuggable(value: &ConstantValue) -> bool {
    match value {
        ConstantValue::Array(items) | ConstantValue::Tuple(items) => items.iter().all(debuggable),
        ConstantValue::ResultOk(inner) | ConstantValue::ResultErr(inner) => debuggable(inner),
        other => printable(other),
    }
}

fn write_value(out: &mut Formatter, value: &ConstantValue) -> Result<(), &'static str> {
    match value {
        ConstantValue::Unit => out.pad("()"),
        ConstantValue::Int(value) => {
            let value = i64::try_from(*value).map_err(|_| "整数常量超出 int 范围")?;
            format_int(out, value);
        }
        ConstantValue::Float(bits) => format_float(out, f64::from_bits(*bits)),
        ConstantValue::Bool(value) => format_bool(out, *value),
        ConstantValue::String(text) => format_str(out, text),
        ConstantValue::Array(items) => write_sequence(out, items, None)?,
        ConstantValue::Tuple(items) => write_sequence(out, items, Some(""))?,
        ConstantValue::ResultOk(inner) => {
            write_sequence(out, std::slice::from_ref(inner), Some("Ok"))?
        }
        ConstantValue::ResultErr(inner) => {
            write_sequence(out, std::slice::from_ref(inner), Some("Err"))?
        }
        ConstantValue::Struct(_) | ConstantValue::Type(_) | ConstantValue::ParsedSource(_) => {
            return Err("该常量没有内建格式 trait 实现");
        }
    }
    Ok(())
}

/// 数组用 list 形状，元组与 Ok/Err 用 tuple 形状；元素按 Debug 递归写入。
fn write_sequence(
    out: &mut Formatter,
    items: &[ConstantValue],
    tuple_name: Option<&str>,
) -> Result<(), &'static str> {
    let mut failure = None;
    let mut builder = match tuple_name {
        Some(name) => out.debug_tuple(name),
        None => out.debug_list(),
    };
    for item in items {
        builder.entry(|child| {
            if let Err(error) = write_value(child, item) {
                failure = Some(error);
            }
        });
    }
    builder.finish();
    failure.map_or(Ok(()), Err)
}
