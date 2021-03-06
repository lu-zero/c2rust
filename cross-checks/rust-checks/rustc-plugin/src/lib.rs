#![feature(plugin_registrar, quote, rustc_private, try_from)]

extern crate rustc_plugin;
extern crate syntax;

#[macro_use]
extern crate matches;

extern crate cross_check_config as xcfg;

mod config;
mod xcheck_util;

use rustc_plugin::Registry;
use syntax::ast;
use syntax::fold;

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;

use syntax::ext::base::{SyntaxExtension, ExtCtxt, Annotatable, MultiItemModifier};
use syntax::ext::quote::rt::{ToTokens, ExtParseUtils};
use syntax::codemap::{Span, FileLoader, RealFileLoader};
use syntax::fold::Folder;
use syntax::symbol::Symbol;
use syntax::print::pprust;
use syntax::ptr::P;
use syntax::tokenstream::TokenTree;
use syntax::util::small_vector::SmallVector;

use xcheck_util::CrossCheckBuilder;

struct ScopeConfig<'xcfg> {
    file_name: Rc<String>, // FIXME: this should be a &str
    items: Option<Rc<xcfg::NamedItemList<'xcfg>>>,
    check_config: config::ScopeCheckConfig,

    // Index of the next field in this scope (if the scope is a structure)
    // We use this to keep track of the index/ident of the next field
    // in a tuple
    field_idx: Cell<usize>,
}

impl<'xcfg> ScopeConfig<'xcfg> {
    fn new(cfg: &'xcfg xcfg::Config, file_name: String,
           ccc: config::ScopeCheckConfig) -> ScopeConfig<'xcfg> {
        let items = cfg.get_file_items(&file_name)
                       .map(xcfg::NamedItemList::new)
                       .map(Rc::new);
        ScopeConfig {
            file_name: Rc::new(file_name),
            items: items,
            check_config: ccc,
            field_idx: Cell::new(0),
        }
    }

    fn get_item_config(&self, item: &str) -> Option<&'xcfg xcfg::ItemConfig> {
        self.items
            .as_ref()
            .and_then(|nil| nil.name_map.get(item))
            .cloned()
    }

    fn from_item(&self, item_config: Option<&'xcfg xcfg::ItemConfig>,
                 ccc: config::ScopeCheckConfig) -> Self {
        ScopeConfig {
            file_name: self.file_name.clone(),
            items: item_config.and_then(xcfg::ItemConfig::nested_items)
                              .map(xcfg::NamedItemList::new)
                              .map(Rc::new),
            check_config: ccc,
            field_idx: Cell::new(0),
        }
    }

    fn same_file(&self, file_name: &str) -> bool {
        *self.file_name == file_name
    }
}

struct CrossChecker<'a, 'cx: 'a, 'exp> {
    expander: &'exp CrossCheckExpander,
    cx: &'a mut ExtCtxt<'cx>,
    scope_stack: Vec<ScopeConfig<'exp>>,
    default_ahasher: Vec<TokenTree>,
    default_shasher: Vec<TokenTree>,

    // New items to add at the next item boundary
    pending_items: Vec<P<ast::Item>>,

    // Whether to skip calling build_new_scope() on the first scope.
    // We set this to true for #[cross_check(...)] invocations caused
    // by macro expansions, since the compiler passes the attribute to us
    // in mi: &MetaItem and not in the item's actual attribute list,
    // so we need to skip parsing the latter.
    skip_first_scope: bool,
}

fn find_cross_check_attr(attrs: &[ast::Attribute]) -> Option<&ast::Attribute> {
    attrs.iter().find(|attr| attr.check_name("cross_check"))
}

