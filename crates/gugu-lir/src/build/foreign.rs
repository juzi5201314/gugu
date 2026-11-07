//! C 类型在具体布局上分类；保留参数分组，内部值表示不参与平台 ABI 判定。
use super::{Builder, Diagnostic, TypeKind, invalid, storage::integer_type, values::Computed};
use crate::body::{Conversion, ForeignArgument, Op, Origin, Type, ValueId, ValueType, id};
use crate::gir::concrete::TypeLayout;

#[derive(Clone, Debug)]
pub(crate) struct CLayout {
    pub lanes: Vec<(u64, u64, ValueType)>,
    pub align: u32,
    pub memory: bool,
}

/// SysV 最多两个 eightbyte；INTEGER 优先于 SSE，未对齐成员使整个参数成为 MEMORY。
fn merge_classes(types: &[TypeLayout], ty: u32, base: u64, classes: &mut [u8; 2]) -> bool {
    let value = &types[ty as usize];
    let layout = value.layout.expect("C 类型有具体布局");
    if layout.size == 0 {
        return true;
    }
    if !base.is_multiple_of(layout.align) {
        return false;
    }
    match &value.kind {
        TypeKind::Aggregate {
            tag_bytes,
            variants,
        } => {
            if *tag_bytes != 0 {
                classes[(base / 8) as usize] = 2;
            }
            variants
                .iter()
                .flatten()
                .all(|field| merge_classes(types, field.ty, base + field.offset, classes))
        }
        TypeKind::Array { element, count } => {
            let stride = types[*element as usize].layout.expect("数组元素布局").size;
            (0..*count).all(|index| merge_classes(types, *element, base + index * stride, classes))
        }
        _ => {
            let class = if matches!(value.kind, TypeKind::Float(_)) {
                1
            } else {
                2
            };
            for at in base / 8..=(base + layout.size - 1) / 8 {
                classes[at as usize] = classes[at as usize].max(class);
            }
            true
        }
    }
}

pub(crate) fn classify(types: &[TypeLayout], ty: u32, windows: bool) -> CLayout {
    let value = &types[ty as usize];
    let layout = value.layout.expect("C 类型有具体布局");
    let align = u32::try_from(layout.align).expect("C 对齐适配 u32");
    if value.transparent {
        let TypeKind::Aggregate { variants, .. } = &value.kind else {
            unreachable!("transparent 必须是聚合")
        };
        let field = variants
            .iter()
            .flatten()
            .find(|field| {
                types[field.ty as usize]
                    .layout
                    .is_some_and(|layout| layout.size != 0)
            })
            .expect("transparent 有唯一非零字段");
        return classify(types, field.ty, windows);
    }
    let scalar = match value.kind {
        TypeKind::Unit | TypeKind::Never => {
            return CLayout {
                lanes: vec![],
                align,
                memory: false,
            };
        }
        TypeKind::Float(32) => Some(Type::F32),
        TypeKind::Float(64) => Some(Type::F64),
        TypeKind::Bool | TypeKind::Char | TypeKind::Int { bits: 0..=64, .. } => {
            Some(integer_type((layout.size * 8) as u16))
        }
        TypeKind::Pointer(_) => Some(Type::Ptr),
        _ => None,
    };
    if let Some(scalar) = scalar {
        let kind = if scalar == Type::Ptr {
            ValueType::pointer(crate::body::Provenance::Raw)
        } else {
            ValueType::scalar(scalar)
        };
        return CLayout {
            lanes: vec![(0, layout.size, kind)],
            align,
            memory: false,
        };
    }
    let mut classes = [0; 2];
    let memory = if windows {
        !matches!(layout.size, 1 | 2 | 4 | 8)
    } else {
        layout.size > 16 || !merge_classes(types, ty, 0, &mut classes)
    };
    // MEMORY 参数在 SysV 的 outgoing 区按值铺开；Win64 由调用点复制并传地址。
    let lanes = if windows && memory {
        vec![]
    } else {
        (0..layout.size)
            .step_by(8)
            .filter_map(|offset| {
                let bytes = (layout.size - offset).min(8);
                let class = if memory || windows {
                    2
                } else {
                    classes[(offset / 8) as usize]
                };
                let kind = match class {
                    0 => return None,
                    1 if bytes <= 4 => Type::F32,
                    1 => Type::F64,
                    _ if windows => integer_type((bytes * 8) as u16),
                    _ => Type::I64,
                };
                Some((offset, bytes, ValueType::scalar(kind)))
            })
            .collect()
    };
    CLayout {
        lanes,
        align,
        memory,
    }
}

