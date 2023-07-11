use heck::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::mem;
use std::process::{Command, Stdio};
use wit_bindgen_core::wit_parser::abi::{AbiVariant, Bindgen, Instruction, LiftLower, WasmType};
use wit_bindgen_core::{
    uwrite, uwriteln, wit_parser::*, Files, InterfaceGenerator as _, Source, TypeInfo, Types,
    WorldGenerator,
};
use wit_bindgen_rust_lib::{
    int_repr, to_rust_ident, wasm_type, FnSig, Ownership, RustFlagsRepr, RustFunctionGenerator,
    RustGenerator, TypeMode,
};

#[derive(Default, Copy, Clone, PartialEq, Eq)]
enum Direction {
    #[default]
    Import,
    Export,
}

#[derive(Default)]
struct ResourceInfo {
    direction: Direction,
    own: Option<TypeId>,
    docs: Docs,
}

#[derive(Default)]
struct RustWasm {
    types: Types,
    src: Source,
    opts: Opts,
    import_modules: BTreeMap<Option<PackageName>, Vec<String>>,
    export_modules: BTreeMap<Option<PackageName>, Vec<String>>,
    skip: HashSet<String>,
    interface_names: HashMap<InterfaceId, String>,
    resources: HashMap<TypeId, ResourceInfo>,
}

#[cfg(feature = "clap")]
fn parse_map(s: &str) -> Result<HashMap<String, String>, String> {
    if s.is_empty() {
        Ok(HashMap::default())
    } else {
        s.split(',')
            .map(|entry| {
                let (key, value) = entry.split_once('=').ok_or_else(|| {
                    format!("expected string of form `<key>=<value>[,<key>=<value>...]`; got `{s}`")
                })?;
                Ok((key.to_owned(), value.to_owned()))
            })
            .collect()
    }
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "clap", derive(clap::Args))]
pub struct Opts {
    /// Whether or not `rustfmt` is executed to format generated code.
    #[cfg_attr(feature = "clap", arg(long))]
    pub rustfmt: bool,

    /// If true, code generation should qualify any features that depend on
    /// `std` with `cfg(feature = "std")`.
    #[cfg_attr(feature = "clap", arg(long))]
    pub std_feature: bool,

    /// If true, code generation should pass borrowed string arguments as
    /// `&[u8]` instead of `&str`. Strings are still required to be valid
    /// UTF-8, but this avoids the need for Rust code to do its own UTF-8
    /// validation if it doesn't already have a `&str`.
    #[cfg_attr(feature = "clap", arg(long))]
    pub raw_strings: bool,

    /// Names of functions to skip generating bindings for.
    #[cfg_attr(feature = "clap", arg(long))]
    pub skip: Vec<String>,

    /// Name of the concrete type which implements the trait representing any
    /// top-level functions exported by the world.
    #[cfg_attr(feature = "clap", arg(long))]
    pub world_exports: Option<String>,

    /// Names of the concrete types which implement the traits representing any
    /// interfaces exported by the world.
    #[cfg_attr(feature = "clap", arg(long, value_parser = parse_map, default_value = ""))]
    pub interface_exports: HashMap<String, String>,

    /// Names of the concrete types which implement the traits representing any
    /// resources exported by the world.
    #[cfg_attr(feature = "clap", arg(long, value_parser = parse_map, default_value = ""))]
    pub resource_exports: HashMap<String, String>,

    /// If true, generate stub implementations for any exported functions,
    /// interfaces, and/or resources.
    #[cfg_attr(feature = "clap", arg(long))]
    pub stubs: bool,

    /// Optionally prefix any export names with the specified value.
    ///
    /// This is useful to avoid name conflicts when testing.
    #[cfg_attr(feature = "clap", arg(long))]
    pub export_prefix: Option<String>,

    /// Whether to generate owning or borrowing type definitions.
    ///
    /// Valid values include:
    /// - `owning`: Generated types will be composed entirely of owning fields,
    /// regardless of whether they are used as parameters to imports or not.
    /// - `borrowing`: Generated types used as parameters to imports will be
    /// "deeply borrowing", i.e. contain references rather than owned values
    /// when applicable.
    /// - `borrowing-duplicate-if-necessary`: As above, but generating distinct
    /// types for borrowing and owning, if necessary.
    #[cfg_attr(feature = "clap", arg(long, default_value_t = Ownership::Owning))]
    pub ownership: Ownership,
}

impl Opts {
    pub fn build(self) -> Box<dyn WorldGenerator> {
        let mut r = RustWasm::new();
        r.skip = self.skip.iter().cloned().collect();
        r.opts = self;
        Box::new(r)
    }
}

impl RustWasm {
    fn new() -> RustWasm {
        RustWasm::default()
    }

    fn interface<'a>(
        &'a mut self,
        wasm_import_module: Option<&'a str>,
        resolve: &'a Resolve,
        in_import: bool,
    ) -> InterfaceGenerator<'a> {
        let mut sizes = SizeAlign::default();
        sizes.fill(resolve);

        InterfaceGenerator {
            current_interface: None,
            wasm_import_module,
            src: Source::default(),
            in_import,
            gen: self,
            sizes,
            resolve,
            return_pointer_area_size: 0,
            return_pointer_area_align: 0,
        }
    }

    fn emit_modules(&mut self, modules: &BTreeMap<Option<PackageName>, Vec<String>>) {
        let mut map = BTreeMap::new();
        for (pkg, modules) in modules {
            match pkg {
                Some(pkg) => {
                    let prev = map
                        .entry(&pkg.namespace)
                        .or_insert(BTreeMap::new())
                        .insert(&pkg.name, modules);
                    assert!(prev.is_none());
                }
                None => {
                    for module in modules {
                        uwriteln!(self.src, "{module}");
                    }
                }
            }
        }
        for (ns, pkgs) in map {
            uwriteln!(self.src, "pub mod {} {{", ns.to_snake_case());
            for (pkg, modules) in pkgs {
                uwriteln!(self.src, "pub mod {} {{", pkg.to_snake_case());
                for module in modules {
                    uwriteln!(self.src, "{module}");
                }
                uwriteln!(self.src, "}}");
            }
            uwriteln!(self.src, "}}");
        }
    }
}

impl WorldGenerator for RustWasm {
    fn preprocess(&mut self, resolve: &Resolve, _world: WorldId) {
        let version = env!("CARGO_PKG_VERSION");
        uwriteln!(
            self.src,
            "// Generated by `wit-bindgen` {version}. DO NOT EDIT!"
        );
        self.types.analyze(resolve);
    }

