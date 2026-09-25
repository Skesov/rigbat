//! Module dependency rules from CLAUDE.md: dependencies point inward, and no
//! adapter imports another adapter. The crate is walked from `src/main.rs`
//! through its `mod` declarations; `#[cfg(test)]` items and modules are skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::{Ident, TokenStream, TokenTree};
use syn::visit::{self, Visit};
use syn::{Attribute, ImplItem, Item, ItemMod, ItemUse, Macro, UseTree};

type TestResult = Result<(), Box<dyn Error>>;

/// May import any module: they wire the others together.
const COMPOSITION_ROOTS: &[&str] = &["main", "app", "doctor"];

/// Shared ports: any adapter may use them. They import only `CORE` and each
/// other — never `config` or an adapter.
const SHARED_PORTS: &[&str] = &["i18n", "icon", "ipc", "gui", "appearance", "clock"];

/// Vocabulary and primitives with no infrastructure behind them.
const CORE: &[&str] = &["domain", "refresh"];

/// User intent (`config.json`): read by every surface, imports only `CORE`
/// and `i18n`.
const CONFIG: &str = "config";

const ADAPTERS: &[&str] = &[
    "autostart",
    "cli",
    "dashboard",
    "discovery",
    "notifications",
    "session",
    "settings",
    "sources",
    "state",
    "tray",
];

/// Adapter-to-adapter edges that are allowed, with the reason.
const ADAPTER_EDGES: &[(&str, &str, &str)] = &[
    (
        "discovery",
        "sources",
        "the registry enumerates the backends",
    ),
    (
        "session",
        "sources",
        "listens on the shared system bus (`sources::Context`)",
    ),
    ("settings", "app", "its own process: runs `poll_once`"),
    ("settings", "discovery", "its own process: runs discovery"),
    ("settings", "sources", "its own process: owns a `Context`"),
    ("settings", "state", "its own process: reads the inventory"),
    (
        "settings",
        "autostart",
        "its own process: toggles autostart",
    ),
];

/// `domain` must not link any of these.
const INFRA_CRATES: &[&str] = &[
    "ashpd",
    "directories",
    "eframe",
    "egui",
    "egui_extras",
    "ksni",
    "nix",
    "notify",
    "rusqlite",
    "tiny_skia",
    "tokio",
    "zbus",
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    from: String,
    to: String,
    at: String,
}

fn allowed(from: &str, to: &str) -> bool {
    if COMPOSITION_ROOTS.contains(&from) {
        return true;
    }
    if from == "domain" {
        return to == "i18n";
    }
    if from == "refresh" {
        return false;
    }
    if SHARED_PORTS.contains(&from) {
        return CORE.contains(&to) || SHARED_PORTS.contains(&to);
    }
    if from == CONFIG {
        return CORE.contains(&to) || to == "i18n";
    }
    if ADAPTERS.contains(&from) {
        return CORE.contains(&to)
            || SHARED_PORTS.contains(&to)
            || to == CONFIG
            || ADAPTER_EDGES.iter().any(|(f, t, _)| *f == from && *t == to);
    }
    false
}

fn known_module(module: &str) -> bool {
    COMPOSITION_ROOTS.contains(&module)
        || SHARED_PORTS.contains(&module)
        || CORE.contains(&module)
        || ADAPTERS.contains(&module)
        || module == CONFIG
}

#[test]
fn module_dependencies_point_inward() -> TestResult {
    let tree = SourceTree::load()?;
    let mut problems = Vec::new();
    for module in &tree.modules {
        if !known_module(module) {
            problems.push(format!("module `{module}` is not classified in this test"));
        }
    }
    for edge in tree.edges() {
        if !allowed(&edge.from, &edge.to) {
            problems.push(format!("{} -> {} ({})", edge.from, edge.to, edge.at));
        }
    }
    assert!(
        problems.is_empty(),
        "forbidden module dependencies:\n{}",
        problems.join("\n")
    );
    Ok(())
}

