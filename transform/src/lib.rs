use {
    std::{iter, path::Path},
    wit_parser::{
        Docs, Function, FunctionKind, Handle, InterfaceId, Resolve, Result_, Results, Type,
        TypeDef, TypeDefKind, TypeId, TypeOwner, UnresolvedPackage, WorldId, WorldItem, WorldKey,
    },
};

struct Asyncify<'a> {
    old_resolve: &'a Resolve,
    new_resolve: Resolve,
    pending: TypeId,
    ready: TypeId,
}

impl<'a> Asyncify<'a> {
    fn asyncify_world_item(
        &mut self,
        key: &WorldKey,
        item: &WorldItem,
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

                if let Some((a, b)) = self.asyncify_function(old) {
                    vec![
                        (
                            WorldKey::Name(format!("{name}-isyswasfa")),
                            WorldItem::Function(a),
                        ),
                        (
                            WorldKey::Name(format!("{name}-isyswasfa-result")),
                            WorldItem::Function(b),
                        ),
                    ]
                } else {
                    vec![(key.clone(), item.clone())]
                }
            }
            WorldItem::Type(_) => vec![(key.clone(), item.clone())],
        }
    }

    fn asyncify_interface(&mut self, interface: InterfaceId) {
        let old = &self.old_resolve.interfaces[interface];

        // TODO: make interface and function include/exclude lists configurable
        if let Some("wasi") = old
            .package
            .map(|p| self.old_resolve.packages[p].name.namespace.as_str())
        {
            return;
        }

        let functions = old
            .functions
            .iter()
            .flat_map(|(name, function)| {
                if let Some((a, b)) = self.asyncify_function(function) {
                    vec![(a.name.clone(), a), (b.name.clone(), b)]
                } else {
                    vec![(name.clone(), function.clone())]
                }
            })
            .collect();

        self.new_resolve.interfaces[interface].functions = functions;
    }

    fn asyncify_function(&mut self, function: &Function) -> Option<(Function, Function)> {
        match &function.kind {
            FunctionKind::Constructor(_) => None,
            FunctionKind::Freestanding | FunctionKind::Static(_) | FunctionKind::Method(_) => {
                Some((
                    Function {
                        name: format!("{}-isyswasfa", function.name),
                        kind: function.kind.clone(),
                        params: function.params.clone(),
                        results: Results::Anon(Type::Id(self.new_resolve.types.alloc(TypeDef {
                            name: None,
                            kind: TypeDefKind::Result(Result_ {
                                ok: match &function.results {
                                    Results::Anon(ty) => Some(*ty),
                                    Results::Named(named) => {
                                        if named.is_empty() {
                                            None
                                        } else {
                                            todo!(
                                                "handle functions returning multiple named results",
                                            )
                                        }
                                    }
                                },
                                err: Some(Type::Id(self.pending)),
                            }),
                            owner: TypeOwner::None,
                            docs: Docs::default(),
                        }))),
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
                        results: function.results.clone(),
                        docs: function.docs.clone(),
                    },
                ))
            }
        }
    }
}

pub fn transform(resolve: &mut Resolve, world: WorldId, poll_suffix: Option<&str>) {
    *resolve = transform_new(resolve, world, poll_suffix);
}

fn transform_new(resolve: &Resolve, world: WorldId, poll_suffix: Option<&str>) -> Resolve {
    let old_world = &resolve.worlds[world];

    let mut new_resolve = resolve.clone();

    let isyswasfa_package = new_resolve
        .push(
            UnresolvedPackage::parse(
                &Path::new("isyswasfa.wit"),
                include_str!("../../wit/deps/isyswasfa/isyswasfa.wit"),
            )
            .unwrap(),
        )
        .unwrap();

    let isyswasfa_interface = new_resolve.packages[isyswasfa_package].interfaces["isyswasfa"];

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
    };

    let imports = iter::once((
        WorldKey::Interface(isyswasfa_interface),
        WorldItem::Interface(isyswasfa_interface),
    ))
    .chain(
        old_world
            .imports
            .iter()
            .flat_map(|(key, item)| asyncify.asyncify_world_item(key, item)),
    )
    .collect();

    let exports = old_world
        .exports
        .iter()
        .flat_map(|(key, item)| asyncify.asyncify_world_item(key, item))
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