    fn import_interface(
        &mut self,
        resolve: &Resolve,
        name: &WorldKey,
        id: InterfaceId,
        _files: &mut Files,
    ) {
        let wasm_import_module = resolve.name_world_key(name);
        let mut gen = self.interface(Some(&wasm_import_module), resolve, true);
        gen.current_interface = Some((id, name));
        gen.types(id);

        let by_resource = group_by_resource(resolve.interfaces[id].functions.values());
        for (resource, funcs) in by_resource {
            if let Some(resource) = resource {
                let name = resolve.types[resource].name.as_deref().unwrap();

                let camel = name.to_upper_camel_case();

                uwriteln!(
                    gen.src,
                    r#"
                        pub struct {camel} {{
                            handle: i32,
                            owned: bool,
                        }}

                        impl Drop for {camel} {{
                             fn drop(&mut self) {{
                                 unsafe {{
                                     if self.owned {{
                                         #[link(wasm_import_module = "imports")]
                                         extern "C" {{
                                             #[link_name = "[resource-drop-own]{name}"]
                                             fn wit_import(_: i32);
                                         }}

                                         wit_import(self.handle)
                                     }} else {{
                                         #[link(wasm_import_module = "imports")]
                                         extern "C" {{
                                             #[link_name = "[resource-drop-borrow]{name}"]
                                             fn wit_import(_: i32);
                                         }}

                                         wit_import(self.handle)
                                     }}
                                 }}
                             }}
                        }}

                        impl {camel} {{
                            #[doc(hidden)]
                            pub unsafe fn from_handle(handle: i32, owned: bool) -> Self {{
                                Self {{ handle, owned }}
                            }}

                            #[doc(hidden)]
                            pub fn into_handle(self) -> i32 {{
                                core::mem::ManuallyDrop::new(self).handle
                            }}
                    "#
                );
            }
            for func in funcs {
                gen.generate_guest_import(func);
            }
            if resource.is_some() {
                gen.src.push_str("}\n");
            }
        }

        gen.finish_append_submodule(name);
    }

    fn import_funcs(
        &mut self,
        resolve: &Resolve,
        _world: WorldId,
        funcs: &[(&str, &Function)],
        _files: &mut Files,
    ) {
        let mut gen = self.interface(Some("$root"), resolve, true);

        for (_, func) in funcs {
            gen.generate_guest_import(func);
        }

        let src = gen.finish();
        self.src.push_str(&src);
    }

    fn export_interface(
        &mut self,
        resolve: &Resolve,
        name: &WorldKey,
        id: InterfaceId,
        _files: &mut Files,
    ) {
        let (pkg, inner_name) = match name {
            WorldKey::Name(name) => (None, name),
            WorldKey::Interface(id) => {
                let interface = &resolve.interfaces[*id];
                (
                    Some(&resolve.packages[interface.package.unwrap()].name),
                    interface.name.as_ref().unwrap(),
                )
            }
        };
        let path = format!(
            "{}{inner_name}",
            if let Some(pkg) = pkg {
                format!("{}::{}::", pkg.namespace, pkg.name)
            } else {
                String::new()
            }
        );
        let impl_name = self
            .opts
            .interface_exports
            .get(&path)
            .cloned()
            .or_else(|| self.opts.stubs.then(|| "Stub".to_owned()))
            .ok_or_else(|| format!("interface export implementation required for `{path}`"));
        let mut gen = self.interface(None, resolve, false);
        gen.current_interface = Some((id, name));
        gen.types(id);
        gen.generate_exports(
            &inner_name.to_upper_camel_case(),
            Some(&path),
            impl_name.as_deref(),
            Some(name),
            resolve.interfaces[id].functions.values(),
        );
        gen.finish_append_submodule(name);
    }

    fn export_funcs(
        &mut self,
        resolve: &Resolve,
        world: WorldId,
        funcs: &[(&str, &Function)],
        _files: &mut Files,
    ) {
        let world_name = &resolve.worlds[world].name;
        let impl_name = self
            .opts
            .world_exports
            .clone()
            .or_else(|| self.opts.stubs.then(|| "Stub".to_owned()))
            .ok_or_else(|| format!("world export implementation required"));
        let trait_name = world_name.to_upper_camel_case();
        let mut gen = self.interface(None, resolve, false);
        gen.generate_exports(
            &trait_name,
            None,
            impl_name.as_deref(),
            None,
            funcs.iter().map(|f| f.1),
        );
        let src = gen.finish();
        self.src.push_str(&src);
    }

    fn export_types(
        &mut self,
        resolve: &Resolve,
        _world: WorldId,
        types: &[(&str, TypeId)],
        _files: &mut Files,
    ) {
        let mut gen = self.interface(None, resolve, false);
        for (name, ty) in types {
            gen.define_type(name, *ty);
        }
        let src = gen.finish();
        self.src.push_str(&src);
    }

    fn finish(&mut self, resolve: &Resolve, world: WorldId, files: &mut Files) {
        let name = &resolve.worlds[world].name;
        let imports = mem::take(&mut self.import_modules);
        self.emit_modules(&imports);
        let exports = mem::take(&mut self.export_modules);
        if !exports.is_empty() {
            self.src.push_str("pub mod exports {\n");
            self.emit_modules(&exports);
            self.src.push_str("}\n");
        }

        self.src.push_str("\n#[cfg(target_arch = \"wasm32\")]\n");

        // The custom section name here must start with "component-type" but
        // otherwise is attempted to be unique here to ensure that this doesn't get
        // concatenated to other custom sections by LLD by accident since LLD will
        // concatenate custom sections of the same name.
        self.src
            .push_str(&format!("#[link_section = \"component-type:{}\"]\n", name,));

        let mut producers = wasm_metadata::Producers::empty();
        producers.add(
            "processed-by",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        );

        let component_type = wit_component::metadata::encode(
            resolve,
            world,
            wit_component::StringEncoding::UTF8,
            Some(&producers),
        )
        .unwrap();

        self.src.push_str("#[doc(hidden)]\n");
        self.src.push_str(&format!(
            "pub static __WIT_BINDGEN_COMPONENT_TYPE: [u8; {}] = ",
            component_type.len()
        ));
        self.src.push_str(&format!("{:?};\n", component_type));

        self.src.push_str(
            "
            #[inline(never)]
            #[doc(hidden)]
            #[cfg(target_arch = \"wasm32\")]
            pub fn __link_section() {}
        ",
        );

        if self.opts.stubs {
            self.src.push_str("\npub struct Stub;\n");
            let world = &resolve.worlds[world];
            let mut funcs = Vec::new();
            for (name, export) in world.exports.iter() {
                let (pkg, name) = match name {
                    WorldKey::Name(name) => (None, name),
                    WorldKey::Interface(id) => {
                        let interface = &resolve.interfaces[*id];
                        (
                            Some(&resolve.packages[interface.package.unwrap()].name),
                            interface.name.as_ref().unwrap(),
                        )
                    }
                };
                match export {
                    WorldItem::Function(func) => {
                        funcs.push(func);
                    }
                    WorldItem::Interface(id) => {
                        for (resource, funcs) in
                            group_by_resource(resolve.interfaces[*id].functions.values())
                        {
                            let mut gen = self.interface(None, resolve, false);
                            gen.generate_stub(resource, pkg, name, true, &funcs);
                            let stub = gen.finish();
                            self.src.push_str(&stub);
                        }
                    }
                    WorldItem::Type(_) => unreachable!(),
                }
            }

            for (resource, funcs) in group_by_resource(funcs.into_iter()) {
                let mut gen = self.interface(None, resolve, false);
                gen.generate_stub(resource, None, &world.name, false, &funcs);
                let stub = gen.finish();
                self.src.push_str(&stub);
            }
        }

        let mut src = mem::take(&mut self.src);
        if self.opts.rustfmt {
            let mut child = Command::new("rustfmt")
                .arg("--edition=2018")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("failed to spawn `rustfmt`");
            child
                .stdin
                .take()
                .unwrap()
                .write_all(src.as_bytes())
                .unwrap();
            src.as_mut_string().truncate(0);
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(src.as_mut_string())
                .unwrap();
            let status = child.wait().unwrap();
            assert!(status.success());
        }

        let module_name = name.to_snake_case();
        files.push(&format!("{module_name}.rs"), src.as_bytes());
    }
}

struct InterfaceGenerator<'a> {
    src: Source,
    current_interface: Option<(InterfaceId, &'a WorldKey)>,
    in_import: bool,
    sizes: SizeAlign,
    gen: &'a mut RustWasm,
    wasm_import_module: Option<&'a str>,
    resolve: &'a Resolve,
    return_pointer_area_size: usize,
    return_pointer_area_align: usize,
}

impl InterfaceGenerator<'_> {
    fn generate_exports<'a>(
        &mut self,
        trait_name: &str,
        path: Option<&str>,
        impl_name: Result<&str, &String>,
        interface_name: Option<&WorldKey>,
        funcs: impl Iterator<Item = &'a Function>,
    ) {
        let by_resource = group_by_resource(funcs);

        for (resource, funcs) in by_resource {
            let trait_name = if let Some(ty) = resource {
                self.resolve.types[ty]
                    .name
                    .as_deref()
                    .unwrap()
                    .to_upper_camel_case()
            } else {
                trait_name.to_owned()
            };
            let mut saw_export = false;
            uwriteln!(self.src, "pub trait {trait_name} {{");
            for &func in &funcs {
                if self.gen.skip.contains(&func.name) {
                    continue;
                }
                saw_export = true;
                let mut sig = FnSig::default();
                sig.use_item_name = true;
                sig.private = true;
                if let FunctionKind::Method(_) = &func.kind {
                    sig.self_arg = Some("&self".into());
                    sig.self_is_first_param = true;
                }
                self.print_signature(func, TypeMode::Owned, &sig);
                self.src.push_str(";\n");
            }
            uwriteln!(self.src, "}}");

            if saw_export {
                let mut path_to_root = String::new();
                if let Some(key) = interface_name {
                    if !self.in_import {
                        path_to_root.push_str("super::");
                    }
                    if let WorldKey::Interface(_) = key {
                        path_to_root.push_str("super::super::");
                    }
                    path_to_root.push_str("super::");
                }
                if let Some(ty) = resource {
                    let name = self.resolve.types[ty].name.as_deref().unwrap();
                    let path = if let Some(path) = path {
                        format!("{path}::{name}")
                    } else {
                        name.to_owned()
                    };
                    let impl_name = self
                        .gen
                        .opts
                        .resource_exports
                        .get(&path)
                        .cloned()
                        .or_else(|| self.gen.opts.stubs.then(|| "Stub".to_owned()))
                        .ok_or_else(|| {
                            format!("resource export implementation required for `{path}`")
                        })
                        .unwrap();

                    uwriteln!(
                        self.src,
                        "use {path_to_root}{impl_name} as Rep{trait_name};"
                    );
                } else {
                    let impl_name = impl_name.unwrap();
                    uwriteln!(
                        self.src,
                        "use {path_to_root}{impl_name} as {trait_name}Impl;"
                    );
                }
                self.src.push_str("const _: () = {\n");
                for &func in &funcs {
                    self.generate_guest_export(func, interface_name, &trait_name);
                }
                self.src.push_str("};\n");
            }
        }
    }

