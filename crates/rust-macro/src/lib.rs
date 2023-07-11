use proc_macro2::{Span, TokenStream};
use std::path::{Path, PathBuf};
use syn::parse::{Error, Parse, ParseStream, Result};
use syn::punctuated::Punctuated;
use syn::{braced, token, Token};
use wit_bindgen_core::wit_parser::{PackageId, Resolve, UnresolvedPackage, WorldId};
use wit_bindgen_rust::Opts;
use wit_bindgen_rust_lib::Ownership;

#[proc_macro]
pub fn generate(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    syn::parse_macro_input!(input as Config)
        .expand()
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

struct Config {
    opts: Opts,
    resolve: Resolve,
    world: WorldId,
    files: Vec<PathBuf>,
}

enum Source {
    Path(String),
    Inline(String),
}

impl Parse for Config {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let call_site = Span::call_site();
        let mut opts = Opts::default();
        let mut world = None;
        let mut source = None;

        if input.peek(token::Brace) {
            let content;
            syn::braced!(content in input);
            let fields = Punctuated::<Opt, Token![,]>::parse_terminated(&content)?;
            for field in fields.into_pairs() {
                match field.into_value() {
                    Opt::Path(s) => {
                        if source.is_some() {
                            return Err(Error::new(s.span(), "cannot specify second source"));
                        }
                        source = Some(Source::Path(s.value()));
                    }
                    Opt::World(s) => {
                        if world.is_some() {
                            return Err(Error::new(s.span(), "cannot specify second world"));
                        }
                        world = Some(s.value());
                    }
                    Opt::Inline(s) => {
                        if source.is_some() {
                            return Err(Error::new(s.span(), "cannot specify second source"));
                        }
                        source = Some(Source::Inline(s.value()));
                    }
                    Opt::UseStdFeature => opts.std_feature = true,
                    Opt::RawStrings => opts.raw_strings = true,
                    Opt::Ownership(ownership) => opts.ownership = ownership,
                    Opt::Skip(list) => opts.skip.extend(list.iter().map(|i| i.value())),
                    Opt::WorldExports(ident) => opts.world_exports = Some(ident.to_string()),
                    Opt::InterfaceExports(exports) => opts.interface_exports.extend(
                        exports
                            .into_iter()
                            .map(|export| (export.key.value(), export.value.to_string())),
                    ),
                    Opt::ResourceExports(exports) => opts.resource_exports.extend(
                        exports
                            .into_iter()
                            .map(|export| (export.key.value(), export.value.to_string())),
                    ),
                    Opt::Stubs => {
                        opts.stubs = true;
                    }
                    Opt::ExportPrefix(prefix) => opts.export_prefix = Some(prefix.value()),
                }
            }
        } else {
            world = input.parse::<Option<syn::LitStr>>()?.map(|s| s.value());
            if input.parse::<Option<syn::token::In>>()?.is_some() {
                source = Some(Source::Path(input.parse::<syn::LitStr>()?.value()));
            }
        }
        let (resolve, pkg, files) =
            parse_source(&source).map_err(|err| Error::new(call_site, format!("{err:?}")))?;
        let world = resolve
            .select_world(pkg, world.as_deref())
            .map_err(|e| Error::new(call_site, format!("{e:?}")))?;
        Ok(Config {
            opts,
            resolve,
            world,
            files,
        })
    }
}

fn parse_source(source: &Option<Source>) -> anyhow::Result<(Resolve, PackageId, Vec<PathBuf>)> {
    let mut resolve = Resolve::default();
    let mut files = Vec::new();
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut parse = |path: &Path| -> anyhow::Result<_> {
        if path.is_dir() {
            let (pkg, sources) = resolve.push_dir(&path)?;
            files = sources;
            Ok(pkg)
        } else {
            let pkg = UnresolvedPackage::parse_file(path)?;
            files.extend(pkg.source_files().map(|s| s.to_owned()));
            resolve.push(pkg)
        }
    };
    let pkg = match source {
        Some(Source::Inline(s)) => {
            resolve.push(UnresolvedPackage::parse("macro-input".as_ref(), &s)?)?
        }
        Some(Source::Path(s)) => parse(&root.join(&s))?,
        None => parse(&root.join("wit"))?,
    };

    Ok((resolve, pkg, files))
}

impl Config {
    fn expand(self) -> Result<TokenStream> {
        let mut files = Default::default();
        self.opts
            .build()
            .generate(&self.resolve, self.world, &mut files);
        let (_, src) = files.iter().next().unwrap();
        let src = std::str::from_utf8(src).unwrap();
        let mut contents = src.parse::<TokenStream>().unwrap();

        // Include a dummy `include_str!` for any files we read so rustc knows that
        // we depend on the contents of those files.
        for file in self.files.iter() {
            contents.extend(
                format!("const _: &str = include_str!(r#\"{}\"#);\n", file.display())
                    .parse::<TokenStream>()
                    .unwrap(),
            );
        }

        Ok(contents)
    }
}

