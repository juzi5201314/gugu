use super::{
    Diagnostic, Field, InstanceSummaryV1, Layout, MonoContext, PassingClass, TypeKind, TypeLayout,
    hir, id, invalid,
};
use crate::frontend::semantics::Ty;
use crate::frontend::types::Layouts;
use std::collections::BTreeMap;

pub(super) struct Builder<'a, 'm> {
    pub(super) context: &'a MonoContext<'m>,
    pub(super) instance: &'a InstanceSummaryV1,
    layouts: Layouts<'a, 'm>,
    indices: BTreeMap<Ty, u32>,
    types: Vec<Option<TypeLayout>>,
    bindings: Vec<Option<u32>>,
}

impl<'a, 'm> Builder<'a, 'm> {
    pub(super) fn new(context: &'a MonoContext<'m>, instance: &'a InstanceSummaryV1) -> Self {
        Self {
            context,
            instance,
            layouts: Layouts::new(context.model, context.checked),
            indices: BTreeMap::new(),
            types: Vec::new(),
            bindings: vec![None; context.module.types.len()],
        }
    }

    pub(super) fn source(&mut self, source: hir::TypeId) -> Result<hir::TypeId, Diagnostic> {
        if let Some(bound) = self.bindings[source.index()] {
            return Ok(hir::TypeId(bound));
        }
        let ty = self.context.type_at(source, &self.instance.substitutions)?;
        let bound = self.intern(&ty)?;
        self.bindings[source.index()] = Some(bound);
        Ok(hir::TypeId(bound))
    }

    pub(super) fn address_type(&mut self, source: hir::TypeId) -> Result<hir::TypeId, Diagnostic> {
        let value = self.context.type_at(source, &self.instance.substitutions)?;
        self.intern(&Ty::Ptr(Box::new(value))).map(hir::TypeId)
    }

    pub(super) fn finish(self) -> Result<Vec<TypeLayout>, Diagnostic> {
        self.types
            .into_iter()
            .map(|ty| ty.ok_or_else(|| invalid("具体类型递归未闭合")))
            .collect()
    }

    pub(super) fn intern(&mut self, ty: &Ty) -> Result<u32, Diagnostic> {
        let ty = crate::frontend::mono::universe::concrete(self.context, ty)?;
        if let Some(&index) = self.indices.get(&ty) {
            return Ok(index);
        }
        let index = id(self.types.len());
        self.indices.insert(ty.clone(), index);
        self.types.push(None);
        let layout = self.layouts.layout(&ty)?;
        let key = if let Ty::Callable(callable, _, _) = &ty {
            self.context.module.definitions[self.context.identities.function(*callable).index()].key
        } else {
            crate::frontend::mono::keys::hash_domain(
                "gugu-mono-v1",
                &self.context.encode_type(&ty)?,
            )
        };
        let (kind, passing) = self.kind(&ty, layout)?;
        self.types[super::index(index)] = Some(TypeLayout {
            key,
            layout,
            kind,
            passing,
        });
        Ok(index)
    }