    fn finish(&mut self) -> String {
        if self.return_pointer_area_align > 0 {
            uwrite!(
                self.src,
                "
                    #[allow(unused_imports)]
                    use wit_bindgen::rt::{{alloc, vec::Vec, string::String}};

                    #[repr(align({align}))]
                    struct _RetArea([u8; {size}]);
                    static mut _RET_AREA: _RetArea = _RetArea([0; {size}]);
                ",
                align = self.return_pointer_area_align,
                size = self.return_pointer_area_size,
            );
        }

        mem::take(&mut self.src).into()
    }

    fn finish_resources(&self) -> String {
        let mut src = String::new();
        for (id, info) in &self.gen.resources {
            if let Direction::Export = info.direction {
                let name = self.resolve.types[*id].name.as_deref().unwrap();
                let camel = name.to_upper_camel_case();
                let snake = to_rust_ident(name);
                let export_prefix = self.gen.opts.export_prefix.as_deref().unwrap_or("");

                uwriteln!(
                    src,
                    r#"
                        const _: () = {{
                            #[doc(hidden)]
                            #[export_name = "{export_prefix}exports#[dtor]{name}"]
                            #[allow(non_snake_case)]
                            unsafe extern "C" fn __export_dtor_{snake}(arg0: i32) {{
                                use wit_bindgen::rt::boxed::Box;
                                drop(Box::from_raw(core::mem::transmute::<isize, *mut Rep{camel}>(
                                    arg0.try_into().unwrap(),
                                )))
                            }}
                        }};
                    "#
                );

                if let Some(_) = &info.own {
                    uwriteln!(
                        src,
                        r#"
                            pub struct Own{camel} {{
                                handle: i32,
                            }}

                            impl Own{camel} {{
                                #[doc(hidden)]
                                pub unsafe fn from_handle(handle: i32) -> Self {{
                                    Self {{ handle }}
                                }}

                                #[doc(hidden)]
                                pub fn into_handle(self) -> i32 {{
                                    core::mem::ManuallyDrop::new(self).handle
                                }}

                                pub fn new(rep: Rep{camel}) -> Own{camel} {{
                                    use wit_bindgen::rt::boxed::Box;
                                    unsafe {{
                                        #[link(wasm_import_module = "[export]exports")]
                                        extern "C" {{
                                            #[link_name = "[resource-new]{name}"]
                                            fn wit_import(_: i32) -> i32;
                                        }}

                                        Own{camel} {{
                                            handle: wit_import(
                                                core::mem::transmute::<*mut Rep{camel}, isize>(
                                                    Box::into_raw(Box::new(rep))
                                                )
                                                    .try_into()
                                                    .unwrap(),
                                            ),
                                        }}
                                    }}
                                }}
                            }}

                            impl core::ops::Deref for Own{camel} {{
                                type Target = Rep{camel};

                                fn deref(&self) -> &Rep{camel} {{
                                    unsafe {{
                                        #[link(wasm_import_module = "[export]exports")]
                                        extern "C" {{
                                            #[link_name = "[resource-rep]{name}"]
                                            fn wit_import(_: i32) -> i32;
                                        }}

                                        core::mem::transmute::<isize, &Rep{camel}>(
                                            wit_import(self.handle).try_into().unwrap()
                                        )
                                    }}
                                }}
                            }}

                            impl Drop for Own{camel} {{
                                fn drop(&mut self) {{
                                    unsafe {{
                                        #[link(wasm_import_module = "my:resources/types")]
                                        extern "C" {{
                                            #[link_name = "[resource-drop-own]{name}"]
                                            fn wit_import(_: i32);
                                        }}

                                        wit_import(self.handle)
                                    }}
                                }}
                            }}
                        "#
                    );
                }
            }
        }

        src
    }

    fn finish_append_submodule(mut self, name: &WorldKey) {
        let module = self.finish();
        let snake = match name {
            WorldKey::Name(name) => to_rust_ident(name),
            WorldKey::Interface(id) => {
                to_rust_ident(self.resolve.interfaces[*id].name.as_ref().unwrap())
            }
        };
        let mut path_to_root = String::from("super::");
        let pkg = match name {
            WorldKey::Name(_) => None,
            WorldKey::Interface(id) => {
                let pkg = self.resolve.interfaces[*id].package.unwrap();
                Some(self.resolve.packages[pkg].name.clone())
            }
        };
        if let Some((id, _)) = self.current_interface {
            let mut path = String::new();
            if !self.in_import {
                path.push_str("exports::");
                path_to_root.push_str("super::");
            }
            if let Some(name) = &pkg {
                path.push_str(&format!(
                    "{}::{}::",
                    name.namespace.to_snake_case(),
                    name.name.to_snake_case()
                ));
                path_to_root.push_str("super::super::");
            }
            path.push_str(&snake);
            self.gen.interface_names.insert(id, path);
        }
        let resources = self.finish_resources();
        let module = format!(
            "
                #[allow(clippy::all)]
                pub mod {snake} {{
                    #[used]
                    #[doc(hidden)]
                    #[cfg(target_arch = \"wasm32\")]
                    static __FORCE_SECTION_REF: fn() = {path_to_root}__link_section;

                    {module}
                    {resources}
                }}
            ",
        );
        let map = if self.in_import {
            &mut self.gen.import_modules
        } else {
            &mut self.gen.export_modules
        };
        map.entry(pkg).or_insert(Vec::new()).push(module);
    }

    fn generate_guest_import(&mut self, func: &Function) {
        if self.gen.skip.contains(&func.name) {
            return;
        }

        let mut sig = FnSig::default();
        let param_mode = TypeMode::AllBorrowed("'_");
        match &func.kind {
            FunctionKind::Freestanding => {}
            FunctionKind::Method(_) | FunctionKind::Static(_) | FunctionKind::Constructor(_) => {
                sig.use_item_name = true;
                if let FunctionKind::Method(_) = &func.kind {
                    sig.self_arg = Some("&self".into());
                    sig.self_is_first_param = true;
                }
            }
        }
        self.src.push_str("#[allow(clippy::all)]\n");
        let params = self.print_signature(func, param_mode, &sig);
        self.src.push_str("{\n");
        self.src.push_str(
            "
                #[allow(unused_imports)]
                use wit_bindgen::rt::{alloc, vec::Vec, string::String};
            ",
        );
        self.src.push_str("unsafe {\n");

        let mut f = FunctionBindgen::new(self, params, None);
        f.gen.resolve.call(
            AbiVariant::GuestImport,
            LiftLower::LowerArgsLiftResults,
            func,
            &mut f,
        );
        let FunctionBindgen {
            needs_cleanup_list,
            src,
            import_return_pointer_area_size,
            import_return_pointer_area_align,
            ..
        } = f;

        if needs_cleanup_list {
            self.src.push_str("let mut cleanup_list = Vec::new();\n");
        }
        if import_return_pointer_area_size > 0 {
            uwrite!(
                self.src,
                "
                    #[repr(align({import_return_pointer_area_align}))]
                    struct RetArea([u8; {import_return_pointer_area_size}]);
                    let mut ret_area = ::core::mem::MaybeUninit::<RetArea>::uninit();
                ",
            );
        }
        self.src.push_str(&String::from(src));

        self.src.push_str("}\n");
        self.src.push_str("}\n");
    }