#[test]
fn module_graph_has_no_cycles() -> TestResult {
    let tree = SourceTree::load()?;
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for edge in tree.edges() {
        graph.entry(edge.from).or_default().insert(edge.to);
    }
    let cycles = find_cycles(&graph);
    assert!(
        cycles.is_empty(),
        "module dependency cycles:\n{}",
        cycles.join("\n")
    );
    Ok(())
}

#[test]
fn domain_links_no_infrastructure_crate() -> TestResult {
    let tree = SourceTree::load()?;
    let problems: Vec<String> = tree
        .roots
        .iter()
        .filter(|r| r.from == "domain" && INFRA_CRATES.contains(&r.name.as_str()))
        .map(|r| format!("{} at {}", r.name, r.at))
        .collect();
    assert!(
        problems.is_empty(),
        "domain uses infrastructure crates:\n{}",
        problems.join("\n")
    );
    Ok(())
}

/// A name a module refers to: the top-level item after `crate::`, or the
/// first segment of any other multi-segment path.
struct Reference {
    from: String,
    name: String,
    at: String,
}

struct SourceTree {
    modules: BTreeSet<String>,
    refs: Vec<Reference>,
    roots: Vec<Reference>,
}

impl SourceTree {
    fn load() -> Result<Self, Box<dyn Error>> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut tree = Self {
            modules: BTreeSet::from(["main".to_owned()]),
            refs: Vec::new(),
            roots: Vec::new(),
        };
        let mut queue = vec![(PathBuf::from("main.rs"), Vec::new())];
        while let Some((file, mod_path)) = queue.pop() {
            let ast = syn::parse_file(&fs::read_to_string(src.join(&file))?)?;
            let mut collector = Collector {
                file: &file,
                mod_path,
                refs: Vec::new(),
                roots: Vec::new(),
                child_mods: Vec::new(),
            };
            collector.visit_file(&ast);
            tree.refs.append(&mut collector.refs);
            tree.roots.append(&mut collector.roots);
            for child in collector.child_mods {
                let Some((name, parents)) = child.split_last() else {
                    continue;
                };
                if parents.is_empty() {
                    tree.modules.insert(name.clone());
                }
                let dir: PathBuf = parents.iter().collect();
                let flat = dir.join(format!("{name}.rs"));
                let path = if src.join(&flat).exists() {
                    flat
                } else {
                    dir.join(name).join("mod.rs")
                };
                queue.push((path, child));
            }
        }
        Ok(tree)
    }

    fn edges(&self) -> Vec<Edge> {
        self.refs
            .iter()
            .map(|r| Edge {
                from: r.from.clone(),
                to: if self.modules.contains(&r.name) {
                    r.name.clone()
                } else {
                    "main".to_owned()
                },
                at: r.at.clone(),
            })
            .filter(|e| e.from != e.to)
            .collect()
    }
}

struct Collector<'a> {
    file: &'a Path,
    /// Module path of the item being visited, e.g. `["tray", "manager"]`.
    mod_path: Vec<String>,
    refs: Vec<Reference>,
    roots: Vec<Reference>,
    /// Module paths of `mod name;` declarations, whose bodies live in files.
    child_mods: Vec<Vec<String>>,
}