    fn kind(
        &mut self,
        ty: &Ty,
        layout: Option<Layout>,
    ) -> Result<(TypeKind, PassingClass), Diagnostic> {
        let bits = PassingClass::BITS;
        let identity = PassingClass::IDENTITY;
        Ok(match ty {
            Ty::Never => (TypeKind::Never, bits),
            Ty::Unit => (TypeKind::Unit, bits),
            Ty::Bool => (TypeKind::Bool, bits),
            Ty::Char => (TypeKind::Char, bits),
            Ty::Int {
                signed,
                bits: width,
            } => (
                TypeKind::Int {
                    signed: *signed,
                    bits: *width,
                },
                bits,
            ),
            Ty::Float(width) => (TypeKind::Float(*width), bits),
            Ty::TypeId => (TypeKind::TypeId, bits),
            Ty::String => (TypeKind::String, PassingClass::COW),
            Ty::Ref(inner) => (TypeKind::Reference(self.intern(inner)?), identity),
            Ty::Ptr(inner) => (TypeKind::Pointer(self.intern(inner)?), bits),
            Ty::Slice(inner) => (TypeKind::Slice(self.intern(inner)?), identity),
            Ty::Chan(inner) => (TypeKind::Channel(self.intern(inner)?), identity),
            Ty::Join(inner) => (TypeKind::Join(self.intern(inner)?), identity),
            Ty::Panic => (TypeKind::Panic, identity),
            Ty::MaybeUninit(inner) => {
                let inner = self.intern(inner)?;
                (TypeKind::MaybeUninit(inner), self.passing(inner)?)
            }
            Ty::Array(element, count) => {
                let element = self.intern(element)?;
                let passing = if *count == 0 {
                    bits
                } else {
                    self.passing(element)?
                };
                (
                    TypeKind::Array {
                        element,
                        count: *count,
                    },
                    passing,
                )
            }
            Ty::Tuple(_)
            | Ty::Named(..)
            | Ty::Option(_)
            | Ty::Result(..)
            | Ty::Range
            | Ty::ChanClosed
            | Ty::TrySendErr
            | Ty::TryRecvErr => self.aggregate(ty)?,
            Ty::Callable(callable, arguments, _) => {
                let arguments = arguments
                    .iter()
                    .filter(|ty| crate::frontend::mono::universe::is_concrete(ty))
                    .map(|ty| {
                        self.context.encode_type(ty).map(|bytes| {
                            crate::frontend::mono::keys::hash_domain("gugu-mono-v1", &bytes)
                        })
                    })
                    .collect::<Result<_, _>>()?;
                (
                    TypeKind::FunctionItem {
                        definition: self.context.identities.function(*callable).0,
                        capturing: layout.is_some_and(|layout| layout.size != 0),
                        arguments,
                    },
                    identity,
                )
            }
            Ty::Function(parameters, result) => {
                let parameters = parameters
                    .iter()
                    .map(|ty| self.intern(ty))
                    .collect::<Result<_, _>>()?;
                (
                    TypeKind::Function {
                        parameters,
                        result: self.intern(result)?,
                    },
                    identity,
                )
            }
            Ty::Dyn(_) => (TypeKind::Dynamic, identity),
            Ty::Error | Ty::Var(_) | Ty::Param(_) | Ty::Projection(..) | Ty::Opaque(..) => {
                return Err(invalid("具体 GIR 中仍有未替换类型"));
            }
        })
    }

    fn passing(&self, index: u32) -> Result<PassingClass, Diagnostic> {
        self.types[super::index(index)]
            .as_ref()
            .map(|ty| ty.passing)
            .ok_or_else(|| invalid("值布局形成递归；引用应当先终止类别递归"))
    }

    fn aggregate(&mut self, ty: &Ty) -> Result<(TypeKind, PassingClass), Diagnostic> {
        let layout = self.layouts.aggregate_layout(ty)?;
        let mut passing = PassingClass::BITS;
        let mut variants = Vec::with_capacity(layout.variants.len());
        for variant in layout.variants {
            let mut fields = Vec::with_capacity(variant.len());
            for (ty, offset) in variant {
                let ty = self.intern(&ty)?;
                passing = passing.union(self.passing(ty)?);
                fields.push(Field { ty, offset });
            }
            variants.push(fields);
        }
        if let Ty::Named(nominal, _) = ty {
            let definition = self
                .context
                .identities
                .item(self.context.model.nominal[*nominal].definition);
            if let Some(class) = super::super::passing::lang_item(self.context.module, definition) {
                passing = class;
            }
        }
        Ok((
            TypeKind::Aggregate {
                tag_bytes: layout
                    .tag
                    .map_or(0, |tag| u8::try_from(tag.size).expect("tag 不超过 16 字节")),
                variants,
            },
            passing,
        ))
    }
}