    fn generate_guest_export(
        &mut self,
        func: &Function,
        interface_name: Option<&WorldKey>,
        trait_name: &str,
    ) {
        if self.gen.skip.contains(&func.name) {
            return;
        }

        let name_snake = func.name.to_snake_case().replace('.', "_");
        let wasm_module_export_name = interface_name.map(|k| self.resolve.name_world_key(k));
        let export_prefix = self.gen.opts.export_prefix.as_deref().unwrap_or("");
        let export_name = func.core_export_name(wasm_module_export_name.as_deref());
        uwrite!(
            self.src,
            "
                #[doc(hidden)]
                #[export_name = \"{export_prefix}{export_name}\"]
                #[allow(non_snake_case)]
                unsafe extern \"C\" fn __export_{name_snake}(\
            ",
        );

        let sig = self.resolve.wasm_signature(AbiVariant::GuestExport, func);
        let mut params = Vec::new();
        for (i, param) in sig.params.iter().enumerate() {
            let name = format!("arg{}", i);
            uwrite!(self.src, "{name}: {},", wasm_type(*param));
            params.push(name);
        }
        self.src.push_str(")");

        match sig.results.len() {
            0 => {}
            1 => {
                uwrite!(self.src, " -> {}", wasm_type(sig.results[0]));
            }
            _ => unimplemented!(),
        }

        self.push_str(" {");

        uwrite!(
            self.src,
            "
                #[allow(unused_imports)]
                use wit_bindgen::rt::{{alloc, vec::Vec, string::String}};

                // Before executing any other code, use this function to run all static
                // constructors, if they have not yet been run. This is a hack required
                // to work around wasi-libc ctors calling import functions to initialize
                // the environment.
                //
                // This functionality will be removed once rust 1.69.0 is stable, at which
                // point wasi-libc will no longer have this behavior.
                //
                // See
                // https://github.com/bytecodealliance/preview2-prototyping/issues/99
                // for more details.
                #[cfg(target_arch=\"wasm32\")]
                wit_bindgen::rt::run_ctors_once();

            "
        );

        let mut f = FunctionBindgen::new(self, params, Some(trait_name));
        f.gen.resolve.call(
            AbiVariant::GuestExport,
            LiftLower::LiftArgsLowerResults,
            func,
            &mut f,
        );
        let FunctionBindgen {
            needs_cleanup_list,
            src,
            ..
        } = f;
        assert!(!needs_cleanup_list);
        self.src.push_str(&String::from(src));
        self.src.push_str("}\n");

        if self.resolve.guest_export_needs_post_return(func) {
            let export_prefix = self.gen.opts.export_prefix.as_deref().unwrap_or("");
            uwrite!(
                self.src,
                "
                    const _: () = {{
                    #[doc(hidden)]
                    #[export_name = \"{export_prefix}cabi_post_{export_name}\"]
                    #[allow(non_snake_case)]
                    unsafe extern \"C\" fn __post_return_{name_snake}(\
                "
            );
            let mut params = Vec::new();
            for (i, result) in sig.results.iter().enumerate() {
                let name = format!("arg{}", i);
                uwrite!(self.src, "{name}: {},", wasm_type(*result));
                params.push(name);
            }
            self.src.push_str(") {\n");

            let mut f = FunctionBindgen::new(self, params, Some(trait_name));
            f.gen.resolve.post_return(func, &mut f);
            let FunctionBindgen {
                needs_cleanup_list,
                src,
                ..
            } = f;
            assert!(!needs_cleanup_list);
            self.src.push_str(&String::from(src));
            self.src.push_str("}\n");
            self.src.push_str("};\n");
        }
    }

    fn generate_stub(
        &mut self,
        resource: Option<TypeId>,
        pkg: Option<&PackageName>,
        name: &str,
        in_interface: bool,
        funcs: &[&Function],
    ) {
        let path = if let Some(pkg) = pkg {
            format!(
                "{}::{}::{}",
                to_rust_ident(&pkg.namespace),
                to_rust_ident(&pkg.name),
                to_rust_ident(name),
            )
        } else {
            to_rust_ident(name)
        };

        let name = resource
            .map(|ty| {
                self.resolve.types[ty]
                    .name
                    .as_deref()
                    .unwrap()
                    .to_upper_camel_case()
            })
            .unwrap_or_else(|| name.to_upper_camel_case());

        let qualified_name = if in_interface {
            format!("exports::{path}::{name}")
        } else {
            name
        };

        uwriteln!(self.src, "impl {qualified_name} for Stub {{");

        for &func in funcs {
            if self.gen.skip.contains(&func.name) {
                continue;
            }
            let mut sig = FnSig::default();
            sig.use_item_name = true;
            sig.private = true;
            if let FunctionKind::Method(_) = &func.kind {
                sig.self_arg = Some("&self".into());
                sig.self_is_first_param = true;
            }
            self.print_signature(func, TypeMode::Owned, &sig);
            self.src.push_str("{ unreachable!() }\n");
        }

        self.src.push_str("}\n");
    }
}

impl<'a> RustGenerator<'a> for InterfaceGenerator<'a> {
    fn resolve(&self) -> &'a Resolve {
        self.resolve
    }

    fn ownership(&self) -> Ownership {
        self.gen.opts.ownership
    }

    fn path_to_interface(&self, interface: InterfaceId) -> Option<String> {
        let mut path = String::new();
        if let Some((cur, name)) = self.current_interface {
            if cur == interface {
                return None;
            }
            if !self.in_import {
                path.push_str("super::");
            }
            match name {
                WorldKey::Name(_) => {
                    path.push_str("super::");
                }
                WorldKey::Interface(_) => {
                    path.push_str("super::super::super::");
                }
            }
        }
        let name = &self.gen.interface_names[&interface];
        path.push_str(&name);
        Some(path)
    }

    fn std_feature(&self) -> bool {
        self.gen.opts.std_feature
    }

    fn use_raw_strings(&self) -> bool {
        self.gen.opts.raw_strings
    }

    fn is_exported_resource(&self, ty: TypeId) -> bool {
        matches!(
            self.gen
                .resources
                .get(&dealias(self.resolve, ty))
                .map(|info| info.direction),
            Some(Direction::Export)
        )
    }

    fn add_own(&mut self, resource: TypeId, handle: TypeId) {
        self.gen
            .resources
            .entry(dealias(self.resolve, resource))
            .or_default()
            .own = Some(handle);
    }

    fn vec_name(&self) -> &'static str {
        "wit_bindgen::rt::vec::Vec"
    }

    fn string_name(&self) -> &'static str {
        "wit_bindgen::rt::string::String"
    }

    fn push_str(&mut self, s: &str) {
        self.src.push_str(s);
    }

    fn info(&self, ty: TypeId) -> TypeInfo {
        self.gen.types.get(ty)
    }

    fn types_mut(&mut self) -> &mut Types {
        &mut self.gen.types
    }

    fn print_borrowed_slice(
        &mut self,
        mutbl: bool,
        ty: &Type,
        lifetime: &'static str,
        mode: TypeMode,
    ) {
        self.print_rust_slice(mutbl, ty, lifetime, mode);
    }

    fn print_borrowed_str(&mut self, lifetime: &'static str) {
        self.push_str("&");
        if lifetime != "'_" {
            self.push_str(lifetime);
            self.push_str(" ");
        }
        if self.gen.opts.raw_strings {
            self.push_str("[u8]");
        } else {
            self.push_str("str");
        }
    }
}

impl<'a> wit_bindgen_core::InterfaceGenerator<'a> for InterfaceGenerator<'a> {
    fn resolve(&self) -> &'a Resolve {
        self.resolve
    }

    fn type_record(&mut self, id: TypeId, _name: &str, record: &Record, docs: &Docs) {
        self.print_typedef_record(id, record, docs, false);
    }

    fn type_resource(&mut self, id: TypeId, _name: &str, docs: &Docs) {
        let entry = self.gen.resources.entry(id).or_default();
        if !self.in_import {
            entry.direction = Direction::Export;
        }
        entry.docs = docs.clone();
    }

    fn type_tuple(&mut self, id: TypeId, _name: &str, tuple: &Tuple, docs: &Docs) {
        self.print_typedef_tuple(id, tuple, docs);
    }

    fn type_flags(&mut self, _id: TypeId, name: &str, flags: &Flags, docs: &Docs) {
        self.src.push_str("wit_bindgen::bitflags::bitflags! {\n");
        self.rustdoc(docs);
        let repr = RustFlagsRepr::new(flags);
        self.src.push_str(&format!(
            "#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Clone, Copy)]\npub struct {}: {repr} {{\n",
            name.to_upper_camel_case(),
        ));
        for (i, flag) in flags.flags.iter().enumerate() {
            self.rustdoc(&flag.docs);
            self.src.push_str(&format!(
                "const {} = 1 << {};\n",
                flag.name.to_shouty_snake_case(),
                i,
            ));
        }
        self.src.push_str("}\n");
        self.src.push_str("}\n");
    }

    fn type_variant(&mut self, id: TypeId, _name: &str, variant: &Variant, docs: &Docs) {
        self.print_typedef_variant(id, variant, docs, false);
    }

    fn type_union(&mut self, id: TypeId, _name: &str, union: &Union, docs: &Docs) {
        self.print_typedef_union(id, union, docs, false);
    }

    fn type_option(&mut self, id: TypeId, _name: &str, payload: &Type, docs: &Docs) {
        self.print_typedef_option(id, payload, docs);
    }

    fn type_result(&mut self, id: TypeId, _name: &str, result: &Result_, docs: &Docs) {
        self.print_typedef_result(id, result, docs);
    }

    fn type_enum(&mut self, id: TypeId, name: &str, enum_: &Enum, docs: &Docs) {
        self.print_typedef_enum(id, name, enum_, docs, &[], Box::new(|_| String::new()));
    }

    fn type_alias(&mut self, id: TypeId, _name: &str, ty: &Type, docs: &Docs) {
        self.print_typedef_alias(id, ty, docs);
    }

    fn type_list(&mut self, id: TypeId, _name: &str, ty: &Type, docs: &Docs) {
        self.print_type_list(id, ty, docs);
    }

    fn type_builtin(&mut self, _id: TypeId, name: &str, ty: &Type, docs: &Docs) {
        self.rustdoc(docs);
        self.src
            .push_str(&format!("pub type {}", name.to_upper_camel_case()));
        self.src.push_str(" = ");
        self.print_ty(ty, TypeMode::Owned);
        self.src.push_str(";\n");
    }
}