mod kw {
    syn::custom_keyword!(std_feature);
    syn::custom_keyword!(raw_strings);
    syn::custom_keyword!(skip);
    syn::custom_keyword!(world);
    syn::custom_keyword!(path);
    syn::custom_keyword!(inline);
    syn::custom_keyword!(ownership);
    syn::custom_keyword!(world_exports);
    syn::custom_keyword!(interface_exports);
    syn::custom_keyword!(resource_exports);
    syn::custom_keyword!(stubs);
    syn::custom_keyword!(export_prefix);
}

#[derive(Clone)]
struct Export {
    key: syn::LitStr,
    value: syn::Ident,
}

impl Parse for Export {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let key = input.parse()?;
        input.parse::<Token![:]>()?;
        let value = input.parse()?;
        Ok(Self { key, value })
    }
}

enum Opt {
    World(syn::LitStr),
    Path(syn::LitStr),
    Inline(syn::LitStr),
    UseStdFeature,
    RawStrings,
    Skip(Vec<syn::LitStr>),
    Ownership(Ownership),
    WorldExports(syn::Ident),
    InterfaceExports(Vec<Export>),
    ResourceExports(Vec<Export>),
    Stubs,
    ExportPrefix(syn::LitStr),
}

impl Parse for Opt {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let l = input.lookahead1();
        if l.peek(kw::path) {
            input.parse::<kw::path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::Path(input.parse()?))
        } else if l.peek(kw::inline) {
            input.parse::<kw::inline>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::Inline(input.parse()?))
        } else if l.peek(kw::world) {
            input.parse::<kw::world>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::World(input.parse()?))
        } else if l.peek(kw::std_feature) {
            input.parse::<kw::std_feature>()?;
            Ok(Opt::UseStdFeature)
        } else if l.peek(kw::raw_strings) {
            input.parse::<kw::raw_strings>()?;
            Ok(Opt::RawStrings)
        } else if l.peek(kw::ownership) {
            input.parse::<kw::ownership>()?;
            input.parse::<Token![:]>()?;
            let ownership = input.parse::<syn::Ident>()?;
            Ok(Opt::Ownership(match ownership.to_string().as_str() {
                "Owning" => Ownership::Owning,
                "Borrowing" => Ownership::Borrowing {
                    duplicate_if_necessary: {
                        let contents;
                        braced!(contents in input);
                        let field = contents.parse::<syn::Ident>()?;
                        match field.to_string().as_str() {
                            "duplicate_if_necessary" => {
                                contents.parse::<Token![:]>()?;
                                contents.parse::<syn::LitBool>()?.value
                            }
                            name => {
                                return Err(Error::new(
                                    field.span(),
                                    format!(
                                        "unrecognized `Ownership::Borrowing` field: `{name}`; \
                                         expected `duplicate_if_necessary`"
                                    ),
                                ));
                            }
                        }
                    },
                },
                name => {
                    return Err(Error::new(
                        ownership.span(),
                        format!(
                            "unrecognized ownership: `{name}`; \
                             expected `Owning` or `Borrowing`"
                        ),
                    ));
                }
            }))
        } else if l.peek(kw::world_exports) {
            input.parse::<kw::world_exports>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::WorldExports(input.parse()?))
        } else if l.peek(kw::interface_exports) {
            input.parse::<kw::interface_exports>()?;
            input.parse::<Token![:]>()?;
            let contents;
            syn::braced!(contents in input);
            let list = Punctuated::<_, Token![,]>::parse_terminated(&contents)?;
            Ok(Opt::InterfaceExports(list.iter().cloned().collect()))
        } else if l.peek(kw::resource_exports) {
            input.parse::<kw::resource_exports>()?;
            input.parse::<Token![:]>()?;
            let contents;
            syn::braced!(contents in input);
            let list = Punctuated::<_, Token![,]>::parse_terminated(&contents)?;
            Ok(Opt::ResourceExports(list.iter().cloned().collect()))
        } else if l.peek(kw::skip) {
            input.parse::<kw::skip>()?;
            input.parse::<Token![:]>()?;
            let contents;
            syn::bracketed!(contents in input);
            let list = Punctuated::<_, Token![,]>::parse_terminated(&contents)?;
            Ok(Opt::Skip(list.iter().cloned().collect()))
        } else if l.peek(kw::stubs) {
            input.parse::<kw::stubs>()?;
            Ok(Opt::Stubs)
        } else if l.peek(kw::export_prefix) {
            input.parse::<kw::export_prefix>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::ExportPrefix(input.parse()?))
        } else {
            Err(l.error())
        }
    }
}
