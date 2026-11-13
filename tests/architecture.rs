//! Source-level architecture gates, including inactive production target branches.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{Attribute, Item, Meta, Token, UseTree};

fn production_values(meta: &Meta) -> u8 {
    match meta {
        Meta::Path(path) if path.is_ident("test") => 2,
        Meta::List(list) => {
            let arguments = list
                .parse_args_with(syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated)
                .expect("valid cfg arguments");
            let values: Vec<_> = arguments.iter().map(production_values).collect();
            if list.path.is_ident("not") && values.len() == 1 {
                ((values[0] & 1) << 1) | ((values[0] & 2) >> 1)
            } else if list.path.is_ident("all") {
                u8::from(values.iter().all(|v| v & 1 != 0))
                    | (u8::from(values.iter().any(|v| v & 2 != 0)) << 1)
            } else if list.path.is_ident("any") {
                u8::from(values.iter().any(|v| v & 1 != 0))
                    | (u8::from(values.iter().all(|v| v & 2 != 0)) << 1)
            } else {
                3
            }
        }
        _ => 3,
    }
}

fn production(attrs: &[Attribute]) -> bool {
    attrs.iter().all(|attr| {
        !attr.path().is_ident("cfg")
            || production_values(&attr.parse_args::<Meta>().expect("valid cfg")) & 1 != 0
    })
}

fn attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(v) => &v.attrs,
        Item::Enum(v) => &v.attrs,
        Item::ExternCrate(v) => &v.attrs,
        Item::Fn(v) => &v.attrs,
        Item::ForeignMod(v) => &v.attrs,
        Item::Impl(v) => &v.attrs,
        Item::Macro(v) => &v.attrs,
        Item::Mod(v) => &v.attrs,
        Item::Static(v) => &v.attrs,
        Item::Struct(v) => &v.attrs,
        Item::Trait(v) => &v.attrs,
        Item::TraitAlias(v) => &v.attrs,
        Item::Type(v) => &v.attrs,
        Item::Union(v) => &v.attrs,
        Item::Use(v) => &v.attrs,
        _ => &[],
    }
}

fn name(ident: &syn::Ident) -> String {
    ident.to_string().trim_start_matches("r#").to_owned()
}

struct Gate<'a> {
    layer: &'a str,
    module: Vec<String>,
    directory: Option<PathBuf>,
    dependencies: &'a BTreeSet<String>,
    errors: Vec<String>,
}

impl Gate<'_> {
    fn check(&mut self, parts: &[String]) {
        let Some(first) = parts.first().map(String::as_str) else {
            return;
        };
        let mut local = None;
        if first == "crate" {
            local = Some(parts[1..].to_vec());
        } else if matches!(first, "self" | "super") {
            let mut module = self.module.clone();
            let mut index = 0;
            while let Some(part) = parts.get(index) {
                match part.as_str() {
                    "super" => {
                        module.pop();
                    }
                    "self" => {}
                    _ => break,
                }
                index += 1;
            }
            module.extend_from_slice(&parts[index..]);
            local = Some(module);
        }
        if let Some(local) = local {
            let allowed: &[&str] = match self.layer {
                "domain" => &["domain"],
                "runtime" => &["runtime", "domain"],
                "sched" => &["sched", "domain"],
                "store" => &["store", "domain", "ids", "timing"],
                "api" => &["api", "app", "domain", "protocol"],
                _ => unreachable!(),
            };
            if !local
                .first()
                .is_some_and(|root| allowed.contains(&root.as_str()))
            {
                self.errors.push(format!(
                    "{} must not depend on {}",
                    self.layer,
                    parts.join("::")
                ));
            }
        }
        if matches!(self.layer, "domain" | "runtime" | "sched") {
            let io = first == "std"
                && (parts.len() == 1
                    || parts.get(1).is_some_and(|p| {
                        ["fs", "io", "net", "process", "thread", "env", "os"].contains(&p.as_str())
                    })
                    || parts
                        .iter()
                        .any(|p| ["Instant", "SystemTime"].contains(&p.as_str())));
            let external = self.dependencies.contains(first)
                && !(self.layer == "domain" && ["serde", "thiserror"].contains(&first));
            if io || external {
                self.errors.push(format!(
                    "{} must stay pure: {}",
                    self.layer,
                    parts.join("::")
                ));
            }
        }
        if self.layer == "api"
            && parts
                .iter()
                .any(|p| ["Mutex", "RwLock"].contains(&p.as_str()))
        {
            self.errors
                .push("API must not own coordination locks".into());
        }
    }

    fn imports(&mut self, tree: &UseTree, prefix: Vec<String>) {
        match tree {
            UseTree::Path(path) => {
                let mut prefix = prefix;
                prefix.push(name(&path.ident));
                self.imports(&path.tree, prefix);
            }
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.imports(tree, prefix.clone());
                }
            }
            UseTree::Name(value) => {
                let mut parts = prefix;
                if value.ident != "self" {
                    parts.push(name(&value.ident));
                }
                self.check(&parts);
            }
            UseTree::Rename(value) => {
                let mut parts = prefix;
                if value.ident != "self" {
                    parts.push(name(&value.ident));
                }
                self.check(&parts);
            }
            UseTree::Glob(_) => self.check(&prefix),
        }
    }
}

