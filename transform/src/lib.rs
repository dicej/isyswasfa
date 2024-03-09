use {
    std::{collections::HashMap, iter, path::Path},
    wit_parser::{
        Case, Docs, Field, Function, FunctionKind, Handle, InterfaceId, Record, Resolve, Result_,
        Results, Tuple, Type, TypeDef, TypeDefKind, TypeId, TypeOwner, UnresolvedPackage, Variant,
        WorldId, WorldItem, WorldKey,
    },
};

struct Asyncify<'a> {
    old_resolve: &'a Resolve,
    new_resolve: Resolve,
    pending: TypeId,
    ready: TypeId,
    option_types: HashMap<Type, TypeId>,
    result_types: HashMap<Option<Type>, TypeId>,
    future_types: HashMap<Option<Type>, TypeId>,
    types: HashMap<TypeId, TypeId>,
    input_stream_own_handle: Option<TypeId>,
}

impl<'a> Asyncify<'a> {
    fn asyncify_world_item(
        &mut self,
        key: &WorldKey,
        item: &WorldItem,
        world: WorldId,
    ) -> Vec<(WorldKey, WorldItem)> {
        match item {
            WorldItem::Interface(old) => {
                self.asyncify_interface(*old);
                vec![(key.clone(), item.clone())]
            }
            WorldItem::Function(old) => {
                let name = match key {
                    WorldKey::Name(name) => name,
                    WorldKey::Interface(_) => unreachable!(),
                };

                if let Some((a, b)) = self.asyncify_function(old, TypeOwner::World(world)) {
                    vec![
                        (
                            WorldKey::Name(format!("{name}-isyswasfa-start")),
                            WorldItem::Function(a),
                        ),
                        (
                            WorldKey::Name(format!("{name}-isyswasfa-result")),
                            WorldItem::Function(b),
                        ),
                    ]
                } else {
                    vec![(
                        key.clone(),
                        WorldItem::Function(self.map_function(old, TypeOwner::World(world))),
                    )]
                }
            }
            WorldItem::Type(_) => vec![(key.clone(), item.clone())],
        }
    }

    fn asyncify_interface(&mut self, interface: InterfaceId) {
        let old = &self.old_resolve.interfaces[interface];

        let package = old.package.map(|p| {
            let package = &self.old_resolve.packages[p].name;
            (package.namespace.as_str(), package.name.as_str())
        });

        // TODO: make interface and function include/exclude lists configurable
        let asyncify =
            |function: &Function| match (package, old.name.as_deref(), function.name.as_str()) {
                (_, _, name)
                    if name.ends_with("-isyswasfa-start")
                        || name.ends_with("-isyswasfa-result") =>
                {
                    false
                }
                (Some(("isyswasfa", _)), ..) => false,
                (Some(("wasi", "http")), Some("handler"), _)
                | (Some(("wasi", "http")), Some("types"), "[static]body.finish") => true,
                (Some(("wasi", _)), ..) => false,
                _ => true,
            };

        self.new_resolve.interfaces[interface].functions.clear();

        let functions = old
            .functions
            .iter()
            .flat_map(|(name, function)| {
                if asyncify(function) {
                    if let Some((a, b)) =
                        self.asyncify_function(function, TypeOwner::Interface(interface))
                    {
                        return vec![(a.name.clone(), a), (b.name.clone(), b)];
                    }
                }

                vec![(
                    name.clone(),
                    self.map_function(function, TypeOwner::Interface(interface)),
                )]
            })
            .collect::<Vec<_>>();

        self.new_resolve.interfaces[interface]
            .functions
            .extend(functions);
    }

    fn map_function(&mut self, function: &Function, owner: TypeOwner) -> Function {
        Function {
            name: function.name.clone(),
            kind: function.kind.clone(),
            params: function
                .params
                .iter()
                .map(|(k, v)| (k.clone(), self.map_type(*v, owner)))
                .collect(),
            results: match &function.results {
                Results::Anon(ty) => Results::Anon(self.map_type(*ty, owner)),
                Results::Named(named) => {
                    if named.is_empty() {
                        Results::Named(Vec::new())
                    } else {
                        todo!("handle functions returning multiple named results",)
                    }
                }
            },
            docs: function.docs.clone(),
        }
    }

