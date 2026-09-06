//! 标准原语按规范路径识别，导入别名和再导出保留同一身份。
use super::{Model, ResolvedTarget};

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum MemoryIntrinsic {
    AddrOf,
    PtrRead,
    PtrWrite,
    ReadUnaligned,
    WriteUnaligned,
    VolatileLoad,
    VolatileStore,
    Transmute,
    Unreachable,
    Uninit,
    UninitNew,
    UninitAsPtr,
    UninitWrite,
    AssumeInit,
    PointerCast,
    ScalarCast,
}

impl MemoryIntrinsic {
    pub(in super::super) fn from_path(path: &str) -> Option<Self> {
        Some(match path {
            "std.ptr.addr_of" => Self::AddrOf,
            "std.ptr.ptr_read" => Self::PtrRead,
            "std.ptr.ptr_write" => Self::PtrWrite,
            "std.ptr.read_unaligned" => Self::ReadUnaligned,
            "std.ptr.write_unaligned" => Self::WriteUnaligned,
            "std.ptr.volatile_load" => Self::VolatileLoad,
            "std.ptr.volatile_store" => Self::VolatileStore,
            "std.mem.transmute" => Self::Transmute,
            "std.hint.unreachable" => Self::Unreachable,
            "std.mem.MaybeUninit.uninit" => Self::Uninit,
            "std.mem.MaybeUninit.new" => Self::UninitNew,
            "std.mem.MaybeUninit.as_ptr" => Self::UninitAsPtr,
            "std.mem.MaybeUninit.write" => Self::UninitWrite,
            "std.mem.MaybeUninit.assume_init" => Self::AssumeInit,
            _ => return None,
        })
    }
}

impl Model<'_> {
    pub(in super::super) fn external_path(&self, module: usize, path: &[&str]) -> Option<String> {
        self.external_path_inner(module, path, false)
    }

    fn external_path_inner(&self, module: usize, path: &[&str], exported: bool) -> Option<String> {
        if path.first() == Some(&"std") {
            return Some(path.join("."));
        }
        let first = path.first()?;
        for import in &self.names.imports {
            if import.module.index() != module
                || import.alias != *first
                || exported && !import.public
            {
                continue;
            }
            match &import.target {
                ResolvedTarget::External(prefix) => {
                    let mut result = prefix.clone();
                    for segment in &path[1..] {
                        result.push('.');
                        result.push_str(segment);
                    }
                    return Some(result);
                }
                ResolvedTarget::Module(target) if path.len() > 1 => {
                    return self.external_path_inner(target.index(), &path[1..], true);
                }
                _ => {}
            }
        }
        None
    }
}