impl<'ast> Visit<'ast> for Gate<'_> {
    fn visit_file(&mut self, file: &'ast syn::File) {
        if production(&file.attrs) {
            visit::visit_file(self, file);
        }
    }
    fn visit_item(&mut self, item: &'ast Item) {
        if production(attributes(item)) {
            visit::visit_item(self, item);
        }
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.module.push(name(&item.ident));
        let directory = self.directory.clone();
        self.directory = directory.as_ref().map(|dir| dir.join(name(&item.ident)));
        if item.content.is_some() {
            visit::visit_item_mod(self, item);
        } else if let Some(directory) = &directory {
            let explicit = item.attrs.iter().find_map(|attr| {
                if attr.path().is_ident("path")
                    && let Meta::NameValue(value) = &attr.meta
                    && let syn::Expr::Lit(value) = &value.value
                    && let syn::Lit::Str(value) = &value.lit
                {
                    return Some(directory.join(value.value()));
                }
                None
            });
            let file = explicit.unwrap_or_else(|| {
                let flat = directory.join(format!("{}.rs", name(&item.ident)));
                if flat.exists() {
                    flat
                } else {
                    directory.join(name(&item.ident)).join("mod.rs")
                }
            });
            self.directory = Some(if file.file_name().is_some_and(|name| name == "mod.rs") {
                file.parent().unwrap().to_path_buf()
            } else {
                file.with_extension("")
            });
            let source = std::fs::read_to_string(&file)
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            let parsed = syn::parse_file(&source)
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            let mut nested = Gate {
                layer: self.layer,
                module: self.module.clone(),
                directory: self.directory.clone(),
                dependencies: self.dependencies,
                errors: Vec::new(),
            };
            nested.visit_file(&parsed);
            self.errors.extend(
                nested
                    .errors
                    .into_iter()
                    .map(|error| format!("{}: {error}", file.display())),
            );
        }
        self.directory = directory;
        self.module.pop();
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if production(&item.attrs) {
            visit::visit_impl_item_fn(self, item);
        }
    }
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.imports(&item.tree, Vec::new());
    }
    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        self.check(&[name(&item.ident)]);
    }
    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.check(
            &path
                .segments
                .iter()
                .map(|p| name(&p.ident))
                .collect::<Vec<_>>(),
        );
        visit::visit_path(self, path);
    }
    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        if self.layer == "api"
            && ["store", "lock", "try_lock"].contains(&name(&expression.method).as_str())
        {
            self.errors.push(format!(
                "API must use application operations, not {}()",
                expression.method
            ));
        }
        visit::visit_expr_method_call(self, expression);
    }
    fn visit_expr_field(&mut self, expression: &'ast syn::ExprField) {
        if self.layer == "api"
            && matches!(&expression.member, syn::Member::Named(id) if name(id) == "store")
        {
            self.errors
                .push("API must not access the store field".into());
        }
        visit::visit_expr_field(self, expression);
    }
}