    fn asyncify_function(
        &mut self,
        function: &Function,
        owner: TypeOwner,
    ) -> Option<(Function, Function)> {
        match &function.kind {
            FunctionKind::Constructor(_) => None,
            FunctionKind::Freestanding | FunctionKind::Static(_) | FunctionKind::Method(_) => {
                let return_ty = match &function.results {
                    Results::Anon(ty) => Some(self.map_type(*ty, owner)),
                    Results::Named(named) => {
                        if named.is_empty() {
                            None
                        } else {
                            todo!("handle functions returning multiple named results",)
                        }
                    }
                };

                Some((
                    Function {
                        name: format!("{}-isyswasfa-start", function.name),
                        kind: function.kind.clone(),
                        params: function
                            .params
                            .iter()
                            .map(|(k, v)| (k.clone(), self.map_type(*v, owner)))
                            .collect(),
                        results: Results::Anon(Type::Id(self.map_result_type(return_ty))),
                        docs: function.docs.clone(),
                    },
                    Function {
                        name: format!(
                            "{}-isyswasfa-result",
                            function.name.replace("[method]", "[static]")
                        ),
                        kind: match &function.kind {
                            FunctionKind::Freestanding | FunctionKind::Static(_) => {
                                function.kind.clone()
                            }
                            FunctionKind::Method(id) => FunctionKind::Static(*id),
                            FunctionKind::Constructor(_) => unreachable!(),
                        },
                        params: vec![("ready".into(), Type::Id(self.ready))],
                        results: return_ty
                            .map(Results::Anon)
                            .unwrap_or_else(|| Results::Named(Vec::new())),
                        docs: function.docs.clone(),
                    },
                ))
            }
        }
    }

    fn map_option_type(&mut self, ty: Type) -> TypeId {
        if let Some(option_ty) = self.option_types.get(&ty) {
            *option_ty
        } else {
            let option_ty = self.new_resolve.types.alloc(TypeDef {
                name: None,
                kind: TypeDefKind::Option(ty),
                owner: TypeOwner::None,
                docs: Docs::default(),
            });

            self.option_types.insert(ty, option_ty);
            option_ty
        }
    }

    fn map_result_type(&mut self, ty: Option<Type>) -> TypeId {
        if let Some(result_ty) = self.result_types.get(&ty) {
            *result_ty
        } else {
            let result_ty = self.new_resolve.types.alloc(TypeDef {
                name: None,
                kind: TypeDefKind::Result(Result_ {
                    ok: ty,
                    err: Some(Type::Id(self.pending)),
                }),
                owner: TypeOwner::None,
                docs: Docs::default(),
            });

            self.result_types.insert(ty, result_ty);
            result_ty
        }
    }

