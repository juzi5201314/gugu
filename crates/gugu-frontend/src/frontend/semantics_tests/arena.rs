use super::{accept_cases, accepts};
accept_cases! {
    /* LocalArena 全接口：构造、`alloc[T]` 结果可直接解引用，reset/destroy 走 unsafe。*/
    std_mem_local_arena_accept_00: true,
        "fn main() { let a = std.mem.LocalArena::new()\n let p = a.alloc[int](41)\n *p = 42\n unsafe { a.reset()\n a.destroy() } }";
    /* with_capacity 与 new 共用同一构造通道。*/
    std_mem_local_arena_capacity_01: true,
        "fn main() { let a = std.mem.LocalArena::with_capacity(4096)\n unsafe { a.destroy() } }";
    /* SyncArena 接口形状相同，内部并发推进。*/
    std_mem_sync_arena_accept_02: true,
        "fn main() { let a = std.mem.SyncArena::new()\n let p = a.alloc[uint](7)\n _ = *p\n unsafe { a.reset()\n a.destroy() } }";
    /* alloc 元素是聚合类型时按值落槽。*/
    std_mem_arena_alloc_aggregate_03: true,
        "fn main() { let a = std.mem.LocalArena::new()\n let p = a.alloc[string](\"hi\")\n if *p != \"hi\" { panic(\"arena\") } else {} }";
    /* 类型实参缺省时由值实参反解。*/
    std_mem_arena_alloc_inferred_04: true,
        "fn main() { let a = std.mem.LocalArena::new()\n _ = a.alloc(1) }";
    /* 容量字面量为负在编译期拒绝。*/
    std_mem_arena_negative_capacity_05: false,
        "fn main() { let a = std.mem.LocalArena::with_capacity(-1)\n _ = a }";
    /* 含 resource 字段的元素类型在 GIR 层拒绝：arena 槽不保证析构序。*/
    std_mem_arena_rejects_resource_06: false,
        "use std.mem.{LocalArena}\nuse std.fs.{File}\nstruct WithRes { cell: File }\nfn put(a: LocalArena, v: WithRes) { _ = a.alloc[WithRes](v) }\nfn main() {}";
    /* reset 是 unsafe fn：离开 unsafe 块调用必须拒绝。*/
    std_mem_arena_reset_requires_unsafe_07: false,
        "fn main() { let a = std.mem.LocalArena::new()\n a.reset() }";
    /* destroy 同样要求 unsafe。*/
    std_mem_arena_destroy_requires_unsafe_08: false,
        "fn main() { let a = std.mem.SyncArena::new()\n a.destroy() }";
    /* 句柄按值复制仍指向同一区域：副本上的 alloc 走同一 cursor。*/
    std_mem_arena_handle_copy_09: true,
        "use std.mem.{LocalArena}\nfn put(a: LocalArena) { _ = a.alloc[int](1) }\nfn main() { let a = LocalArena::new()\n put(a)\n unsafe { a.destroy() } }";
}
