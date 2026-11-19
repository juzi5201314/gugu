//! 显式 native 链接输入；顺序属于 loader 查找语义，规范编码保留声明顺序。
use serde::{Deserialize, Serialize};

/// 数据导入不能作为普通外调入口；Library 只声明 DT_NEEDED/导入目录条目，
/// 不产生待解析符号。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NativeImportKind {
    Function,
    Data,
    Library,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeImport {
    pub library: String,
    pub name: String,
    pub kind: NativeImportKind,
}

impl NativeImport {
    pub fn function(library: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            library: library.into(),
            name: name.into(),
            kind: NativeImportKind::Function,
        }
    }

    pub fn data(library: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            library: library.into(),
            name: name.into(),
            kind: NativeImportKind::Data,
        }
    }

    /// 仅声明对某个动态库的装载依赖；`name` 与 `library` 相同以通过非空校验，
    /// 但后端不为它生成符号或重定位。
    pub fn library(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            library: name.clone(),
            name,
            kind: NativeImportKind::Library,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeLink {
    /// 按声明顺序登记库、符号与种类；默认不添加用户库。
    pub imports: Vec<NativeImport>,
    /// Linux 动态解释器由调用方的目标工具链提供，禁止探测宿主。
    pub interpreter: Option<String>,
}

impl NativeLink {
    /// 内建网络需求在 action key 形成前合并，显式导入仍保留原顺序和冲突校验。
    pub fn with_standard_library(
        mut self,
        target: crate::TargetName,
        lir: &crate::lir::Validated,
    ) -> Self {
        if target == crate::TargetName::X86_64Macos {
            return self;
        }
        let network = lir.bodies().iter().any(|body| {
            body.instructions.iter().any(|instruction| {
                matches!(
                    instruction.op,
                    crate::lir::body::Op::RuntimeMachineCall(
                        crate::runtime::RuntimeMachineOp::NetCall
                    )
                )
            })
        });
        if network && target == crate::TargetName::X86_64Windows {
            for &name in crate::x64::fs_native::net::WINDOWS_SYMBOLS {
                self.add_system_import("ws2_32.dll", name);
            }
            for name in [
                "CreateIoCompletionPort",
                "GetQueuedCompletionStatus",
                "CancelIoEx",
            ] {
                self.add_system_import("kernel32.dll", name);
            }
        }
        let resolver = lir.bodies().iter().any(|body| {
            body.instructions.iter().any(|instruction| {
                matches!(
                    instruction.op,
                    crate::lir::body::Op::RuntimeMachineCall(
                        crate::runtime::RuntimeMachineOp::ResolverSymbol
                    )
                )
            })
        });
        if resolver {
            let library = if target == crate::TargetName::X86_64Linux {
                "libc.so.6"
            } else {
                "ws2_32.dll"
            };
            for symbol in ["getaddrinfo", "freeaddrinfo"] {
                self.add_system_import(library, symbol);
            }
            if target == crate::TargetName::X86_64Linux {
                if self.interpreter.is_none() {
                    self.interpreter = target.descriptor().linux_interpreter.map(str::to_owned);
                }
                for symbol in Self::THREAD_SYMBOLS {
                    self.add_system_import(library, symbol);
                }
            }
        }
        let environment = lir.bodies().iter().any(|body| {
            body.instructions.iter().any(|instruction| {
                matches!(
                    instruction.op,
                    crate::lir::body::Op::RuntimeMachineCall(
                        crate::runtime::RuntimeMachineOp::EnvironmentSymbol
                    )
                )
            })
        });
        if environment {
            if target == crate::TargetName::X86_64Linux {
                if !self.imports.iter().any(|import| {
                    import.name == "environ"
                        && import.library == "libc.so.6"
                        && import.kind == NativeImportKind::Data
                }) {
                    self.imports
                        .push(NativeImport::data("libc.so.6", "environ"));
                }
                for &name in &crate::x64::fs_native::environment::LINUX_SYMBOLS[1..] {
                    self.add_system_import("libc.so.6", name);
                }
                for name in Self::THREAD_SYMBOLS {
                    self.add_system_import("libc.so.6", name);
                }
                if self.interpreter.is_none() {
                    self.interpreter = target.descriptor().linux_interpreter.map(str::to_owned);
                }
            } else {
                for &name in crate::x64::fs_native::environment::WINDOWS_SYMBOLS {
                    self.add_system_import("kernel32.dll", name);
                }
            }
        }
        let process = lir.bodies().iter().any(|body| {
            body.instructions.iter().any(|instruction| {
                matches!(
                    instruction.op,
                    crate::lir::body::Op::RuntimeMachineCall(
                        crate::runtime::RuntimeMachineOp::ProcessWorker
                            | crate::runtime::RuntimeMachineOp::ProcessCall
                    )
                )
            })
        });
        if process {
            if target == crate::TargetName::X86_64Linux {
                for name in Self::THREAD_SYMBOLS {
                    self.add_system_import("libc.so.6", name);
                }
                if self.interpreter.is_none() {
                    self.interpreter = target.descriptor().linux_interpreter.map(str::to_owned);
                }
            } else {
                for &name in crate::x64::fs_native::process::WINDOWS_SYMBOLS {
                    self.add_system_import("kernel32.dll", name);
                }
            }
        }
        self
    }

    fn add_system_import(&mut self, library: &str, symbol: &str) {
        if !self.imports.iter().any(|import| {
            import.library.eq_ignore_ascii_case(library)
                && import.name == symbol
                && import.kind == NativeImportKind::Function
        }) {
            self.imports.push(NativeImport::function(library, symbol));
        }
    }

    /// x86_64 glibc 线程入口需要的符号；静态 syscall 文件 worker 不消费这些符号。
    pub const THREAD_SYMBOLS: [&'static str; 6] = [
        "pthread_create",
        "pthread_tryjoin_np",
        "pthread_attr_init",
        "pthread_attr_setstack",
        "pthread_attr_destroy",
        "__errno_location",
    ];

    pub(crate) fn validate_bridge(
        &self,
        target: crate::TargetName,
        world: &crate::x64::codegen::X64World,
    ) -> Result<(), String> {
        if target != crate::TargetName::X86_64Linux
            || !["foreign_dispatch", "foreign_dirty_dispatch"]
                .iter()
                .any(|name| {
                    world
                        .glues()
                        .is_bound_symbol(&crate::x64::mangle::mangle_runtime(name))
                })
        {
            return Ok(());
        }
        if self.interpreter.is_none() {
            return Err(
                "普通 Linux ForeignBridge 需要显式配置目标 glibc 解释器及 pthread 导入".into(),
            );
        }
        for name in Self::THREAD_SYMBOLS {
            if !self
                .imports
                .iter()
                .any(|import| import.name == name && import.kind == NativeImportKind::Function)
            {
                return Err(format!(
                    "普通 Linux ForeignBridge 缺少目标 glibc 导入 {name}"
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_bridge_targets(
        &self,
        lir: &crate::lir::Validated,
    ) -> Result<(), String> {
        use crate::lir::body::{CallTarget, Op, Terminator};
        for body in lir.bodies() {
            let instructions =
                body.instructions
                    .iter()
                    .filter_map(|instruction| match &instruction.op {
                        Op::Call(call) | Op::ForeignCall(call) => Some(call),
                        _ => None,
                    });
            let terminators = body
                .blocks
                .iter()
                .filter_map(|block| match &block.terminator {
                    Terminator::Invoke { call, .. } | Terminator::TailCall { call, .. } => {
                        Some(call)
                    }
                    _ => None,
                });
            for call in instructions.chain(terminators) {
                if crate::x64::lower::foreign::dispatcher(call.kind).is_some()
                    && let CallTarget::External { name, .. } = &call.target
                    && !self.imports.iter().any(|import| {
                        &import.name == name && import.kind == NativeImportKind::Function
                    })
                    && !lir.exports().iter().any(|export| &export.name == name)
                {
                    return Err(format!("ForeignBridge 目标 {name} 缺少显式 native 导入"));
                }
            }
        }
        Ok(())
    }

    /// 保留 loader 查找顺序的缓存输入。
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::canonical::to_vec(self).expect("native 链接输入可规范编码")
    }

    /// 校验目标相关输入；不读取宿主文件系统。
    pub fn validate(&self, target: crate::TargetName) -> Result<(), String> {
        let mut symbols = std::collections::BTreeSet::new();
        for NativeImport {
            library,
            name: symbol,
            kind,
        } in &self.imports
        {
            if library.is_empty()
                || symbol.is_empty()
                || library.contains('\0')
                || symbol.contains('\0')
            {
                return Err("native 导入名为空或包含 NUL".into());
            }
            if *kind == NativeImportKind::Library {
                continue;
            }
            if !symbols.insert(symbol) {
                return Err(format!("native 符号 {symbol} 重复导入"));
            }
        }
        match (target, self.interpreter.as_deref(), self.imports.is_empty()) {
            (crate::TargetName::X86_64Linux, None, false) => Err("动态导入缺少解释器路径".into()),
            (crate::TargetName::X86_64Linux, Some(path), _)
                if !path.starts_with('/') || path.contains('\0') =>
            {
                Err("动态解释器必须是无 NUL 的绝对目标路径".into())
            }
            (crate::TargetName::X86_64Windows, Some(_), _) => {
                Err("Windows 链接不接受 ELF 解释器".into())
            }
            _ => Ok(()),
        }
    }
}