    fn map_type(&mut self, ty: Type, owner: TypeOwner) -> Type {
        if let Type::Id(id) = ty {
            Type::Id(if let Some(id) = self.types.get(&id) {
                *id
            } else {
                let def = &self.old_resolve.types[id];
                let kind = match &def.kind {
                    TypeDefKind::Record(record) => {
                        let mut changed = false;
                        let fields = record
                            .fields
                            .iter()
                            .map(|f| {
                                let ty = self.map_type(f.ty, owner);
                                if ty != f.ty {
                                    changed = true;
                                }
                                Field {
                                    name: f.name.clone(),
                                    ty,
                                    docs: f.docs.clone(),
                                }
                            })
                            .collect();
                        changed.then_some(TypeDefKind::Record(Record { fields }))
                    }
                    TypeDefKind::Tuple(tuple) => {
                        let mut changed = false;
                        let types = tuple
                            .types
                            .iter()
                            .map(|ty| {
                                let new_ty = self.map_type(*ty, owner);
                                if new_ty != *ty {
                                    changed = true;
                                }
                                new_ty
                            })
                            .collect();
                        changed.then_some(TypeDefKind::Tuple(Tuple { types }))
                    }
                    TypeDefKind::Variant(variant) => {
                        let mut changed = false;
                        let cases = variant
                            .cases
                            .iter()
                            .map(|c| {
                                let ty = c.ty.map(|ty| self.map_type(ty, owner));
                                if ty != c.ty {
                                    changed = true;
                                }
                                Case {
                                    name: c.name.clone(),
                                    ty,
                                    docs: c.docs.clone(),
                                }
                            })
                            .collect();
                        changed.then_some(TypeDefKind::Variant(Variant { cases }))
                    }
                    TypeDefKind::Option(ty) => {
                        let new_ty = self.map_type(*ty, owner);
                        (new_ty != *ty).then_some(TypeDefKind::Option(new_ty))
                    }
                    TypeDefKind::Result(result) => {
                        let ok = result.ok.map(|ty| self.map_type(ty, owner));
                        let err = result.err.map(|ty| self.map_type(ty, owner));
                        (ok != result.ok || err != result.err)
                            .then_some(TypeDefKind::Result(Result_ { ok, err }))
                    }
                    TypeDefKind::List(ty) => {
                        let new_ty = self.map_type(*ty, owner);
                        (new_ty != *ty).then_some(TypeDefKind::List(new_ty))
                    }
                    TypeDefKind::Future(ty) => {
                        let ty = ty.map(|ty| self.map_type(ty, owner));
                        let new_id = if let Some(future_ty) = self.future_types.get(&ty) {
                            *future_ty
                        } else {
                            let element_name = self.type_name(ty);

                            let sender_name = format!("isyswasfa-sender-{element_name}");
                            let sender = self.new_resolve.types.alloc(TypeDef {
                                name: Some(sender_name.clone()),
                                kind: TypeDefKind::Resource,
                                owner,
                                docs: Docs::default(),
                            });

                            let sender_handle = self.new_resolve.types.alloc(TypeDef {
                                name: None,
                                kind: TypeDefKind::Handle(Handle::Own(sender)),
                                owner: TypeOwner::None,
                                docs: Docs::default(),
                            });

                            let receiver_name = format!("isyswasfa-receiver-{element_name}");
                            let receiver = self.new_resolve.types.alloc(TypeDef {
                                name: Some(receiver_name.clone()),
                                kind: TypeDefKind::Resource,
                                owner,
                                docs: Docs::default(),
                            });

                            let receiver_handle = self.new_resolve.types.alloc(TypeDef {
                                name: None,
                                kind: TypeDefKind::Handle(Handle::Own(receiver)),
                                owner: TypeOwner::None,
                                docs: Docs::default(),
                            });

                            let pipe_name = format!("isyswasfa-pipe-{element_name}");
                            let pipe = Function {
                                name: pipe_name.clone(),
                                kind: FunctionKind::Freestanding,
                                params: Vec::new(),
                                results: Results::Anon(Type::Id(self.new_resolve.types.alloc(
                                    TypeDef {
                                        name: None,
                                        kind: TypeDefKind::Tuple(Tuple {
                                            types: vec![
                                                Type::Id(sender_handle),
                                                Type::Id(receiver_handle),
                                            ],
                                        }),
                                        owner: TypeOwner::None,
                                        docs: Docs::default(),
                                    },
                                ))),
                                docs: Docs::default(),
                            };

                            let send_name = format!("[static]isyswasfa-sender-{element_name}.send");
                            let send = Function {
                                name: send_name.clone(),
                                kind: FunctionKind::Static(sender),
                                params: iter::once(("sender".into(), Type::Id(sender_handle)))
                                    .chain(ty.map(|ty| ("result".into(), ty)))
                                    .collect(),
                                results: Results::Named(Vec::new()),
                                docs: Docs::default(),
                            };

                            let option_ty = ty.map(|ty| Type::Id(self.map_option_type(ty)));

                            let receive_start_name = format!(
                                "[static]isyswasfa-receiver-{element_name}.receive-isyswasfa-start"
                            );
                            let receive_start = Function {
                                name: receive_start_name.clone(),
                                kind: FunctionKind::Static(receiver),
                                params: vec![("receiver".into(), Type::Id(receiver_handle))],
                                results: Results::Anon(Type::Id(self.map_result_type(option_ty))),
                                docs: Docs::default(),
                            };

                            let receive_result_name =
                                format!("[static]isyswasfa-receiver-{element_name}.receive-isyswasfa-result");
                            let receive_result = Function {
                                name: receive_result_name.clone(),
                                kind: FunctionKind::Static(receiver),
                                params: vec![("ready".into(), Type::Id(self.ready))],
                                results: option_ty
                                    .map(Results::Anon)
                                    .unwrap_or_else(|| Results::Named(Vec::new())),
                                docs: Docs::default(),
                            };

                            match owner {
                                TypeOwner::World(_) => {
                                    todo!("import future types and functions into world")
                                }
                                TypeOwner::Interface(id) => {
                                    let interface = &mut self.new_resolve.interfaces[id];
                                    interface.types.insert(sender_name, sender);
                                    interface.types.insert(receiver_name, receiver);
                                    interface.functions.insert(pipe_name, pipe);
                                    interface.functions.insert(send_name, send);
                                    interface
                                        .functions
                                        .insert(receive_start_name, receive_start);
                                    interface
                                        .functions
                                        .insert(receive_result_name, receive_result);
                                }
                                TypeOwner::None => unreachable!(),
                            }

                            self.future_types.insert(ty, receiver_handle);
                            receiver_handle
                        };

                        self.types.insert(id, new_id);
                        return Type::Id(new_id);
                    }
                    TypeDefKind::Stream(stream) => {
                        if stream.element == Some(Type::U8) && stream.end.is_none() {
                            if let Some(id) = self.input_stream_own_handle {
                                Some(TypeDefKind::Type(Type::Id(id)))
                            } else {
                                panic!("must import `wasi:io/streams` in order to use stream<T>")
                            }
                        } else {
                            todo!("stream<T> for non-u8 types not yet supported; PRs welcome :)")
                        }
                    }
                    TypeDefKind::Type(ty) => {
                        let new_ty = self.map_type(*ty, owner);
                        (new_ty != *ty).then_some(TypeDefKind::Type(new_ty))
                    }
                    TypeDefKind::Enum(_)
                    | TypeDefKind::Flags(_)
                    | TypeDefKind::Handle(_)
                    | TypeDefKind::Resource => None,
                    TypeDefKind::Unknown => unreachable!(),
                };

                if let Some(kind) = kind {
                    let new_id = self.new_resolve.types.alloc(TypeDef {
                        name: def.name.clone(),
                        kind,
                        owner: def.owner,
                        docs: def.docs.clone(),
                    });
                    self.types.insert(id, new_id);
                    new_id
                } else {
                    id
                }
            })
        } else {
            ty
        }
    }

