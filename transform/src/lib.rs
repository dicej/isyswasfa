use {
    indexmap::IndexMap,
    std::{collections::HashMap, iter, path::Path},
    wit_parser::{
        Docs, Function, FunctionKind, Handle, Interface, InterfaceId, Resolve, Result_, Results,
        Type, TypeDef, TypeDefKind, TypeId, TypeOwner, UnresolvedPackage, World, WorldId,
        WorldItem, WorldKey,
    },
};

struct Asyncify<'a> {
    old_resolve: &'a Resolve,
    new_resolve: Resolve,
    pending: TypeId,
    ready: TypeId,
    interfaces: HashMap<InterfaceId, InterfaceId>,
    functions: HashMap<WorldKey, (Function, Function)>,
}

impl<'a> Asyncify<'a> {
    fn asyncify_world_item(
        &mut self,
        key: &WorldKey,
        item: &WorldItem,
    ) -> Vec<(WorldKey, WorldItem)> {
        let mut new_key = || match key {
            WorldKey::Name(name) => WorldKey::Name(name.clone()),
            WorldKey::Interface(old) => {
                WorldKey::Interface(if let Some(new) = self.interfaces.get(old).copied() {
                    new
                } else {
                    let new = self.asyncify_interface(*old);
                    self.interfaces.insert(*old, new);
                    new
                })
            }
        };

        match item {
            WorldItem::Interface(old) => {
                vec![(
                    new_key(),
                    WorldItem::Interface(if let Some(new) = self.interfaces.get(old).copied() {
                        new
                    } else {
                        let new = self.asyncify_interface(*old);
                        self.interfaces.insert(*old, new);
                        new
                    }),
                )]
            }
            WorldItem::Function(old) => {
                let new_key = |suffix| match key {
                    WorldKey::Name(name) => WorldKey::Name(format!("{name}{suffix}")),
                    WorldKey::Interface(_) => unreachable!(),
                };

                let new = if let Some(new) = self.functions.get(key).cloned() {
                    Some(new)
                } else {
                    if let Some(new) = self.asyncify_function(old) {
                        self.functions.insert(key.clone(), new.clone());
                        Some(new)
                    } else {
                        None
                    }
                };

                if let Some((a, b)) = new {
                    vec![
                        (new_key("-isyswasfa"), WorldItem::Function(a)),
                        (new_key("-isyswasfa-result"), WorldItem::Function(b)),
                    ]
                } else {
                    vec![(key.clone(), item.clone())]
                }
            }
            WorldItem::Type(old) => vec![(new_key(), WorldItem::Type(*old))],
        }
    }

    fn asyncify_interface(&mut self, interface: InterfaceId) -> InterfaceId {
        let old = &self.old_resolve.interfaces[interface];
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

        self.new_resolve.interfaces.alloc(Interface {
            name: old.name.clone(),
            types: old.types.clone(),
            functions,
            docs: old.docs.clone(),
            package: old.package,
        })
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
                        results: match &function.results {
                            Results::Anon(ty) => {
                                Results::Anon(Type::Id(self.new_resolve.types.alloc(TypeDef {
                                    name: None,
                                    kind: TypeDefKind::Result(Result_ {
                                        ok: Some(*ty),
                                        err: Some(Type::Id(self.pending)),
                                    }),
                                    owner: TypeOwner::None,
                                    docs: Docs::default(),
                                })))
                            }
                            Results::Named(_) => {
                                todo!("handle functions returning multiple named results")
                            }
                        },
                        docs: function.docs.clone(),
                    },
                    Function {
                        name: format!("{}-isyswasfa-result", function.name),
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

pub fn transform(
    resolve: &Resolve,
    world: WorldId,
    poll_suffix: Option<&str>,
) -> (Resolve, WorldId) {
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

    let new_world = new_resolve.worlds.alloc(World {
        name: old_world.name.clone(),
        imports: IndexMap::new(),
        exports: IndexMap::new(),
        package: old_world.package,
        docs: old_world.docs.clone(),
        includes: Vec::new(),
        include_names: Vec::new(),
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
        interfaces: HashMap::new(),
        functions: HashMap::new(),
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
        .imports
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
        let new_world = &mut asyncify.new_resolve.worlds[new_world];
        new_world.imports = imports;
        new_world.exports = exports;
    }

    (asyncify.new_resolve, new_world)
}