struct FunctionBindgen<'a, 'b> {
    gen: &'b mut InterfaceGenerator<'a>,
    params: Vec<String>,
    trait_name: Option<&'b str>,
    src: Source,
    blocks: Vec<String>,
    block_storage: Vec<(Source, Vec<(String, String)>)>,
    tmp: usize,
    needs_cleanup_list: bool,
    cleanup: Vec<(String, String)>,
    import_return_pointer_area_size: usize,
    import_return_pointer_area_align: usize,
}

impl<'a, 'b> FunctionBindgen<'a, 'b> {
    fn new(
        gen: &'b mut InterfaceGenerator<'a>,
        params: Vec<String>,
        trait_name: Option<&'b str>,
    ) -> FunctionBindgen<'a, 'b> {
        FunctionBindgen {
            gen,
            params,
            trait_name,
            src: Default::default(),
            blocks: Vec::new(),
            block_storage: Vec::new(),
            tmp: 0,
            needs_cleanup_list: false,
            cleanup: Vec::new(),
            import_return_pointer_area_size: 0,
            import_return_pointer_area_align: 0,
        }
    }

    fn emit_cleanup(&mut self) {
        for (ptr, layout) in mem::take(&mut self.cleanup) {
            self.push_str(&format!(
                "if {layout}.size() != 0 {{\nalloc::dealloc({ptr}, {layout});\n}}\n"
            ));
        }
        if self.needs_cleanup_list {
            self.push_str(
                "for (ptr, layout) in cleanup_list {\n
                    if layout.size() != 0 {\n
                        alloc::dealloc(ptr, layout);\n
                    }\n
                }\n",
            );
        }
    }

    fn declare_import(
        &mut self,
        module_name: &str,
        name: &str,
        params: &[WasmType],
        results: &[WasmType],
    ) -> String {
        // Define the actual function we're calling inline
        uwriteln!(
            self.src,
            "
                #[link(wasm_import_module = \"{module_name}\")]
                extern \"C\" {{
                    #[cfg_attr(target_arch = \"wasm32\", link_name = \"{name}\")]
                    #[cfg_attr(not(target_arch = \"wasm32\"), link_name = \"{module_name}_{name}\")]
                    fn wit_import(\
            "
        );
        for param in params.iter() {
            self.push_str("_: ");
            self.push_str(wasm_type(*param));
            self.push_str(", ");
        }
        self.push_str(")");
        assert!(results.len() < 2);
        for result in results.iter() {
            self.push_str(" -> ");
            self.push_str(wasm_type(*result));
        }
        self.push_str(";\n}\n");
        "wit_import".to_string()
    }
}

impl RustFunctionGenerator for FunctionBindgen<'_, '_> {
    fn push_str(&mut self, s: &str) {
        self.src.push_str(s);
    }

    fn tmp(&mut self) -> usize {
        let ret = self.tmp;
        self.tmp += 1;
        ret
    }

    fn rust_gen(&self) -> &dyn RustGenerator {
        self.gen
    }

    fn lift_lower(&self) -> LiftLower {
        if self.gen.in_import {
            LiftLower::LowerArgsLiftResults
        } else {
            LiftLower::LiftArgsLowerResults
        }
    }
}

impl Bindgen for FunctionBindgen<'_, '_> {
    type Operand = String;

    fn push_block(&mut self) {
        let prev_src = mem::take(&mut self.src);
        let prev_cleanup = mem::take(&mut self.cleanup);
        self.block_storage.push((prev_src, prev_cleanup));
    }

    fn finish_block(&mut self, operands: &mut Vec<String>) {
        if self.cleanup.len() > 0 {
            self.needs_cleanup_list = true;
            self.push_str("cleanup_list.extend_from_slice(&[");
            for (ptr, layout) in mem::take(&mut self.cleanup) {
                self.push_str("(");
                self.push_str(&ptr);
                self.push_str(", ");
                self.push_str(&layout);
                self.push_str("),");
            }
            self.push_str("]);\n");
        }
        let (prev_src, prev_cleanup) = self.block_storage.pop().unwrap();
        let src = mem::replace(&mut self.src, prev_src);
        self.cleanup = prev_cleanup;
        let expr = match operands.len() {
            0 => "()".to_string(),
            1 => operands[0].clone(),
            _ => format!("({})", operands.join(", ")),
        };
        if src.is_empty() {
            self.blocks.push(expr);
        } else if operands.is_empty() {
            self.blocks.push(format!("{{\n{}\n}}", &src[..]));
        } else {
            self.blocks.push(format!("{{\n{}\n{}\n}}", &src[..], expr));
        }
    }

    fn return_pointer(&mut self, size: usize, align: usize) -> String {
        let tmp = self.tmp();

        // Imports get a per-function return area to facilitate using the
        // stack whereas exports use a per-module return area to cut down on
        // stack usage. Note that for imports this also facilitates "adapter
        // modules" for components to not have data segments.
        if self.gen.in_import {
            self.import_return_pointer_area_size = self.import_return_pointer_area_size.max(size);
            self.import_return_pointer_area_align =
                self.import_return_pointer_area_align.max(align);
            uwrite!(self.src, "let ptr{tmp} = ret_area.as_mut_ptr() as i32;");
        } else {
            self.gen.return_pointer_area_size = self.gen.return_pointer_area_size.max(size);
            self.gen.return_pointer_area_align = self.gen.return_pointer_area_align.max(align);
            uwriteln!(self.src, "let ptr{tmp} = _RET_AREA.0.as_mut_ptr() as i32;");
        }
        format!("ptr{}", tmp)
    }

    fn sizes(&self) -> &SizeAlign {
        &self.gen.sizes
    }

    fn is_list_canonical(&self, resolve: &Resolve, ty: &Type) -> bool {
        resolve.all_bits_valid(ty)
    }

    fn emit(
        &mut self,
        resolve: &Resolve,
        inst: &Instruction<'_>,
        operands: &mut Vec<String>,
        results: &mut Vec<String>,
    ) {
        let mut top_as = |cvt: &str| {
            let mut s = operands.pop().unwrap();
            s.push_str(" as ");
            s.push_str(cvt);
            results.push(s);
        };

        match inst {
            Instruction::GetArg { nth } => results.push(self.params[*nth].clone()),
            Instruction::I32Const { val } => results.push(format!("{}i32", val)),
            Instruction::ConstZero { tys } => {
                for ty in tys.iter() {
                    match ty {
                        WasmType::I32 => results.push("0i32".to_string()),
                        WasmType::I64 => results.push("0i64".to_string()),
                        WasmType::F32 => results.push("0.0f32".to_string()),
                        WasmType::F64 => results.push("0.0f64".to_string()),
                    }
                }
            }

            Instruction::I64FromU64 | Instruction::I64FromS64 => {
                let s = operands.pop().unwrap();
                results.push(format!("wit_bindgen::rt::as_i64({})", s));
            }
            Instruction::I32FromChar
            | Instruction::I32FromU8
            | Instruction::I32FromS8
            | Instruction::I32FromU16
            | Instruction::I32FromS16
            | Instruction::I32FromU32
            | Instruction::I32FromS32 => {
                let s = operands.pop().unwrap();
                results.push(format!("wit_bindgen::rt::as_i32({})", s));
            }

            Instruction::F32FromFloat32 => {
                let s = operands.pop().unwrap();
                results.push(format!("wit_bindgen::rt::as_f32({})", s));
            }
            Instruction::F64FromFloat64 => {
                let s = operands.pop().unwrap();
                results.push(format!("wit_bindgen::rt::as_f64({})", s));
            }
            Instruction::Float32FromF32
            | Instruction::Float64FromF64
            | Instruction::S32FromI32
            | Instruction::S64FromI64 => {
                results.push(operands.pop().unwrap());
            }
            Instruction::S8FromI32 => top_as("i8"),
            Instruction::U8FromI32 => top_as("u8"),
            Instruction::S16FromI32 => top_as("i16"),
            Instruction::U16FromI32 => top_as("u16"),
            Instruction::U32FromI32 => top_as("u32"),
            Instruction::U64FromI64 => top_as("u64"),
            Instruction::CharFromI32 => {
                results.push(format!(
                    "{{
                        #[cfg(not(debug_assertions))]
                        {{ ::core::char::from_u32_unchecked({} as u32) }}
                        #[cfg(debug_assertions)]
                        {{ ::core::char::from_u32({} as u32).unwrap() }}
                    }}",
                    operands[0], operands[0]
                ));
            }