    fn type_name(&self, ty: Option<Type>) -> String {
        if let Some(ty) = ty {
            match ty {
                Type::Bool => "bool".into(),
                Type::U8 => "u8".into(),
                Type::U16 => "u16".into(),
                Type::U32 => "u32".into(),
                Type::U64 => "u64".into(),
                Type::S8 => "s8".into(),
                Type::S16 => "s16".into(),
                Type::S32 => "s32".into(),
                Type::S64 => "s64".into(),
                Type::Float32 => "float32".into(),
                Type::Float64 => "float64".into(),
                Type::Char => "char".into(),
                Type::String => "string".into(),
                Type::Id(id) => {
                    let ty = &self.old_resolve.types[id];
                    if let Some(name) = ty.name.as_deref() {
                        name.into()
                    } else {
                        match &ty.kind {
                            TypeDefKind::Handle(Handle::Borrow(id)) => {
                                format!("borrow-{}", self.type_name(Some(Type::Id(*id))))
                            }
                            TypeDefKind::Handle(Handle::Own(id)) => {
                                format!("own-{}", self.type_name(Some(Type::Id(*id))))
                            }
                            TypeDefKind::Tuple(tuple) => format!(
                                "tuple-{}",
                                tuple
                                    .types
                                    .iter()
                                    .map(|ty| self.type_name(Some(*ty)))
                                    .collect::<Vec<_>>()
                                    .join("-")
                            ),
                            TypeDefKind::Option(ty) => {
                                format!("option-{}", self.type_name(Some(*ty)))
                            }
                            TypeDefKind::Result(result) => format!(
                                "result-{}-{}",
                                self.type_name(result.ok),
                                self.type_name(result.err)
                            ),
                            TypeDefKind::List(ty) => format!("list-{}", self.type_name(Some(*ty))),
                            TypeDefKind::Type(ty) => self.type_name(Some(*ty)),
                            _ => unreachable!(),
                        }
                    }
                }
            }
        } else {
            "unit".into()
        }
    }
}

pub fn transform(resolve: &mut Resolve, world: WorldId, poll_suffix: Option<&str>) {
    *resolve = transform_new(resolve, world, poll_suffix);
}