impl<'a, 'cx, 'exp> CrossChecker<'a, 'cx, 'exp> {
    fn new(expander: &'exp CrossCheckExpander,
           cx: &'a mut ExtCtxt<'cx>,
           top_scope: ScopeConfig<'exp>,
           skip_first_scope: bool) -> CrossChecker<'a, 'cx, 'exp> {
        let default_ahasher = {
            let q = quote_ty!(cx, ::cross_check_runtime::hash::jodyhash::JodyHasher);
            q.to_tokens(cx)
        };
        let default_shasher = {
            let q = quote_ty!(cx, ::cross_check_runtime::hash::simple::SimpleHasher);
            q.to_tokens(cx)
        };
        CrossChecker {
            expander: expander,
            cx: cx,
            scope_stack: vec![top_scope],
            default_ahasher: default_ahasher,
            default_shasher: default_shasher,
            pending_items: vec![],
            skip_first_scope: skip_first_scope,
        }
    }

    #[inline]
    fn last_scope(&self) -> &ScopeConfig<'exp> {
        self.scope_stack.last().unwrap()
    }

    #[inline]
    fn config(&self) -> &config::ScopeCheckConfig {
        &self.last_scope().check_config
    }

    fn build_new_scope(&self, item: &ast::Item) -> ScopeConfig<'exp> {
        let span = match item.node {
            ast::ItemKind::Mod(ref m) => m.inner,
            _ => item.span
        };
        let last_scope = self.last_scope();
        let mod_file_name = self.cx.codemap().span_to_filename(span);
        let mod_file_name = mod_file_name.to_string();
        let same_file = last_scope.same_file(&mod_file_name);

        // Check if there are any file-level defaults, and if so, apply them
        let file_defaults_config = if !same_file {
            self.expander.build_file_defaults_config(self.cx,
                                                     self.config(),
                                                     &mod_file_name)
        } else { None };
        let mut new_config = if let Some(cfg) = file_defaults_config {
            // Build new config from file defaults
            cfg.inherit(item)
        } else {
            // Inherit from parent scope
            self.config().inherit(item)
        };

        // We have either a #[cross_check] attribute
        // or external config, so create a new ScopeCheckConfig
        // TODO: order???
        let xcheck_attr = find_cross_check_attr(&item.attrs);
        if let Some(ref attr) = xcheck_attr {
            let mi = attr.parse_meta(self.cx.parse_sess).unwrap();
            new_config.parse_attr_config(self.cx, &mi);
        };

        let item_xcfg_config = {
            let item_ident_str = item.ident.name.as_str();
            // If the item is an impl for a type, e.g.:
            // `impl T { ... }`, then we take its name
            // from the type, not from the identifier
            let item_name = match item.node {
                ast::ItemKind::Impl(.., ref ty, _) => {
                    // FIXME: handle generics in the type
                    Cow::from(pprust::ty_to_string(ty))
                }
                _ => Cow::from(&*item_ident_str)
            };
            last_scope.get_item_config(&*item_name)
        };
        if let Some(ref xcfg) = item_xcfg_config {
            new_config.parse_xcfg_config(self.cx, xcfg);
        };

        // Since rustc switched FileName from String to an enum,
        // we need to convert it to String ourselves, using format!
        // FIXME: ideally, we find a better way to handle all the
        // different types of virtual files from FileName
        if !same_file {
            // We should only ever get a file name mismatch
            // at the top of a module
            assert_matches!(item.node, ast::ItemKind::Mod(_));
            ScopeConfig::new(&self.expander.external_config, mod_file_name, new_config)
        } else {
            last_scope.from_item(item_xcfg_config, new_config)
        }
    }

    // Get the ahasher/shasher pair
    fn get_hasher_pair(&self) -> (&Vec<TokenTree>, &Vec<TokenTree>) {
        (self.config().inherited.ahasher.as_ref().unwrap_or(self.default_ahasher.as_ref()),
         self.config().inherited.shasher.as_ref().unwrap_or(self.default_shasher.as_ref()))
    }

    // Get the cross-check block for this argument
    fn build_arg_xcheck(&self, arg: &ast::Arg) -> Option<ast::Stmt> {
        match arg.pat.node {
            ast::PatKind::Ident(_, ref ident, _) => {
                // Parameter pattern is just an identifier,
                // so we can reference it directly by name
                let arg_idx = xcfg::FieldIndex::from_str(&*ident.node.name.as_str());
                let arg_xcheck_cfg = self.config().function_config()
                    .args.get(&arg_idx)
                    .unwrap_or(&self.config().inherited.all_args);
                arg_xcheck_cfg.build_xcheck(self.cx, "FUNCTION_ARG_TAG", "val_ref",
                                            |tag, pre_hash_stmts| {
                    // By default, we use cross_check_hash
                    // to hash the value of the identifier
                    let (ahasher, shasher) = self.get_hasher_pair();
                    quote_expr!(self.cx, {
                        use cross_check_runtime::hash::CrossCheckHash as XCH;
                        let val_ref = &$ident;
                        $pre_hash_stmts
                        let hash = XCH::cross_check_hash::<$ahasher, $shasher>(val_ref);
                        hash.map(|hash| ($tag, hash))
                    })
                })
            }
            _ => unimplemented!()
        }
    }

    // Create the arguments for #[cross_check_hash]
    // FIXME: we need to store them as strings, since there
    // doesn't seem to be a good way to create NestedMetaItems
    fn build_hash_attr_args(&self) -> Vec<String> {
        let mut res: Vec<String> = vec![];
        if let Some(ref ahasher) = self.config().inherited.ahasher {
            let ahasher_str = pprust::tts_to_string(
                &ahasher.to_tokens(self.cx));
            let mi = format!("ahasher=\"{}\"", ahasher_str);
            res.push(mi);
        }
        if let Some(ref shasher) = self.config().inherited.shasher {
            let shasher_str = pprust::tts_to_string(
                &shasher.to_tokens(self.cx));
            let mi = format!("shasher=\"{}\"", shasher_str);
            res.push(mi);
        }
        let struct_config = self.config().struct_config();
        if let Some(ref field_hasher) = struct_config.field_hasher.as_ref() {
            let mi = format!("field_hasher=\"{}\"", field_hasher);
            res.push(mi);
        }
        if let Some(ref custom_hash) = struct_config.custom_hash.as_ref() {
            let mi = format!("custom_hash=\"{}\"", custom_hash);
            res.push(mi);
        }
        res
    }

    fn build_extra_xchecks(&self, extra_xchecks: &[xcfg::ExtraXCheck]) -> Vec<ast::Stmt> {
        extra_xchecks.iter().flat_map(|ex| {
            // TODO: allow the custom functions to return Option or an iterator???
            let expr = self.cx.parse_expr(ex.custom.clone());
            let tag_str = match ex.tag {
                xcfg::XCheckTag::Unknown        => "UNKNOWN_TAG",
                xcfg::XCheckTag::FunctionEntry  => "FUNCTION_ENTRY_TAG",
                xcfg::XCheckTag::FunctionExit   => "FUNCTION_EXIT_TAG",
                xcfg::XCheckTag::FunctionArg    => "FUNCTION_ARG_TAG",
                xcfg::XCheckTag::FunctionReturn => "FUNCTION_RETURN_TAG",
            };
            let tag = ast::Ident::from_str(tag_str);
            quote_stmt!(self.cx, cross_check_raw!($tag, $expr))
        }).collect::<Vec<ast::Stmt>>()
    }

    fn build_function_xchecks(&mut self, fn_ident: &ast::Ident,
                              fn_decl: &ast::FnDecl,
                              block: P<ast::Block>) -> P<ast::Block> {
        let checked_block = if self.config().inherited.enabled {
            // Add the cross-check to the beginning of the function
            // TODO: only add the checks to C abi functions???
            let ref cfg = self.config();
            let entry_xcheck = cfg.inherited.entry
                .build_ident_xcheck(self.cx, "FUNCTION_ENTRY_TAG", fn_ident);
            let exit_xcheck = cfg.inherited.exit
                .build_ident_xcheck(self.cx, "FUNCTION_EXIT_TAG", fn_ident);
            // Insert cross-checks for function arguments
            let arg_xchecks = fn_decl.inputs.iter()
                .flat_map(|ref arg| self.build_arg_xcheck(arg))
                .collect::<Vec<ast::Stmt>>();
            let result_xcheck = cfg.inherited.ret
                .build_xcheck(self.cx, "FUNCTION_RETURN_TAG", "val_ref",
                              |tag, pre_hash_stmts| {
                // By default, we use cross_check_hash
                // to hash the value of the identifier
                let (ahasher, shasher) = self.get_hasher_pair();
                quote_expr!(self.cx, {
                    use cross_check_runtime::hash::CrossCheckHash as XCH;
                    let val_ref = &__c2rust_fn_result;
                    $pre_hash_stmts
                    let hash = XCH::cross_check_hash::<$ahasher, $shasher>(val_ref);
                    hash.map(|hash| ($tag, hash))
                })
            });

            let ref fcfg = cfg.function_config();
            let entry_extra_xchecks = self.build_extra_xchecks(&fcfg.entry_extra);
            let exit_extra_xchecks = self.build_extra_xchecks(&fcfg.exit_extra);
            // Extract the result type from the function signature,
            // so we can attach it to the __c2rust_fn_body closure
            let result_ty = match fn_decl.output {
                ast::FunctionRetTy::Default(_) => quote_ty!(self.cx, ()),
                ast::FunctionRetTy::Ty(ref ty) => ty.clone(),
            };
            quote_block!(self.cx, {
                $entry_xcheck
                $arg_xchecks
                $entry_extra_xchecks
                let mut __c2rust_fn_body = || -> $result_ty { $block };
                let __c2rust_fn_result = __c2rust_fn_body();
                $exit_xcheck
                $result_xcheck
                $exit_extra_xchecks
                __c2rust_fn_result
            })
        } else {
            block
        };
        // Add our typedefs to the beginning of each function;
        // whatever the configuration says, we should always add these
        let (ahasher, shasher) = self.get_hasher_pair();
        quote_block!(self.cx, {
            #[allow(dead_code)]
            mod cross_check_types {
                pub type DefaultAggHasher    = $ahasher;
                pub type DefaultSimpleHasher = $shasher;
            };
            $checked_block
        })
    }

    fn build_union_hash(&mut self, union_ident: &ast::Ident) -> P<ast::Item> {
        let custom_hash_opt = &self.config().struct_config().custom_hash;
        let hash_body = if let Some(ref custom_hash) = *custom_hash_opt {
            // User provided a custom hash function, use it
            self.cx.parse_expr(custom_hash.clone())
        } else {
            // TODO: emit warning
            quote_expr!(self.cx, {
                if _depth == 0 {
                    ::cross_check_runtime::hash::LEAF_RECORD_HASH
                } else {
                    ::cross_check_runtime::hash::ANY_UNION_HASH
                }
            })
        };
        quote_item!(self.cx,
            impl ::cross_check_runtime::hash::CrossCheckHash for $union_ident {
                #[inline]
                fn cross_check_hash_depth<HA, HS>(&self, _depth: usize) -> u64
                        where HA: ::cross_check_runtime::hash::CrossCheckHasher,
                              HS: ::cross_check_runtime::hash::CrossCheckHasher {
                    $hash_body
                }
            }
        ).expect(&format!("unable to implement CrossCheckHash for union '{}'",
                          union_ident.to_string()))
    }

    #[cfg(not(feature="c-hash-functions"))]
    fn build_type_c_hash_function(&mut self, _: &ast::Ident) -> Option<P<ast::Item>> {
        assert!(cfg!(feature="c-hash-functions")); // Expected to fail, is intentional
        None
    }

    #[cfg(feature="c-hash-functions")]
    fn build_type_c_hash_function(&mut self, ty_ident: &ast::Ident) -> Option<P<ast::Item>> {
        assert!(cfg!(feature="c-hash-functions"));
        let hash_fn_name = format!("__c2rust_hash_{}", ty_ident);
        let hash_fn = ast::Ident::from_str(&hash_fn_name);

        // Check if function has already been emitted;
        // FIXME: should this check be optional (compile-time feature)???
        if !self.expander.c_hash_functions.borrow_mut().insert(hash_fn_name) {
            return None;
        }

        let (ahasher, shasher) = self.get_hasher_pair();
        Some(quote_item!(self.cx,
            #[no_mangle]
            pub unsafe extern "C" fn $hash_fn(x: *mut $ty_ident, depth: usize) -> u64 {
                use ::cross_check_runtime::hash::CrossCheckHash;
                CrossCheckHash::cross_check_hash_depth::<$ahasher, $shasher>(&*x, depth)
            }
        ).expect(&format!("unable to implement C ABI hash function for type '{}'",
                          ty_ident.to_string())))
    }

    fn internal_fold_item_simple(&mut self, item: ast::Item) -> ast::Item {
        let folded_item = fold::noop_fold_item_simple(item, self);
        match folded_item.node {
            ast::ItemKind::Fn(fn_decl, unsafety, constness, abi, generics, block) => {
                let checked_block = self.build_function_xchecks(
                    &folded_item.ident, &*fn_decl, block);
                let checked_fn = ast::ItemKind::Fn(
                    fn_decl,
                    unsafety,
                    constness,
                    abi,
                    generics,
                    checked_block);
                // Build and return the replacement function item
                ast::Item {
                    node: checked_fn,
                    ..folded_item
                }
            }
            ast::ItemKind::Union(_, _) => {
                let union_hash_impl = self.build_union_hash(&folded_item.ident);
                self.pending_items.push(union_hash_impl);
                if cfg!(feature="c-hash-functions") {
                    let c_hash_func = self.build_type_c_hash_function(&folded_item.ident);
                    self.pending_items.extend(c_hash_func.into_iter());
                }
                folded_item
            }
            ast::ItemKind::Enum(_, _) |
            ast::ItemKind::Struct(_, _) => {
                // Prepend #[derive(CrossCheckHash)] automatically
                // to every structure definition
                let mut item_attrs = folded_item.attrs;
                if self.config().inherited.enabled {
                    let xcheck_hash_derive_attr = quote_attr!(self.cx, #[derive(CrossCheckHash)]);
                    item_attrs.push(xcheck_hash_derive_attr);

                    let attr_args = self.cx.parse_tts(self.build_hash_attr_args().join(","));
                    if !attr_args.is_empty() {
                        let xcheck_hash_attr = quote_attr!(self.cx, #[cross_check_hash($attr_args)]);
                        item_attrs.push(xcheck_hash_attr);
                    }

                    if cfg!(feature="c-hash-functions") {
                        let c_hash_func = self.build_type_c_hash_function(&folded_item.ident);
                        self.pending_items.extend(c_hash_func.into_iter());
                    }
                }
                ast::Item {
                    attrs: item_attrs,
                    ..folded_item
                }
            }
            ast::ItemKind::Mac(_) => {
                if !cfg!(feature = "expand_macros") {
                    self.expander.insert_macro_scope(folded_item.span, &self.config());
                }
                folded_item
            }
            _ => folded_item
        }
    }

    // Parse the #[cross_check(...)] attribute and turn it into a XCheck
    fn parse_field_attr(&self, attrs: &[ast::Attribute]) -> Option<xcfg::XCheckType> {
        let xcheck_attr = find_cross_check_attr(attrs);
        xcheck_attr.and_then(|attr| {
            attr.parse_meta(self.cx.parse_sess).ok().and_then(|mi| {
                let args = xcfg::attr::get_syntax_item_args(&mi);
                xcheck_util::parse_xcheck_arglist(&args)
            })
        })
    }
}

impl<'a, 'cx, 'exp> Folder for CrossChecker<'a, 'cx, 'exp> {
    fn fold_item_simple(&mut self, item: ast::Item) -> ast::Item {
        if self.skip_first_scope {
            // If skip_first_scope is true, skip building a new scope
            // (see the comment for skip_first_scope in CrossChecker above)
            self.skip_first_scope = false;
            self.internal_fold_item_simple(item)
        } else {
            let new_scope = self.build_new_scope(&item);
            self.scope_stack.push(new_scope);
            let new_item = self.internal_fold_item_simple(item);
            self.scope_stack.pop();
            new_item
        }
    }

    fn fold_impl_item(&mut self, item: ast::ImplItem) -> SmallVector<ast::ImplItem> {
        match item.node {
            ast::ImplItemKind::Method(sig, body) => {
                // FIXME: this is a bit hacky: we forcibly build a fake
                // Item::Fn with the same signature and body as our method,
                // then add cross-checks to that one
                let fake_item = ast::Item {
                    ident:  item.ident,
                    attrs:  item.attrs,
                    id:     item.id,
                    vis:    item.vis,
                    span:   item.span,
                    tokens: item.tokens,
                    node: ast::ItemKind::Fn(sig.decl, sig.unsafety,
                                            sig.constness, sig.abi,
                                            item.generics, body)
                };
                let folded_fake_item = self.fold_item_simple(fake_item);
                let (folded_sig, folded_generics, folded_body) = match folded_fake_item.node {
                    ast::ItemKind::Fn(decl, unsafety, constness, abi, generics, body) => {
                        let sig = ast::MethodSig {
                            unsafety: unsafety,
                            constness: constness,
                            abi: abi,
                            decl: decl
                        };
                        // TODO: call noop_fold_method_sig on sig???
                        (sig, generics, body)
                    }
                    n @ _ => panic!("unexpected folded item node: {:?}", n)
                };
                SmallVector::one(ast::ImplItem {
                    ident:  folded_fake_item.ident,
                    attrs:  folded_fake_item.attrs,
                    id:     folded_fake_item.id,
                    vis:    folded_fake_item.vis,
                    span:   folded_fake_item.span,
                    tokens: folded_fake_item.tokens,
                    defaultness: item.defaultness,
                    generics: folded_generics,
                    node: ast::ImplItemKind::Method(folded_sig, folded_body)
                })
            }
            _ => fold::noop_fold_impl_item(item, self)
        }
    }

    fn fold_stmt(&mut self, s: ast::Stmt) -> SmallVector<ast::Stmt> {
       if cfg!(feature = "expand-macros") {
           if let ast::StmtKind::Mac(_) = s.node {
               return self.cx.expander().fold_stmt(s)
                   .into_iter()
                   .flat_map(|stmt| self.fold_stmt(stmt).into_iter())
                   .collect();
           }
       } else {
           self.expander.insert_macro_scope(s.span, &self.config());
       }

       let folded_stmt = fold::noop_fold_stmt(s, self);
       folded_stmt.into_iter().flat_map(|s| {
           let new_stmt = match s.node {
               ast::StmtKind::Local(ref local) => {
                   let attr = find_cross_check_attr(&*local.attrs);
                   // TODO: check that the cross_check attr is "yes"
                   attr.and_then(|_| {
                       // TODO: only add cross-checks for initialized locals???
                       // (in other words, check local.init.is_some())
                       match local.pat.node {
                           ast::PatKind::Ident(_, ident, _) => {
                               Some(quote_stmt!(self.cx, cross_check_value!($ident)).unwrap())
                           },
                           // TODO: handle more pattern types
                           _ => None
                       }
                   })
               },
               _ => None
           };
           Some(s).into_iter()
                  .chain(new_stmt.into_iter())
                  .collect::<Vec<_>>()
       }).collect()
    }

    fn fold_variant_data(&mut self, vdata: ast::VariantData) -> ast::VariantData {
        self.last_scope().field_idx.set(0);
        fold::noop_fold_variant_data(vdata, self)
    }

    fn fold_struct_field(&mut self, sf: ast::StructField) -> ast::StructField {
        let folded_sf = fold::noop_fold_struct_field(sf, self);

        // Get the name of the field; the compiler should give us the name
        // if the field is in a Struct. If it's in a Tuple, we use
        // the field index, which we need to compute ourselves from field_idx.
        let sf_name = folded_sf.ident
            .map(|ident| xcfg::FieldIndex::from_str(&*ident.name.as_str()))
            .unwrap_or_else(|| {
                // We use field_idx to keep track of the index/name
                // of the fields inside a VariantData
                let idx = self.last_scope().field_idx.get();
                self.last_scope().field_idx.set(idx + 1);
                xcfg::FieldIndex::Int(idx)
            });

        let sf_attr_xcheck = self.parse_field_attr(&folded_sf.attrs);
        let sf_xcfg_xcheck = self.config().struct_config().fields.get(&sf_name);
        let sf_xcheck = sf_xcfg_xcheck.or(sf_attr_xcheck.as_ref());
        let hash_attr = sf_xcheck.and_then(|sf_xcheck| {
            match *sf_xcheck {
                xcfg::XCheckType::Default => None,

                xcfg::XCheckType::AsType(ref ty) =>
                    Some(quote_attr!(self.cx, #[cross_check_hash(as_type=$ty)])),

                xcfg::XCheckType::None |
                xcfg::XCheckType::Disabled =>
                    Some(quote_attr!(self.cx, #[cross_check_hash(none)])),

                xcfg::XCheckType::Djb2(_) => unimplemented!(),

                xcfg::XCheckType::Fixed(id) => {
                    // FIXME: we're passing the id in as a string because
                    // that's how derive-macros parses it
                    let sid = format!("{}", id);
                    Some(quote_attr!(self.cx, #[cross_check_hash(fixed_hash=$sid)]))
                },

                xcfg::XCheckType::Custom(ref s) =>
                    Some(quote_attr!(self.cx, #[cross_check_hash(custom_hash=$s)])),
            }
        });

        // Remove #[cross_check] from attributes, then append #[cross_check_hash]
        let sf_attrs = folded_sf.attrs.into_iter()
            .filter(|attr| !attr.check_name("cross_check"))
            .chain(hash_attr.into_iter())
            .collect();
        ast::StructField {
            attrs: sf_attrs,
            ..folded_sf
        }
    }

    // Fold functions that handle macro expansion
    fn fold_item(&mut self, item: P<ast::Item>) -> SmallVector<P<ast::Item>> {
        if cfg!(feature = "expand-macros") {
            if let ast::ItemKind::Mac(_) = item.node {
                return self.cx.expander().fold_item(item)
                    .into_iter()
                    .flat_map(|item| self.fold_item(item).into_iter())
                    .collect();
            }
        }
        let mut res = fold::noop_fold_item(item, self);
        // Add the pending items
        res.extend(self.pending_items.drain(..));
        res
    }

    fn fold_expr(&mut self, expr: P<ast::Expr>) -> P<ast::Expr> {
        if cfg!(feature = "expand-macros") {
            if let ast::ExprKind::Mac(_) = expr.node {
                return self.cx.expander().fold_expr(expr)
                    .map(|e| fold::noop_fold_expr(e, self));
            }
        } else {
           self.expander.insert_macro_scope(expr.span, &self.config());
        }
        expr.map(|e| fold::noop_fold_expr(e, self))
    }

    // TODO: fold_block???

    fn fold_foreign_item(&mut self, ni: ast::ForeignItem) -> ast::ForeignItem {
        let folded_ni = fold::noop_fold_foreign_item(ni, self);
        if let ast::ForeignItemKind::Ty = folded_ni.node {
            // Foreign type, implement CrossCheckHash for it
            // This is implemented as a call to the `__c2rust_hash_T` function
            // TODO: include ahasher/shasher into the function name
            // TODO: configure this via attribute&external configuration
            //       * option to disable CrossCheckHash altogether
            //       * option to use a custom function
            let ty_name = folded_ni.ident;
            let hash_fn_name = format!("__c2rust_hash_{}", ty_name);
            let hash_fn = ast::Ident::from_str(&hash_fn_name);
            let hash_impl_item = quote_item!(self.cx,
                impl ::cross_check_runtime::hash::CrossCheckHash for $ty_name {
                    #[inline]
                    fn cross_check_hash_depth<HA, HS>(&self, depth: usize) -> u64
                            where HA: ::cross_check_runtime::hash::CrossCheckHasher,
                                  HS: ::cross_check_runtime::hash::CrossCheckHasher {
                        extern {
                            #[no_mangle]
                            fn $hash_fn(_: *const $ty_name, _: usize) -> u64;
                        }
                        unsafe { $hash_fn(self as *const $ty_name, depth) }
                    }
                }
            ).expect(&format!("unable to implement CrossCheckHash for foreign type '{}'", ty_name));
            self.pending_items.push(hash_impl_item);
        };
        folded_ni
    }

    fn fold_mac(&mut self, mac: ast::Mac) -> ast::Mac {
       mac
    }
}

#[derive(Default)]
struct CrossCheckExpander {
    // Arguments passed to plugin
    // TODO: pre-parse them???
    external_config: xcfg::Config,
    macro_scopes: RefCell<HashMap<Span, Rc<config::InheritedCheckConfig>>>,

    // List of already emitted C ABI hash functions,
    // used to prevent the emission of duplicates
    #[cfg(feature="c-hash-functions")]
    c_hash_functions: RefCell<HashSet<String>>,
}

impl CrossCheckExpander {
    fn new(args: &[ast::NestedMetaItem]) -> CrossCheckExpander {
        CrossCheckExpander {
            external_config: CrossCheckExpander::parse_config_files(args),
            macro_scopes: Default::default(),
            ..Default::default()
        }
    }

    fn parse_config_files(args: &[ast::NestedMetaItem]) -> xcfg::Config {
        // Parse arguments of the form
        // #[plugin(cross_check_plugin(config_file = "..."))]
        let fl = RealFileLoader;
        args.iter()
            .filter(|nmi| nmi.check_name("config_file"))
            .map(|mi| mi.value_str().expect("invalid string for config_file"))
            .map(|fsym| PathBuf::from(&*fsym.as_str()))
            .map(|fp| fl.abs_path(&fp)
                        .expect(&format!("invalid path to config file: {:?}", fp)))
            .map(|fp| fl.read_file(&fp)
                        .expect(&format!("could not read config file: {:?}", fp)))
            // TODO: use a Reader to read&parse each configuration file
            // without storing its contents in an intermediate String buffer???
            .map(|fd| xcfg::parse_string(&fd).expect("could not parse config file"))
            .fold(Default::default(), |acc, fc| acc.merge(fc))
    }

    fn insert_macro_scope(&self, sp: Span, config: &config::ScopeCheckConfig) {
        self.macro_scopes.borrow_mut().insert(sp, Rc::clone(&config.inherited));
    }

    fn find_span_scope(&self, sp: Span) -> Option<Rc<config::InheritedCheckConfig>> {
        let macro_scopes = self.macro_scopes.borrow();
        macro_scopes.get(&sp).cloned().or_else(|| {
            sp.ctxt().outer().expn_info().and_then(|ei| {
                self.find_span_scope(ei.call_site)
            })
        })
    }

    /// Build a FileDefaults ScopeCheckConfig for the given file,
    /// if we have any FileDefaults in the external configuration
    fn build_file_defaults_config(&self, cx: &ExtCtxt, parent: &config::ScopeCheckConfig,
                                  file_name: &str) -> Option<config::ScopeCheckConfig> {
        let file_items = self.external_config.get_file_items(file_name);
        file_items.map(|file_items| {
            let mut new_config = parent.new_file();
            let mut file_cfg = xcfg::DefaultsConfig::default();
            for item in file_items.items().iter() {
                match item {
                    &xcfg::ItemConfig::Defaults(ref def) => file_cfg.merge(def),
                    _ => (),
                }
            }
            let file_item_cfg = xcfg::ItemConfig::Defaults(file_cfg);
            new_config.parse_xcfg_config(cx, &file_item_cfg);
            new_config
        })
    }
}

impl MultiItemModifier for CrossCheckExpander {
    fn expand(&self,
              cx: &mut ExtCtxt,
              sp: Span,
              mi: &ast::MetaItem,
              item: Annotatable) -> Vec<Annotatable> {
        match item {
            Annotatable::Item(i) => {
                let span_scope = self.find_span_scope(sp);
                // If we're seeing #![cross_check] at the top of the crate or a module,
                // create a fresh configuration and perform a folding; otherwise, just
                // ignore this expansion and let the higher level one do everything
                let ni = match (&i.node, span_scope) {
                    (&ast::ItemKind::Mod(_), None) => {
                        let mut top_config = config::ScopeCheckConfig::new();
                        top_config.parse_attr_config(cx, mi);
                        let top_file_name = cx.codemap().span_to_filename(sp);
                        let top_file_name = top_file_name.to_string();
                        // FIXME: do we need to build a FileDefaults???
                        let top_config = self.build_file_defaults_config(cx, &top_config,
                                                                         &top_file_name)
                            .unwrap_or(top_config);
                        let top_scope = ScopeConfig::new(&self.external_config,
                                                         top_file_name,
                                                         top_config);
                        CrossChecker::new(self, cx, top_scope, true)
                            .fold_item(i)
                            .expect_one("too many items returned")
                    }
                    (_, Some(scope_config)) => {
                        // If this #[cross_check(...)] expansion is caused by a
                        // macro expansion, handle it here
                        let mut config = config::ScopeCheckConfig::from_item(&i, scope_config);
                        config.parse_attr_config(cx, mi);
                        let file_name = cx.codemap().span_to_filename(sp);
                        let file_name = file_name.to_string();
                        // TODO: build a FileDefaults???
                        let scope = ScopeConfig::new(&self.external_config,
                                                     file_name,
                                                     config);
                        CrossChecker::new(self, cx, scope, true)
                            .fold_item(i)
                            .expect_one("too many items returned")
                    }
                    (_, None) => i
                };
                Annotatable::Item(ni).into()
            }
            // TODO: handle TraitItem
            // TODO: handle ImplItem
            _ => panic!("unexpected item: {:?}", item),
        }
    }
}

#[plugin_registrar]
pub fn plugin_registrar(reg: &mut Registry) {
    let ecc = CrossCheckExpander::new(reg.args());
    // TODO: parse args
    reg.register_syntax_extension(
        Symbol::intern("cross_check"),
        SyntaxExtension::MultiModifier(Box::new(ecc)));
}
