use super::{accept_cases, accepts};

accept_cases! {
    /* file/line/column 在调用点折叠为字面量：普通调用直接定型为 string/int。*/
    std_src_callsite_fold_00: true,
        "use std.src.{file, line, column}\nfn main() { let f = file()\n let l = line()\n let c = column()\n if l < 1 || c < 1 || f == \"\" { panic(\"src\") } else {} }";
    /* 早期 comptime 常量走同一内建求值，位置取自调用表达式 span。*/
    std_src_early_const_01: true,
        "const L: int = std.src.line()\nconst C: int = std.src.column()\nconst F: string = std.src.file()\nfn main() { if L < 1 || C < 1 || F == \"\" { panic(\"src\") } else {} }";
    /* 同一函数里 file() 与 panic 的源路径数据块字节相同：GVN 不得把
       GcHeap/Raw 两种 provenance 的 SymbolAddr 合并。*/
    std_src_file_panic_data_shared_02: true,
        "fn main() { let f = std.src.file()\n if f == \"never-matches\" { panic(\"x\") } else {} }";
    /* 源位置查询不接收参数。*/
    std_src_rejects_arguments_03: false,
        "fn main() { _ = std.src.line(1) }";
    /* embed_file 在函数体运行期位置拒绝：checker 先拦，不走到求值。*/
    std_mem_embed_runtime_position_04: false,
        "fn main() { let b = std.mem.embed_file(\"data.bin\") }";
    /* 内存源码没有编译输入根：const 位置求值时报缺宿主。*/
    std_mem_embed_no_host_05: false,
        "const b: [byte; 1] = std.mem.embed_file(\"data.bin\")\nfn main() {}";
    /* 普通函数体内 caller() 折叠为调用点常量 Location。*/
    std_src_caller_fold_06: true,
        "fn main() { let l = std.src.caller()\n if l.line < 1 || l.column < 1 || l.file == \"\" { panic(\"caller\") } else {} }";
    /* #[track_caller] 函数体内 caller() 读到其调用者的位置：隐藏位置形参
       在调用点注入并由体内 Resolved 转发。*/
    std_src_caller_track_caller_07: true,
        "#[track_caller]\nfn where_line() int { std.src.caller().line }\nfn main() { if where_line() < 1 { panic(\"caller\") } else {} }";
    /* 链式 track_caller：inner 的 caller() 走到 inner 的调用者 outer 的调用点。*/
    std_src_caller_chain_08: true,
        "#[track_caller]\nfn inner() int { std.src.caller().line }\n#[track_caller]\nfn outer() int { inner() }\nfn main() { if outer() < 1 { panic(\"caller\") } else {} }";
    /* track_caller 函数物化为函数值即拒：隐藏实参无法经指针转述。*/
    std_src_track_caller_fn_value_09: false,
        "#[track_caller]\nfn inner() int { std.src.caller().line }\nfn main() { let f = inner\n _ = f() }";
    /* track_caller 函数作实参同样拒。*/
    std_src_track_caller_as_argument_10: false,
        "#[track_caller]\nfn inner() int { std.src.caller().line }\nfn apply(f: fn() int) int { f() }\nfn main() { _ = apply(inner) }";
}