fn inspect(
    layer: &str,
    module: Vec<String>,
    source: &str,
    dependencies: &BTreeSet<String>,
    directory: Option<PathBuf>,
) -> Vec<String> {
    let file = syn::parse_file(source).expect("parse Rust source");
    let mut gate = Gate {
        layer,
        module,
        directory,
        dependencies,
        errors: Vec::new(),
    };
    gate.visit_file(&file);
    gate.errors
}

#[test]
fn production_dependencies_follow_module_boundaries() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(root.join("Cargo.toml")).unwrap()).unwrap();
    let dependencies = manifest["dependencies"]
        .as_table()
        .unwrap()
        .keys()
        .map(|name| name.replace('-', "_"))
        .collect();
    for layer in ["domain", "runtime", "sched", "store", "api"] {
        let directory = root.join("src").join(layer);
        let errors = inspect(
            layer,
            vec![layer.into()],
            &std::fs::read_to_string(directory.join("mod.rs")).unwrap(),
            &dependencies,
            Some(directory),
        );
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }
}

#[test]
fn boundary_gate_rejects_real_dependencies_and_access() {
    let dependencies = ["tokio", "rusqlite", "libc"].map(String::from).into();
    for (layer, source) in [
        ("domain", "use crate::{domain::job::JobState, app::App};"),
        ("runtime", "use std::{fs as disk, collections::HashMap};"),
        ("sched", "fn f() { tokio::spawn(async {}); }"),
        ("domain", "use std as os; fn f() { os::fs::read(\"x\"); }"),
        ("store", "use super::super::app::App;"),
        ("api", "fn f(s: S) { s.store().job(\"j\"); }"),
        ("api", "fn f(s: S) { s.store.lock(); }"),
        (
            "domain",
            "#[cfg(any(test, target_os = \"linux\"))] use crate::app::App;",
        ),
        ("domain", "#[cfg(not(test))] use crate::app::App;"),
        ("runtime", "extern crate libc as os;"),
        ("domain", "use crate::r#store::Store;"),
    ] {
        assert!(
            !inspect(
                layer,
                vec![layer.into(), "fixture".into()],
                source,
                &dependencies,
                None,
            )
            .is_empty(),
            "{layer}: {source}"
        );
    }
}

#[test]
fn boundary_gate_keeps_test_code_and_value_types_separate() {
    for source in [
        "#[cfg(test)] mod tests { use crate::store::Store; }",
        "#[cfg(all(test, target_os = \"macos\"))] fn test() { std::fs::read(\"x\"); }",
        "impl Value { #[cfg(test)] fn test() { std::fs::read(\"x\"); } }",
        "use std::{path::Path, time::Duration}; use crate::domain::job::JobState;",
        "// use crate::store::Store;\nconst EXAMPLE: &str = \"std::fs::read\";",
    ] {
        assert!(
            inspect(
                "domain",
                vec!["domain".into()],
                source,
                &BTreeSet::new(),
                None
            )
            .is_empty(),
            "{source}"
        );
    }
}

#[test]
fn boundary_gate_follows_production_modules_without_opening_test_modules() {
    let directory = tempfile::tempdir().unwrap();
    let source = "#[cfg(test)] mod tests; mod child;";
    std::fs::write(
        directory.path().join("tests.rs"),
        "use crate::store::Store;",
    )
    .unwrap();
    let child = directory.path().join("child.rs");
    std::fs::write(&child, "use std::path::Path;").unwrap();
    let check = || {
        inspect(
            "domain",
            vec!["domain".into()],
            source,
            &BTreeSet::new(),
            Some(directory.path().to_path_buf()),
        )
    };
    assert!(check().is_empty());
    std::fs::write(child, "use crate::store::Store;").unwrap();
    assert!(check().iter().any(|error| error.contains("child.rs")));
}