impl Builder<'_> {
    pub(super) fn c_layout(&self, ty: u32) -> CLayout {
        classify(
            &self.concrete.types,
            ty,
            self.body.target == "x86_64-windows",
        )
    }

    pub(super) fn c_argument(
        &mut self,
        value: Computed,
        arguments: &mut Vec<ValueId>,
        groups: &mut Vec<ForeignArgument>,
        by_value: &mut Vec<(u32, [u8; 32], u64)>,
    ) -> Result<(), Diagnostic> {
        let ty = value.ty();
        let layout = self.c_layout(ty);
        let first = id(arguments.len());
        let address = self.computed_address(value)?;
        let value_layout = self.layout(ty).layout.expect("C 参数布局");
        // cage 来源在读取任何 C 参数车道前形成独立值副本；全指针 profile 保留既有路径。
        let address = if self.cage_enabled() && value_layout.size != 0 {
            let copy = self.temporary_bytes(
                value_layout
                    .size
                    .next_multiple_of(value_layout.align.max(16)),
                value_layout.align.max(16),
                self.layout(ty).key,
                vec![],
            )?;
            let bytes = self.constant(value_layout.size, Type::I64);
            self.runtime(
                crate::body::RuntimeCall::ForeignCopy,
                &[copy, address, bytes],
                &[],
            )?;
            copy
        } else {
            address
        };
        if layout.memory && self.body.target == "x86_64-windows" {
            let value_layout = self.layout(ty).layout.expect("C 参数布局");
            let copy = if self.cage_enabled() {
                address
            } else {
                let copy = self.temporary_bytes(
                    value_layout
                        .size
                        .next_multiple_of(value_layout.align.max(16)),
                    value_layout.align.max(16),
                    self.layout(ty).key,
                    vec![],
                )?;
                self.copy_memory(copy, address, ty)?;
                copy
            };
            arguments.push(copy);
            by_value.push((
                first,
                self.layout(ty).key,
                self.layout(ty).layout.expect("C 参数布局").size,
            ));
        } else {
            for &(offset, bytes, kind) in &layout.lanes {
                let source = self.offset(address, offset);
                let value = if kind.ty.bytes() == Some(bytes) {
                    self.load(source, kind, layout.align.min(bytes as u32), false)
                } else {
                    let bits = self.load_bits(source, bytes);
                    if matches!(kind.ty, Type::F32 | Type::F64) {
                        let bits = self.integer_resize(
                            bits,
                            if kind.ty == Type::F32 {
                                Type::I32
                            } else {
                                Type::I64
                            },
                            false,
                        );
                        self.emit_one(
                            Op::Convert(Conversion::Bitcast),
                            &[bits],
                            kind,
                            Origin::None,
                        )
                    } else {
                        bits
                    }
                };
                arguments.push(value);
            }
        }
        let lanes = id(arguments.len()) - first;
        if lanes != 0 {
            groups.push(ForeignArgument {
                first,
                lanes,
                bytes: u32::try_from(self.layout(ty).layout.expect("C 参数布局").size)
                    .map_err(|_| invalid("C 参数大小超过平台栈偏移范围"))?,
                align: layout.align,
                memory: layout.memory,
            });
        }
        Ok(())
    }

    pub(super) fn c_result(&mut self, ty: u32, values: &[ValueId]) -> Result<Computed, Diagnostic> {
        let layout = self.c_layout(ty);
        if values.len() != layout.lanes.len() {
            return Err(invalid("C 返回车道不匹配"));
        }
        let address = self.temporary(ty)?;
        self.zero_object(address, ty)?;
        for ((offset, bytes, _), &value) in layout.lanes.into_iter().zip(values) {
            let destination = self.offset(address, offset);
            if self.machine_type(value).ty.bytes() == Some(bytes) {
                self.store(destination, value, layout.align.min(bytes as u32), false);
            } else {
                self.store_bits(destination, value, bytes);
            }
        }
        Ok(Computed::Address { ty, address })
    }
}
