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
}