            Instruction::Bitcasts { casts } => {
                wit_bindgen_rust_lib::bitcast(casts, operands, results)
            }

            Instruction::I32FromBool => {
                results.push(format!("match {} {{ true => 1, false => 0 }}", operands[0]));
            }
            Instruction::BoolFromI32 => {
                results.push(format!(
                    "{{
                        #[cfg(not(debug_assertions))]
                        {{ ::core::mem::transmute::<u8, bool>({} as u8) }}
                        #[cfg(debug_assertions)]
                        {{
                            match {} {{
                                0 => false,
                                1 => true,
                                _ => panic!(\"invalid bool discriminant\"),
                            }}
                        }}
                    }}",
                    operands[0], operands[0],
                ));
            }

            Instruction::FlagsLower { flags, .. } => {
                let tmp = self.tmp();
                self.push_str(&format!("let flags{} = {};\n", tmp, operands[0]));
                for i in 0..flags.repr().count() {
                    results.push(format!("(flags{}.bits() >> {}) as i32", tmp, i * 32));
                }
            }
            Instruction::FlagsLift { flags, ty, .. } => {
                let repr = RustFlagsRepr::new(flags);
                let name = self.gen.type_path(*ty, true);
                let mut result = format!("{name}::empty()");
                for (i, op) in operands.iter().enumerate() {
                    result.push_str(&format!(
                        " | {name}::from_bits_retain((({op} as {repr}) << {}) as _)",
                        i * 32
                    ));
                }
                results.push(result);
            }

            Instruction::HandleLower {
                handle: Handle::Own(_),
                ..
            } => {
                let op = &operands[0];
                results.push(format!("({op}).into_handle()"))
            }

            Instruction::HandleLower {
                handle: Handle::Borrow(_),
                ..
            } => {
                let op = &operands[0];
                results.push(format!("({op}).handle"))
            }

            Instruction::HandleLift { handle, .. } => {
                let op = &operands[0];
                let (prefix, resource, owned) = match handle {
                    Handle::Borrow(resource) => ("&", resource, false),
                    Handle::Own(resource) => ("", resource, true),
                };
                let resource = dealias(resolve, *resource);
                let name = self.gen.type_path(resource, true);

                results.push(
                    if let Direction::Export = self.gen.gen.resources[&resource].direction {
                        match handle {
                            Handle::Borrow(_) => format!(
                                "core::mem::transmute::<isize, &Rep{name}>\
                                 ({op}.try_into().unwrap())"
                            ),
                            Handle::Own(_) => format!("Own{name}::from_handle({op})"),
                        }
                    } else {
                        format!("{prefix}{name}::from_handle({op}, {owned})")
                    },
                );
            }

            Instruction::RecordLower { ty, record, .. } => {
                self.record_lower(*ty, record, &operands[0], results);
            }
            Instruction::RecordLift { ty, record, .. } => {
                self.record_lift(*ty, record, operands, results);
            }

            Instruction::TupleLower { tuple, .. } => {
                self.tuple_lower(tuple, &operands[0], results);
            }
            Instruction::TupleLift { .. } => {
                self.tuple_lift(operands, results);
            }

            Instruction::VariantPayloadName => results.push("e".to_string()),

            Instruction::VariantLower {
                variant,
                results: result_types,
                ty,
                ..
            } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - variant.cases.len()..)
                    .collect::<Vec<_>>();
                self.let_results(result_types.len(), results);
                let op0 = &operands[0];
                self.push_str(&format!("match {op0} {{\n"));
                let name = self.typename_lower(*ty);
                for (case, block) in variant.cases.iter().zip(blocks) {
                    let case_name = case.name.to_upper_camel_case();
                    self.push_str(&format!("{name}::{case_name}"));
                    if case.ty.is_some() {
                        self.push_str(&format!("(e) => {block},\n"));
                    } else {
                        self.push_str(&format!(" => {{\n{block}\n}}\n"));
                    }
                }
                self.push_str("};\n");
            }

            Instruction::VariantLift {
                name, variant, ty, ..
            } => {
                let mut result = String::new();
                result.push_str("{");

                let named_enum = variant.cases.iter().all(|c| c.ty.is_none());
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - variant.cases.len()..)
                    .collect::<Vec<_>>();
                let op0 = &operands[0];

                if named_enum {
                    // In unchecked mode when this type is a named enum then we know we
                    // defined the type so we can transmute directly into it.
                    result.push_str("#[cfg(not(debug_assertions))]");
                    result.push_str("{");
                    result.push_str("::core::mem::transmute::<_, ");
                    result.push_str(&name.to_upper_camel_case());
                    result.push_str(">(");
                    result.push_str(op0);
                    result.push_str(" as ");
                    result.push_str(int_repr(variant.tag()));
                    result.push_str(")");
                    result.push_str("}");
                }

                if named_enum {
                    result.push_str("#[cfg(debug_assertions)]");
                }
                result.push_str("{");
                result.push_str(&format!("match {op0} {{\n"));
                let name = self.typename_lift(*ty);
                for (i, (case, block)) in variant.cases.iter().zip(blocks).enumerate() {
                    let pat = i.to_string();
                    let block = if case.ty.is_some() {
                        format!("({block})")
                    } else {
                        String::new()
                    };
                    let case = case.name.to_upper_camel_case();
                    if i == variant.cases.len() - 1 {
                        result.push_str("#[cfg(debug_assertions)]");
                        result.push_str(&format!("{pat} => {name}::{case}{block},\n"));
                        result.push_str("#[cfg(not(debug_assertions))]");
                        result.push_str(&format!("_ => {name}::{case}{block},\n"));
                    } else {
                        result.push_str(&format!("{pat} => {name}::{case}{block},\n"));
                    }
                }
                result.push_str("#[cfg(debug_assertions)]");
                result.push_str("_ => panic!(\"invalid enum discriminant\"),\n");
                result.push_str("}");
                result.push_str("}");

                result.push_str("}");
                results.push(result);
            }

            Instruction::UnionLower {
                union,
                results: result_types,
                ty,
                ..
            } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - union.cases.len()..)
                    .collect::<Vec<_>>();
                self.let_results(result_types.len(), results);
                let op0 = &operands[0];
                self.push_str(&format!("match {op0} {{\n"));
                let name = self.typename_lower(*ty);
                for (case_name, block) in self.gen.union_case_names(union).into_iter().zip(blocks) {
                    self.push_str(&format!("{name}::{case_name}(e) => {block},\n"));
                }
                self.push_str("};\n");
            }

            Instruction::UnionLift { union, ty, .. } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - union.cases.len()..)
                    .collect::<Vec<_>>();
                let op0 = &operands[0];
                let mut result = format!("match {op0} {{\n");
                for (i, (case_name, block)) in self
                    .gen
                    .union_case_names(union)
                    .into_iter()
                    .zip(blocks)
                    .enumerate()
                {
                    let pat = i.to_string();
                    let name = self.typename_lift(*ty);
                    if i == union.cases.len() - 1 {
                        result.push_str("#[cfg(debug_assertions)]");
                        result.push_str(&format!("{pat} => {name}::{case_name}({block}),\n"));
                        result.push_str("#[cfg(not(debug_assertions))]");
                        result.push_str(&format!("_ => {name}::{case_name}({block}),\n"));
                    } else {
                        result.push_str(&format!("{pat} => {name}::{case_name}({block}),\n"));
                    }
                }
                result.push_str("#[cfg(debug_assertions)]");
                result.push_str("_ => panic!(\"invalid union discriminant\"),\n");
                result.push_str("}");
                results.push(result);
            }

            Instruction::OptionLower {
                results: result_types,
                ..
            } => {
                let some = self.blocks.pop().unwrap();
                let none = self.blocks.pop().unwrap();
                self.let_results(result_types.len(), results);
                let operand = &operands[0];
                self.push_str(&format!(
                    "match {operand} {{
                        Some(e) => {some},
                        None => {{\n{none}\n}},
                    }};"
                ));
            }

            Instruction::OptionLift { .. } => {
                let some = self.blocks.pop().unwrap();
                let none = self.blocks.pop().unwrap();
                assert_eq!(none, "()");
                let operand = &operands[0];
                results.push(format!(
                    "match {operand} {{
                        0 => None,
                        1 => Some({some}),
                        #[cfg(not(debug_assertions))]
                        _ => ::core::hint::unreachable_unchecked(),
                        #[cfg(debug_assertions)]
                        _ => panic!(\"invalid enum discriminant\"),
                    }}"
                ));
            }

            Instruction::ResultLower {
                results: result_types,
                result,
                ..
            } => {
                let err = self.blocks.pop().unwrap();
                let ok = self.blocks.pop().unwrap();
                self.let_results(result_types.len(), results);
                let operand = &operands[0];
                let ok_binding = if result.ok.is_some() { "e" } else { "_" };
                let err_binding = if result.err.is_some() { "e" } else { "_" };
                self.push_str(&format!(
                    "match {operand} {{
                        Ok({ok_binding}) => {{ {ok} }},
                        Err({err_binding}) => {{ {err} }},
                    }};"
                ));
            }

            Instruction::ResultLift { .. } => {
                let err = self.blocks.pop().unwrap();
                let ok = self.blocks.pop().unwrap();
                let operand = &operands[0];
                results.push(format!(
                    "match {operand} {{
                        0 => Ok({ok}),
                        1 => Err({err}),
                        #[cfg(not(debug_assertions))]
                        _ => ::core::hint::unreachable_unchecked(),
                        #[cfg(debug_assertions)]
                        _ => panic!(\"invalid enum discriminant\"),
                    }}"
                ));
            }

            Instruction::EnumLower { enum_, ty, .. } => {
                let mut result = format!("match {} {{\n", operands[0]);
                let name = self.gen.type_path(*ty, true);
                for (i, case) in enum_.cases.iter().enumerate() {
                    let case = case.name.to_upper_camel_case();
                    result.push_str(&format!("{name}::{case} => {i},\n"));
                }
                result.push_str("}");
                results.push(result);
            }

            Instruction::EnumLift { enum_, ty, .. } => {
                let mut result = String::new();
                result.push_str("{");

                // In checked mode do a `match`.
                result.push_str("#[cfg(debug_assertions)]");
                result.push_str("{");
                result.push_str("match ");
                result.push_str(&operands[0]);
                result.push_str(" {\n");
                let name = self.gen.type_path(*ty, true);
                for (i, case) in enum_.cases.iter().enumerate() {
                    let case = case.name.to_upper_camel_case();
                    result.push_str(&format!("{i} => {name}::{case},\n"));
                }
                result.push_str("_ => panic!(\"invalid enum discriminant\"),\n");
                result.push_str("}");
                result.push_str("}");

                // In unchecked mode when this type is a named enum then we know we
                // defined the type so we can transmute directly into it.
                result.push_str("#[cfg(not(debug_assertions))]");
                result.push_str("{");
                result.push_str("::core::mem::transmute::<_, ");
                result.push_str(&self.gen.type_path(*ty, true));
                result.push_str(">(");
                result.push_str(&operands[0]);
                result.push_str(" as ");
                result.push_str(int_repr(enum_.tag()));
                result.push_str(")");
                result.push_str("}");

                result.push_str("}");
                results.push(result);
            }

            Instruction::ListCanonLower { realloc, .. } => {
                let tmp = self.tmp();
                let val = format!("vec{}", tmp);
                let ptr = format!("ptr{}", tmp);
                let len = format!("len{}", tmp);
                if realloc.is_none() {
                    self.push_str(&format!("let {} = {};\n", val, operands[0]));
                } else {
                    let op0 = operands.pop().unwrap();
                    self.push_str(&format!("let {} = ({}).into_boxed_slice();\n", val, op0));
                }
                self.push_str(&format!("let {} = {}.as_ptr() as i32;\n", ptr, val));
                self.push_str(&format!("let {} = {}.len() as i32;\n", len, val));
                if realloc.is_some() {
                    self.push_str(&format!("::core::mem::forget({});\n", val));
                }
                results.push(ptr);
                results.push(len);
            }

            Instruction::ListCanonLift { .. } => {
                let tmp = self.tmp();
                let len = format!("len{}", tmp);
                self.push_str(&format!("let {} = {} as usize;\n", len, operands[1]));
                let result = format!(
                    "Vec::from_raw_parts({} as *mut _, {1}, {1})",
                    operands[0], len
                );
                results.push(result);
            }

            Instruction::StringLower { realloc } => {
                let tmp = self.tmp();
                let val = format!("vec{}", tmp);
                let ptr = format!("ptr{}", tmp);
                let len = format!("len{}", tmp);
                if realloc.is_none() {
                    self.push_str(&format!("let {} = {};\n", val, operands[0]));
                } else {
                    let op0 = format!("{}.into_bytes()", operands[0]);
                    self.push_str(&format!("let {} = ({}).into_boxed_slice();\n", val, op0));
                }
                self.push_str(&format!("let {} = {}.as_ptr() as i32;\n", ptr, val));
                self.push_str(&format!("let {} = {}.len() as i32;\n", len, val));
                if realloc.is_some() {
                    self.push_str(&format!("::core::mem::forget({});\n", val));
                }
                results.push(ptr);
                results.push(len);
            }

            Instruction::StringLift => {
                let tmp = self.tmp();
                let len = format!("len{}", tmp);
                self.push_str(&format!("let {} = {} as usize;\n", len, operands[1]));
                let result = format!(
                    "Vec::from_raw_parts({} as *mut _, {1}, {1})",
                    operands[0], len
                );
                if self.gen.gen.opts.raw_strings {
                    results.push(result);
                } else {
                    let mut converted = String::new();
                    converted.push_str("{");

                    converted.push_str("#[cfg(not(debug_assertions))]");
                    converted.push_str("{");
                    converted.push_str(&format!("String::from_utf8_unchecked({})", result));
                    converted.push_str("}");

                    converted.push_str("#[cfg(debug_assertions)]");
                    converted.push_str("{");
                    converted.push_str(&format!("String::from_utf8({}).unwrap()", result));
                    converted.push_str("}");

                    converted.push_str("}");
                    results.push(converted);
                }
            }

            Instruction::ListLower { element, realloc } => {
                let body = self.blocks.pop().unwrap();
                let tmp = self.tmp();
                let vec = format!("vec{tmp}");
                let result = format!("result{tmp}");
                let layout = format!("layout{tmp}");
                let len = format!("len{tmp}");
                self.push_str(&format!(
                    "let {vec} = {operand0};\n",
                    operand0 = operands[0]
                ));
                self.push_str(&format!("let {len} = {vec}.len() as i32;\n"));
                let size = self.gen.sizes.size(element);
                let align = self.gen.sizes.align(element);
                self.push_str(&format!(
                    "let {layout} = alloc::Layout::from_size_align_unchecked({vec}.len() * {size}, {align});\n",
                ));
                self.push_str(&format!(
                    "let {result} = if {layout}.size() != 0\n{{\nlet ptr = alloc::alloc({layout});\n",
                ));
                self.push_str(&format!(
                    "if ptr.is_null()\n{{\nalloc::handle_alloc_error({layout});\n}}\nptr\n}}",
                ));
                self.push_str(&format!("else {{\n::core::ptr::null_mut()\n}};\n",));
                self.push_str(&format!("for (i, e) in {vec}.into_iter().enumerate() {{\n",));
                self.push_str(&format!(
                    "let base = {result} as i32 + (i as i32) * {size};\n",
                ));
                self.push_str(&body);
                self.push_str("}\n");
                results.push(format!("{result} as i32"));
                results.push(len);

                if realloc.is_none() {
                    // If an allocator isn't requested then we must clean up the
                    // allocation ourselves since our callee isn't taking
                    // ownership.
                    self.cleanup.push((result, layout));
                }
            }

            Instruction::ListLift { element, .. } => {
                let body = self.blocks.pop().unwrap();
                let tmp = self.tmp();
                let size = self.gen.sizes.size(element);
                let align = self.gen.sizes.align(element);
                let len = format!("len{tmp}");
                let base = format!("base{tmp}");
                let result = format!("result{tmp}");
                self.push_str(&format!(
                    "let {base} = {operand0};\n",
                    operand0 = operands[0]
                ));
                self.push_str(&format!(
                    "let {len} = {operand1};\n",
                    operand1 = operands[1]
                ));
                self.push_str(&format!(
                    "let mut {result} = Vec::with_capacity({len} as usize);\n",
                ));

                self.push_str("for i in 0..");
                self.push_str(&len);
                self.push_str(" {\n");
                self.push_str("let base = ");
                self.push_str(&base);
                self.push_str(" + i *");
                self.push_str(&size.to_string());
                self.push_str(";\n");
                self.push_str(&result);
                self.push_str(".push(");
                self.push_str(&body);
                self.push_str(");\n");
                self.push_str("}\n");
                results.push(result);
                self.push_str(&format!(
                    "wit_bindgen::rt::dealloc({base}, ({len} as usize) * {size}, {align});\n",
                ));
            }

            Instruction::IterElem { .. } => results.push("e".to_string()),

            Instruction::IterBasePointer => results.push("base".to_string()),

            Instruction::CallWasm { name, sig, .. } => {
                let func = self.declare_import(
                    self.gen.wasm_import_module.unwrap(),
                    name,
                    &sig.params,
                    &sig.results,
                );

                // ... then call the function with all our operands
                if sig.results.len() > 0 {
                    self.push_str("let ret = ");
                    results.push("ret".to_string());
                }
                self.push_str(&func);
                self.push_str("(");
                self.push_str(&operands.join(", "));
                self.push_str(");\n");
            }

            Instruction::CallInterface { func, .. } => {
                self.let_results(func.results.len(), results);
                match &func.kind {
                    FunctionKind::Freestanding => {
                        self.push_str(&format!(
                            "<{0}Impl as {0}>::{1}",
                            self.trait_name.unwrap(),
                            to_rust_ident(&func.name)
                        ));
                    }
                    FunctionKind::Method(ty) | FunctionKind::Static(ty) => {
                        self.push_str(&format!(
                            "<Rep{0} as {0}>::{1}",
                            resolve.types[*ty]
                                .name
                                .as_deref()
                                .unwrap()
                                .to_upper_camel_case(),
                            to_rust_ident(func.item_name())
                        ));
                    }
                    FunctionKind::Constructor(ty) => {
                        self.push_str(&format!(
                            "Own{0}::new(<Rep{0} as {0}>::new",
                            resolve.types[*ty]
                                .name
                                .as_deref()
                                .unwrap()
                                .to_upper_camel_case()
                        ));
                    }
                }
                self.push_str("(");
                self.push_str(&operands.join(", "));
                self.push_str(")");
                if let FunctionKind::Constructor(_) = &func.kind {
                    self.push_str(")");
                }
                self.push_str(";\n");
            }

            Instruction::Return { amt, .. } => {
                self.emit_cleanup();
                match amt {
                    0 => {}
                    1 => {
                        self.push_str(&operands[0]);
                        self.push_str("\n");
                    }
                    _ => {
                        self.push_str("(");
                        self.push_str(&operands.join(", "));
                        self.push_str(")\n");
                    }
                }
            }

            Instruction::I32Load { offset } => {
                results.push(format!("*(({} + {}) as *const i32)", operands[0], offset));
            }
            Instruction::I32Load8U { offset } => {
                results.push(format!(
                    "i32::from(*(({} + {}) as *const u8))",
                    operands[0], offset
                ));
            }
            Instruction::I32Load8S { offset } => {
                results.push(format!(
                    "i32::from(*(({} + {}) as *const i8))",
                    operands[0], offset
                ));
            }
            Instruction::I32Load16U { offset } => {
                results.push(format!(
                    "i32::from(*(({} + {}) as *const u16))",
                    operands[0], offset
                ));
            }
            Instruction::I32Load16S { offset } => {
                results.push(format!(
                    "i32::from(*(({} + {}) as *const i16))",
                    operands[0], offset
                ));
            }
            Instruction::I64Load { offset } => {
                results.push(format!("*(({} + {}) as *const i64)", operands[0], offset));
            }
            Instruction::F32Load { offset } => {
                results.push(format!("*(({} + {}) as *const f32)", operands[0], offset));
            }
            Instruction::F64Load { offset } => {
                results.push(format!("*(({} + {}) as *const f64)", operands[0], offset));
            }
            Instruction::I32Store { offset } => {
                self.push_str(&format!(
                    "*(({} + {}) as *mut i32) = {};\n",
                    operands[1], offset, operands[0]
                ));
            }
            Instruction::I32Store8 { offset } => {
                self.push_str(&format!(
                    "*(({} + {}) as *mut u8) = ({}) as u8;\n",
                    operands[1], offset, operands[0]
                ));
            }
            Instruction::I32Store16 { offset } => {
                self.push_str(&format!(
                    "*(({} + {}) as *mut u16) = ({}) as u16;\n",
                    operands[1], offset, operands[0]
                ));
            }
            Instruction::I64Store { offset } => {
                self.push_str(&format!(
                    "*(({} + {}) as *mut i64) = {};\n",
                    operands[1], offset, operands[0]
                ));
            }
            Instruction::F32Store { offset } => {
                self.push_str(&format!(
                    "*(({} + {}) as *mut f32) = {};\n",
                    operands[1], offset, operands[0]
                ));
            }
            Instruction::F64Store { offset } => {
                self.push_str(&format!(
                    "*(({} + {}) as *mut f64) = {};\n",
                    operands[1], offset, operands[0]
                ));
            }

            Instruction::Malloc { .. } => unimplemented!(),

            Instruction::GuestDeallocate { size, align } => {
                self.push_str(&format!(
                    "wit_bindgen::rt::dealloc({}, {}, {});\n",
                    operands[0], size, align
                ));
            }

            Instruction::GuestDeallocateString => {
                self.push_str(&format!(
                    "wit_bindgen::rt::dealloc({}, ({}) as usize, 1);\n",
                    operands[0], operands[1],
                ));
            }

            Instruction::GuestDeallocateVariant { blocks } => {
                let max = blocks - 1;
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - blocks..)
                    .collect::<Vec<_>>();
                let op0 = &operands[0];
                self.src.push_str(&format!("match {op0} {{\n"));
                for (i, block) in blocks.into_iter().enumerate() {
                    let pat = if i == max {
                        String::from("_")
                    } else {
                        i.to_string()
                    };
                    self.src.push_str(&format!("{pat} => {block},\n"));
                }
                self.src.push_str("}\n");
            }

            Instruction::GuestDeallocateList { element } => {
                let body = self.blocks.pop().unwrap();
                let tmp = self.tmp();
                let size = self.gen.sizes.size(element);
                let align = self.gen.sizes.align(element);
                let len = format!("len{tmp}");
                let base = format!("base{tmp}");
                self.push_str(&format!(
                    "let {base} = {operand0};\n",
                    operand0 = operands[0]
                ));
                self.push_str(&format!(
                    "let {len} = {operand1};\n",
                    operand1 = operands[1]
                ));

                if body != "()" {
                    self.push_str("for i in 0..");
                    self.push_str(&len);
                    self.push_str(" {\n");
                    self.push_str("let base = ");
                    self.push_str(&base);
                    self.push_str(" + i *");
                    self.push_str(&size.to_string());
                    self.push_str(";\n");
                    self.push_str(&body);
                    self.push_str("\n}\n");
                }
                self.push_str(&format!(
                    "wit_bindgen::rt::dealloc({base}, ({len} as usize) * {size}, {align});\n",
                ));
            }
        }
    }
}

fn dealias(resolve: &Resolve, mut id: TypeId) -> TypeId {
    loop {
        match &resolve.types[id].kind {
            TypeDefKind::Type(Type::Id(that_id)) => id = *that_id,
            _ => break id,
        }
    }
}

fn group_by_resource<'a>(
    funcs: impl Iterator<Item = &'a Function>,
) -> BTreeMap<Option<TypeId>, Vec<&'a Function>> {
    let mut by_resource = BTreeMap::<_, Vec<_>>::new();
    for func in funcs {
        match &func.kind {
            FunctionKind::Freestanding => by_resource.entry(None).or_default().push(func),
            FunctionKind::Method(ty) | FunctionKind::Static(ty) | FunctionKind::Constructor(ty) => {
                by_resource.entry(Some(*ty)).or_default().push(func);
            }
        }
    }
    by_resource
}