fn transform_new(resolve: &Resolve, world: WorldId, poll_suffix: Option<&str>) -> Resolve {
    let old_world = &resolve.worlds[world];

    let mut new_resolve = resolve.clone();

    let isyswasfa_package = resolve
        .packages
        .iter()
        .find_map(|(id, p)| {
            matches!(
                (p.name.namespace.as_str(), p.name.name.as_str()),
                ("isyswasfa", "isyswasfa")
            )
            .then_some(id)
        })
        .unwrap_or_else(|| {
            new_resolve
                .push(
                    UnresolvedPackage::parse(
                        Path::new("isyswasfa.wit"),
                        include_str!("../../wit/deps/isyswasfa/isyswasfa.wit"),
                    )
                    .unwrap(),
                )
                .unwrap()
        });

    let isyswasfa_interface = new_resolve.packages[isyswasfa_package].interfaces["isyswasfa"];

    let match_qualified_name = |id, namespace, package_name, interface_name| {
        let interface = &resolve.interfaces[id];
        if let (Some(pid), Some(name)) = (interface.package, &interface.name) {
            let package = &resolve.packages[pid].name;
            package.namespace == namespace && package.name == package_name && name == interface_name
        } else {
            false
        }
    };

    let poll_interface = if old_world
        .imports
        .keys()
        .any(|key| matches!(key, WorldKey::Interface(id) if match_qualified_name(*id, "wasi", "io", "poll")))
    {
        let io_package = resolve
            .packages
            .iter()
            .find_map(|(id, p)| {
                matches!(
                    (p.name.namespace.as_str(), p.name.name.as_str()),
                    ("isyswasfa", "io")
                )
                .then_some(id)
            })
            .unwrap_or_else(|| {
                new_resolve
                    .push(
                        UnresolvedPackage::parse(
                            Path::new("poll.wit"),
                            include_str!("../../wit/deps/isyswasfa-io/poll.wit"),
                        )
                        .unwrap(),
                    )
                    .unwrap()
            });

        Some(new_resolve.packages[io_package].interfaces["poll"])
    } else {
        None
    };

    let input_stream = old_world.imports.keys().find_map(|key| {
        if let WorldKey::Interface(id) = key {
            if match_qualified_name(*id, "wasi", "io", "streams") {
                return Some(resolve.interfaces[*id].types["input-stream"]);
            }
        }

        None
    });

    let input_stream_own_handle = input_stream.map(|id| {
        new_resolve.types.alloc(TypeDef {
            name: None,
            kind: TypeDefKind::Handle(Handle::Own(id)),
            owner: TypeOwner::None,
            docs: Docs::default(),
        })
    });

    let pending = new_resolve.interfaces[isyswasfa_interface].types["pending"];
    let pending = new_resolve.types.alloc(TypeDef {
        name: None,
        kind: TypeDefKind::Handle(Handle::Own(pending)),
        owner: TypeOwner::None,
        docs: Docs::default(),
    });

    let ready = new_resolve.interfaces[isyswasfa_interface].types["ready"];
    let ready = new_resolve.types.alloc(TypeDef {
        name: None,
        kind: TypeDefKind::Handle(Handle::Own(ready)),
        owner: TypeOwner::None,
        docs: Docs::default(),
    });

    let poll_function = poll_suffix.map(|poll_suffix| {
        let poll_input = new_resolve.interfaces[isyswasfa_interface].types["poll-input"];
        let list_poll_input = new_resolve.types.alloc(TypeDef {
            name: None,
            kind: TypeDefKind::List(Type::Id(poll_input)),
            owner: TypeOwner::None,
            docs: Docs::default(),
        });

        let poll_output = new_resolve.interfaces[isyswasfa_interface].types["poll-output"];
        let list_poll_output = new_resolve.types.alloc(TypeDef {
            name: None,
            kind: TypeDefKind::List(Type::Id(poll_output)),
            owner: TypeOwner::None,
            docs: Docs::default(),
        });

        Function {
            name: format!("isyswasfa-poll{poll_suffix}"),
            kind: FunctionKind::Freestanding,
            params: vec![("input".to_owned(), Type::Id(list_poll_input))],
            results: Results::Anon(Type::Id(list_poll_output)),
            docs: Docs::default(),
        }
    });

    let mut asyncify = Asyncify {
        old_resolve: resolve,
        new_resolve,
        pending,
        ready,
        option_types: HashMap::new(),
        result_types: HashMap::new(),
        future_types: HashMap::new(),
        input_stream_own_handle,
        types: HashMap::new(),
    };

    let imports = iter::once((
        WorldKey::Interface(isyswasfa_interface),
        WorldItem::Interface(isyswasfa_interface),
    ))
    .chain(
        old_world
            .imports
            .iter()
            .flat_map(|(key, item)| asyncify.asyncify_world_item(key, item, world)),
    )
    .chain(poll_interface.map(|id| (WorldKey::Interface(id), WorldItem::Interface(id))))
    .collect();

    let exports = old_world
        .exports
        .iter()
        .flat_map(|(key, item)| asyncify.asyncify_world_item(key, item, world))
        .chain(poll_function.map(|poll_function| {
            (
                WorldKey::Name(poll_function.name.clone()),
                WorldItem::Function(poll_function),
            )
        }))
        .collect();

    {
        let new_world = &mut asyncify.new_resolve.worlds[world];
        new_world.imports = imports;
        new_world.exports = exports;
    }

    asyncify.new_resolve
}
