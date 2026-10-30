//! 模板内只允许前向控制流；稠密指令序号上的单遍数据流同时证明出口栈指针不变。
use std::{borrow::Cow, collections::BTreeMap};

pub(in super::super) fn managed_stack(template: &str) -> Result<u64, &'static str> {
    let mut instructions = Vec::new();
    let mut named = BTreeMap::new();
    let mut numeric: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for line in template.lines() {
        let line = line.split_once('#').map_or(line, |(code, _)| code);
        for text in line.split(';') {
            let mut text = text.trim();
            while let Some((label, rest)) = text.split_once(':') {
                let label = label.trim();
                if label.is_empty()
                    || !label
                        .chars()
                        .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.' | '$'))
                {
                    break;
                }
                if label.bytes().all(|byte| byte.is_ascii_digit()) {
                    numeric
                        .entry(label.trim_start_matches('0'))
                        .or_default()
                        .push(instructions.len());
                } else if named.insert(label, instructions.len()).is_some() {
                    return Err("汇编模板含重复标签");
                }
                text = rest.trim();
            }
            if text.starts_with('.') {
                return Err("managed asm 不允许用汇编 directive 隐藏机器指令");
            }
            if !text.is_empty() {
                instructions.push(text);
            }
        }
    }
    let mut depths = vec![None; instructions.len() + 1];
    depths[0] = Some(0_i64);
    let mut reserve = 0_i64;
    for (index, text) in instructions.iter().enumerate() {
        let (opcode, operands, repeated) = instruction(text)?;
        let opcode = opcode.as_ref();
        if prohibited(opcode) || repeated && string_instruction(opcode) {
            return Err("managed asm 不能包含 system/wait、ret 或 repeat string 指令");
        }
        let call = matches!(opcode, "call" | "callq" | "calll" | "callw");
        let jump = matches!(opcode, "jmp" | "jmpq" | "jmpl" | "jmpw");
        let branch = call || opcode.starts_with('j') || opcode.starts_with("loop");
        let target = if branch {
            if operands.starts_with('*') {
                return Err("managed asm 不能包含无法解析的间接 branch/call");
            }
            let target = if let Some(label) = operands.strip_suffix('f').filter(|label| {
                !label.is_empty() && label.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                numeric
                    .get(label.trim_start_matches('0'))
                    .and_then(|targets| targets.iter().copied().find(|target| *target > index))
            } else if let Some(label) = operands.strip_suffix('b').filter(|label| {
                !label.is_empty() && label.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                numeric
                    .get(label.trim_start_matches('0'))
                    .and_then(|targets| {
                        targets
                            .iter()
                            .rev()
                            .copied()
                            .find(|target| *target <= index)
                    })
            } else {
                named.get(operands).copied()
            };
            let target = target.ok_or("managed asm 不能跳转或调用模板之外的符号")?;
            if target <= index {
                return Err("managed asm 不能包含回边");
            }
            Some(target)
        } else {
            None
        };
        let adjustment = stack_adjustment(opcode, operands, call)?;
        let Some(depth) = depths[index] else {
            continue;
        };
        let depth = depth.checked_add(adjustment).ok_or("汇编栈调整溢出")?;
        reserve = reserve.max(depth);
        if let Some(target) = target {
            merge(&mut depths[target], depth)?;
        }
        if !jump && !call {
            merge(&mut depths[index + 1], depth)?;
        }
    }
    if depths[instructions.len()] != Some(0) {
        return Err("managed asm 的所有出口必须恢复原始栈指针");
    }
    Ok(reserve as u64)
}

fn merge(destination: &mut Option<i64>, value: i64) -> Result<(), &'static str> {
    if destination.is_some_and(|previous| previous != value) {
        return Err("汇编控制流合流时栈深度不一致");
    }
    *destination = Some(value);
    Ok(())
}