impl Collector<'_> {
    fn reference(&self, name: &Ident) -> Reference {
        Reference {
            from: self
                .mod_path
                .first()
                .cloned()
                .unwrap_or_else(|| "main".to_owned()),
            name: name.to_string(),
            at: format!("{}:{}", self.file.display(), name.span().start().line),
        }
    }

    fn record(&mut self, name: &Ident) {
        let r = self.reference(name);
        self.refs.push(r);
    }

    /// A path `a::b::…` given by its segments.
    fn path(&mut self, segments: &[&Ident]) {
        let supers = segments.iter().take_while(|s| **s == "super").count();
        match segments {
            [first, next, ..] if *first == "crate" => self.record(next),
            _ if supers > 0 => {
                if supers >= self.mod_path.len()
                    && let Some(next) = segments.get(supers)
                {
                    self.record(next);
                }
            }
            [first, _, ..] => {
                let r = self.reference(first);
                self.roots.push(r);
            }
            _ => {}
        }
    }

    fn use_tree(&mut self, tree: &UseTree, prefix: &mut Vec<Ident>) {
        match tree {
            UseTree::Path(p) => {
                prefix.push(p.ident.clone());
                self.use_tree(&p.tree, prefix);
                prefix.pop();
            }
            UseTree::Name(n) => self.use_leaf(prefix, &n.ident),
            UseTree::Rename(r) => self.use_leaf(prefix, &r.ident),
            UseTree::Glob(_) => self.path(&prefix.iter().collect::<Vec<_>>()),
            UseTree::Group(g) => {
                for item in &g.items {
                    self.use_tree(item, prefix);
                }
            }
        }
    }

    fn use_leaf(&mut self, prefix: &[Ident], leaf: &Ident) {
        let segments: Vec<&Ident> = prefix.iter().chain(std::iter::once(leaf)).collect();
        self.path(&segments);
    }

    /// Paths inside macro input, which `syn` leaves as tokens.
    fn tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        for (i, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(g) => self.tokens(g.stream()),
                TokenTree::Ident(_) if !follows_colon(&tokens, i) => {
                    let segments = path_at(&tokens, i);
                    self.path(&segments.iter().collect::<Vec<_>>());
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if !is_cfg_test(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        if !is_cfg_test(impl_item_attrs(item)) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_item_mod(&mut self, module: &'ast ItemMod) {
        let mut path = self.mod_path.clone();
        path.push(module.ident.to_string());
        match &module.content {
            None => self.child_mods.push(path),
            Some((_, items)) => {
                let outer = std::mem::replace(&mut self.mod_path, path);
                for item in items {
                    self.visit_item(item);
                }
                self.mod_path = outer;
            }
        }
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        self.use_tree(&item.tree, &mut Vec::new());
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments: Vec<&Ident> = path.segments.iter().map(|s| &s.ident).collect();
        self.path(&segments);
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        self.tokens(mac.tokens.clone());
        visit::visit_macro(self, mac);
    }
}

fn follows_colon(tokens: &[TokenTree], i: usize) -> bool {
    i.checked_sub(1)
        .and_then(|p| tokens.get(p))
        .is_some_and(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ':'))
}

/// Segments of the `a::b::c` token path starting at `start`.
fn path_at(tokens: &[TokenTree], start: usize) -> Vec<Ident> {
    let mut segments = Vec::new();
    let mut i = start;
    while let Some(TokenTree::Ident(ident)) = tokens.get(i) {
        segments.push(ident.clone());
        match (tokens.get(i + 1), tokens.get(i + 2)) {
            (Some(TokenTree::Punct(a)), Some(TokenTree::Punct(b)))
                if a.as_char() == ':' && b.as_char() == ':' =>
            {
                i += 3;
            }
            _ => break,
        }
    }
    segments
}

fn is_cfg_test(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().is_ident("cfg") && a.parse_args::<Ident>().is_ok_and(|arg| arg == "test"))
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(i) => &i.attrs,
        ImplItem::Fn(i) => &i.attrs,
        ImplItem::Type(i) => &i.attrs,
        ImplItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn find_cycles(graph: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    fn visit(
        node: &str,
        graph: &BTreeMap<String, BTreeSet<String>>,
        stack: &mut Vec<String>,
        done: &mut BTreeSet<String>,
        cycles: &mut Vec<String>,
    ) {
        if let Some(pos) = stack.iter().position(|n| n == node) {
            let mut cycle = stack[pos..].to_vec();
            cycle.push(node.to_owned());
            cycles.push(cycle.join(" -> "));
            return;
        }
        if done.contains(node) {
            return;
        }
        stack.push(node.to_owned());
        for next in graph.get(node).into_iter().flatten() {
            visit(next, graph, stack, done, cycles);
        }
        stack.pop();
        done.insert(node.to_owned());
    }
    let mut cycles = Vec::new();
    let mut done = BTreeSet::new();
    for node in graph.keys() {
        visit(node, graph, &mut Vec::new(), &mut done, &mut cycles);
    }
    cycles
}