fn instruction(text: &str) -> Result<(Cow<'_, str>, &str, bool), &'static str> {
    let mut text = text;
    let mut repeated = false;
    loop {
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        let word = &text[..end];
        let opcode = if word.bytes().any(|byte| byte.is_ascii_uppercase()) {
            Cow::Owned(word.to_ascii_lowercase())
        } else {
            Cow::Borrowed(word)
        };
        text = text[end..].trim();
        if matches!(opcode.as_ref(), "rep" | "repe" | "repz" | "repne" | "repnz") {
            repeated = true;
        } else if !matches!(
            opcode.as_ref(),
            "lock"
                | "notrack"
                | "bnd"
                | "cs"
                | "ds"
                | "es"
                | "ss"
                | "fs"
                | "gs"
                | "data16"
                | "data32"
                | "addr16"
                | "addr32"
                | "rex64"
        ) && !opcode.starts_with("rex")
        {
            return Ok((opcode, text, repeated));
        }
        if text.is_empty() {
            return Err("managed asm 不能使用独立机器指令前缀");
        }
    }
}

fn string_instruction(opcode: &str) -> bool {
    ["movs", "cmps", "scas", "lods", "stos", "ins", "outs"]
        .iter()
        .any(|prefix| {
            opcode
                .strip_prefix(prefix)
                .is_some_and(|suffix| matches!(suffix, "" | "b" | "w" | "l" | "q"))
        })
}

fn prohibited(opcode: &str) -> bool {
    opcode.starts_with("ret")
        || opcode.starts_with("lret")
        || opcode.starts_with("iret")
        || matches!(
            opcode,
            "syscall"
                | "sysenter"
                | "sysexit"
                | "sysret"
                | "sysretq"
                | "sysretl"
                | "int"
                | "int1"
                | "int3"
                | "into"
                | "hlt"
                | "mwait"
                | "monitor"
                | "umwait"
                | "tpause"
                | "mwaitx"
                | "monitorx"
                | "clzero"
                | "cli"
                | "sti"
                | "in"
                | "inb"
                | "inw"
                | "inl"
                | "out"
                | "outb"
                | "outw"
                | "outl"
                | "insb"
                | "insw"
                | "insl"
                | "outsb"
                | "outsw"
                | "outsl"
                | "invlpg"
                | "invd"
                | "wbinvd"
                | "rdmsr"
                | "wrmsr"
                | "lgdt"
                | "lidt"
                | "lldt"
                | "ltr"
                | "lcall"
                | "ljmp"
                | "ud2"
                | "xbegin"
                | "xend"
                | "xabort"
                | "vmcall"
                | "vmlaunch"
                | "vmresume"
                | "vmrun"
                | "vmxoff"
                | "rsm"
                | "wrfsbase"
                | "wrgsbase"
                | "enter"
                | "leave"
                | "enterq"
                | "leaveq"
        )
}

fn stack_adjustment(opcode: &str, operands: &str, call: bool) -> Result<i64, &'static str> {
    if operands
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .any(|operand| {
            let operand = operand.strip_prefix('%').unwrap_or(operand);
            operand.get(..2).is_some_and(|prefix| {
                prefix.eq_ignore_ascii_case("cr") || prefix.eq_ignore_ascii_case("dr")
            })
        })
    {
        return Err("managed asm 不能访问系统控制寄存器");
    }
    let destination = operands.rsplit(',').next().unwrap_or(operands).trim();
    if ["%rsp", "%esp", "%sp", "%spl"]
        .iter()
        .any(|register| destination.eq_ignore_ascii_case(register))
        && !opcode.starts_with("cmp")
        && !opcode.starts_with("test")
    {
        let (source, _) = operands
            .rsplit_once(',')
            .ok_or("无法证明 managed asm 的栈指针调整")?;
        if !destination.eq_ignore_ascii_case("%rsp") {
            return Err("managed asm 不能部分覆盖栈指针");
        }
        let source = source.trim();
        return match opcode {
            "add" | "addq" => immediate(source)?.checked_neg().ok_or("汇编栈调整溢出"),
            "sub" | "subq" => immediate(source),
            "lea" | "leaq" => {
                let (offset, base) = source.split_once('(').ok_or("无法证明 lea 的栈指针调整")?;
                if !base.eq_ignore_ascii_case("%rsp)") {
                    return Err("无法证明 lea 的栈指针调整");
                }
                integer(if offset.is_empty() { "0" } else { offset })?
                    .checked_neg()
                    .ok_or("汇编栈调整溢出")
            }
            _ => Err("managed asm 不能执行未知的栈指针修改"),
        };
    }
    Ok(
        if call || matches!(opcode, "push" | "pushq" | "pushf" | "pushfq") {
            8
        } else if matches!(opcode, "pop" | "popq" | "popf" | "popfq") {
            -8
        } else if matches!(opcode, "pushw" | "pushfw") {
            2
        } else if matches!(opcode, "popw" | "popfw") {
            -2
        } else {
            0
        },
    )
}

fn immediate(text: &str) -> Result<i64, &'static str> {
    integer(
        text.strip_prefix('$')
            .ok_or("栈指针调整必须使用整数立即数")?,
    )
}
fn integer(text: &str) -> Result<i64, &'static str> {
    let (negative, text) = text
        .strip_prefix('-')
        .map_or((false, text), |text| (true, text));
    let text = text.strip_prefix('+').unwrap_or(text);
    let (radix, digits) = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .map_or((10, text), |digits| (16, digits));
    let value = i128::from_str_radix(digits, radix).map_err(|_| "无法解析汇编栈调整常量")?;
    let value = if negative {
        value.checked_neg().ok_or("汇编栈调整溢出")?
    } else {
        value
    };
    i64::try_from(value).map_err(|_| "汇编栈调整超出 i64 范围")
}
