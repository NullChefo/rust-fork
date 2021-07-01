//! Handle syntactic aspects of inserting a new `use`.
use std::cmp::Ordering;

use hir::Semantics;
use syntax::{
    algo,
    ast::{self, make, AstNode, AttrsOwner, ModuleItemOwner, PathSegmentKind, VisibilityOwner},
    match_ast, ted, AstToken, Direction, NodeOrToken, SyntaxNode, SyntaxToken,
};

use crate::{
    helpers::merge_imports::{
        common_prefix, eq_attrs, eq_visibility, try_merge_imports, use_tree_path_cmp, MergeBehavior,
    },
    RootDatabase,
};

pub use hir::PrefixKind;

/// How imports should be grouped into use statements.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ImportGranularity {
    /// Do not change the granularity of any imports and preserve the original structure written by the developer.
    Preserve,
    /// Merge imports from the same crate into a single use statement.
    Crate,
    /// Merge imports from the same module into a single use statement.
    Module,
    /// Flatten imports so that each has its own use statement.
    Item,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InsertUseConfig {
    pub granularity: ImportGranularity,
    pub enforce_granularity: bool,
    pub prefix_kind: PrefixKind,
    pub group: bool,
    pub skip_glob_imports: bool,
}

#[derive(Debug, Clone)]
pub enum ImportScope {
    File(ast::SourceFile),
    Module(ast::ItemList),
    Block(ast::BlockExpr),
}

impl ImportScope {
    fn from(syntax: SyntaxNode) -> Option<Self> {
        fn contains_cfg_attr(attrs: &dyn AttrsOwner) -> bool {
            attrs
                .attrs()
                .any(|attr| attr.as_simple_call().map_or(false, |(ident, _)| ident == "cfg"))
        }
        match_ast! {
            match syntax {
                ast::Module(module) => module.item_list().map(ImportScope::Module),
                ast::SourceFile(file) => Some(ImportScope::File(file)),
                ast::Fn(func) => contains_cfg_attr(&func).then(|| func.body().map(ImportScope::Block)).flatten(),
                ast::Const(konst) => contains_cfg_attr(&konst).then(|| match konst.body()? {
                    ast::Expr::BlockExpr(block) => Some(block),
                    _ => None,
                }).flatten().map(ImportScope::Block),
                ast::Static(statik) => contains_cfg_attr(&statik).then(|| match statik.body()? {
                    ast::Expr::BlockExpr(block) => Some(block),
                    _ => None,
                }).flatten().map(ImportScope::Block),
                _ => None,

            }
        }
    }

    /// Determines the containing syntax node in which to insert a `use` statement affecting `position`.
    pub fn find_insert_use_container_with_macros(
        position: &SyntaxNode,
        sema: &Semantics<'_, RootDatabase>,
    ) -> Option<Self> {
        sema.ancestors_with_macros(position.clone()).find_map(Self::from)
    }

    /// Determines the containing syntax node in which to insert a `use` statement affecting `position`.
    pub fn find_insert_use_container(position: &SyntaxNode) -> Option<Self> {
        std::iter::successors(Some(position.clone()), SyntaxNode::parent).find_map(Self::from)
    }

    pub fn as_syntax_node(&self) -> &SyntaxNode {
        match self {
            ImportScope::File(file) => file.syntax(),
            ImportScope::Module(item_list) => item_list.syntax(),
            ImportScope::Block(block) => block.syntax(),
        }
    }

    pub fn clone_for_update(&self) -> Self {
        match self {
            ImportScope::File(file) => ImportScope::File(file.clone_for_update()),
            ImportScope::Module(item_list) => ImportScope::Module(item_list.clone_for_update()),
            ImportScope::Block(block) => ImportScope::Block(block.clone_for_update()),
        }
    }

    fn guess_granularity_from_scope(&self) -> ImportGranularityGuess {
        // The idea is simple, just check each import as well as the import and its precedent together for
        // whether they fulfill a granularity criteria.
        let use_stmt = |item| match item {
            ast::Item::Use(use_) => {
                let use_tree = use_.use_tree()?;
                Some((use_tree, use_.visibility(), use_.attrs()))
            }
            _ => None,
        };
        let mut use_stmts = match self {
            ImportScope::File(f) => f.items(),
            ImportScope::Module(m) => m.items(),
            ImportScope::Block(b) => b.items(),
        }
        .filter_map(use_stmt);
        let mut res = ImportGranularityGuess::Unknown;
        let (mut prev, mut prev_vis, mut prev_attrs) = match use_stmts.next() {
            Some(it) => it,
            None => return res,
        };
        loop {
            if let Some(use_tree_list) = prev.use_tree_list() {
                if use_tree_list.use_trees().any(|tree| tree.use_tree_list().is_some()) {
                    // Nested tree lists can only occur in crate style, or with no proper style being enforced in the file.
                    break ImportGranularityGuess::Crate;
                } else {
                    // Could still be crate-style so continue looking.
                    res = ImportGranularityGuess::CrateOrModule;
                }
            }

            let (curr, curr_vis, curr_attrs) = match use_stmts.next() {
                Some(it) => it,
                None => break res,
            };
            if eq_visibility(prev_vis, curr_vis.clone()) && eq_attrs(prev_attrs, curr_attrs.clone())
            {
                if let Some((prev_path, curr_path)) = prev.path().zip(curr.path()) {
                    if let Some((prev_prefix, _)) = common_prefix(&prev_path, &curr_path) {
                        if prev.use_tree_list().is_none() && curr.use_tree_list().is_none() {
                            let prefix_c = prev_prefix.qualifiers().count();
                            let curr_c = curr_path.qualifiers().count() - prefix_c;
                            let prev_c = prev_path.qualifiers().count() - prefix_c;
                            if curr_c == 1 && prev_c == 1 {
                                // Same prefix, only differing in the last segment and no use tree lists so this has to be of item style.
                                break ImportGranularityGuess::Item;
                            } else {
                                // Same prefix and no use tree list but differs in more than one segment at the end. This might be module style still.
                                res = ImportGranularityGuess::ModuleOrItem;
                            }
                        } else {
                            // Same prefix with item tree lists, has to be module style as it
                            // can't be crate style since the trees wouldn't share a prefix then.
                            break ImportGranularityGuess::Module;
                        }
                    }
                }
            }
            prev = curr;
            prev_vis = curr_vis;
            prev_attrs = curr_attrs;
        }
    }
}

#[derive(PartialEq, PartialOrd, Debug, Clone, Copy)]
enum ImportGranularityGuess {
    Unknown,
    Item,
    Module,
    ModuleOrItem,
    Crate,
    CrateOrModule,
}

/// Insert an import path into the given file/node. A `merge` value of none indicates that no import merging is allowed to occur.
pub fn insert_use<'a>(scope: &ImportScope, path: ast::Path, cfg: &InsertUseConfig) {
    let _p = profile::span("insert_use");
    let mut mb = match cfg.granularity {
        ImportGranularity::Crate => Some(MergeBehavior::Crate),
        ImportGranularity::Module => Some(MergeBehavior::Module),
        ImportGranularity::Item | ImportGranularity::Preserve => None,
    };
    if !cfg.enforce_granularity {
        let file_granularity = scope.guess_granularity_from_scope();
        mb = match file_granularity {
            ImportGranularityGuess::Unknown => mb,
            ImportGranularityGuess::Item => None,
            ImportGranularityGuess::Module => Some(MergeBehavior::Module),
            ImportGranularityGuess::ModuleOrItem => mb.and(Some(MergeBehavior::Module)),
            ImportGranularityGuess::Crate => Some(MergeBehavior::Crate),
            ImportGranularityGuess::CrateOrModule => mb.or(Some(MergeBehavior::Crate)),
        };
    }

    let use_item =
        make::use_(None, make::use_tree(path.clone(), None, None, false)).clone_for_update();
    // merge into existing imports if possible
    if let Some(mb) = mb {
        let filter = |it: &_| !(cfg.skip_glob_imports && ast::Use::is_simple_glob(it));
        for existing_use in
            scope.as_syntax_node().children().filter_map(ast::Use::cast).filter(filter)
        {
            if let Some(merged) = try_merge_imports(&existing_use, &use_item, mb) {
                ted::replace(existing_use.syntax(), merged.syntax());
                return;
            }
        }
    }

    // either we weren't allowed to merge or there is no import that fits the merge conditions
    // so look for the place we have to insert to
    insert_use_(scope, path, cfg.group, use_item);
}

#[derive(Eq, PartialEq, PartialOrd, Ord)]
enum ImportGroup {
    // the order here defines the order of new group inserts
    Std,
    ExternCrate,
    ThisCrate,
    ThisModule,
    SuperModule,
}

impl ImportGroup {
    fn new(path: &ast::Path) -> ImportGroup {
        let default = ImportGroup::ExternCrate;

        let first_segment = match path.first_segment() {
            Some(it) => it,
            None => return default,
        };

        let kind = first_segment.kind().unwrap_or(PathSegmentKind::SelfKw);
        match kind {
            PathSegmentKind::SelfKw => ImportGroup::ThisModule,
            PathSegmentKind::SuperKw => ImportGroup::SuperModule,
            PathSegmentKind::CrateKw => ImportGroup::ThisCrate,
            PathSegmentKind::Name(name) => match name.text().as_str() {
                "std" => ImportGroup::Std,
                "core" => ImportGroup::Std,
                _ => ImportGroup::ExternCrate,
            },
            PathSegmentKind::Type { .. } => unreachable!(),
        }
    }
}

fn insert_use_(
    scope: &ImportScope,
    insert_path: ast::Path,
    group_imports: bool,
    use_item: ast::Use,
) {
    let scope_syntax = scope.as_syntax_node();
    let group = ImportGroup::new(&insert_path);
    let path_node_iter = scope_syntax
        .children()
        .filter_map(|node| ast::Use::cast(node.clone()).zip(Some(node)))
        .flat_map(|(use_, node)| {
            let tree = use_.use_tree()?;
            let path = tree.path()?;
            let has_tl = tree.use_tree_list().is_some();
            Some((path, has_tl, node))
        });

    if !group_imports {
        if let Some((_, _, node)) = path_node_iter.last() {
            cov_mark::hit!(insert_no_grouping_last);
            ted::insert(ted::Position::after(node), use_item.syntax());
        } else {
            cov_mark::hit!(insert_no_grouping_last2);
            ted::insert(ted::Position::first_child_of(scope_syntax), make::tokens::blank_line());
            ted::insert(ted::Position::first_child_of(scope_syntax), use_item.syntax());
        }
        return;
    }

    // Iterator that discards anything thats not in the required grouping
    // This implementation allows the user to rearrange their import groups as this only takes the first group that fits
    let group_iter = path_node_iter
        .clone()
        .skip_while(|(path, ..)| ImportGroup::new(path) != group)
        .take_while(|(path, ..)| ImportGroup::new(path) == group);

    // track the last element we iterated over, if this is still None after the iteration then that means we never iterated in the first place
    let mut last = None;
    // find the element that would come directly after our new import
    let post_insert: Option<(_, _, SyntaxNode)> = group_iter
        .inspect(|(.., node)| last = Some(node.clone()))
        .find(|&(ref path, has_tl, _)| {
            use_tree_path_cmp(&insert_path, false, path, has_tl) != Ordering::Greater
        });

    if let Some((.., node)) = post_insert {
        cov_mark::hit!(insert_group);
        // insert our import before that element
        return ted::insert(ted::Position::before(node), use_item.syntax());
    }
    if let Some(node) = last {
        cov_mark::hit!(insert_group_last);
        // there is no element after our new import, so append it to the end of the group
        return ted::insert(ted::Position::after(node), use_item.syntax());
    }

    // the group we were looking for actually doesn't exist, so insert

    let mut last = None;
    // find the group that comes after where we want to insert
    let post_group = path_node_iter
        .inspect(|(.., node)| last = Some(node.clone()))
        .find(|(p, ..)| ImportGroup::new(p) > group);
    if let Some((.., node)) = post_group {
        cov_mark::hit!(insert_group_new_group);
        ted::insert(ted::Position::before(&node), use_item.syntax());
        if let Some(node) = algo::non_trivia_sibling(node.into(), Direction::Prev) {
            ted::insert(ted::Position::after(node), make::tokens::single_newline());
        }
        return;
    }
    // there is no such group, so append after the last one
    if let Some(node) = last {
        cov_mark::hit!(insert_group_no_group);
        ted::insert(ted::Position::after(&node), use_item.syntax());
        ted::insert(ted::Position::after(node), make::tokens::single_newline());
        return;
    }
    // there are no imports in this file at all
    if let Some(last_inner_element) = scope_syntax
        .children_with_tokens()
        .filter(|child| match child {
            NodeOrToken::Node(node) => is_inner_attribute(node.clone()),
            NodeOrToken::Token(token) => is_inner_comment(token.clone()),
        })
        .last()
    {
        cov_mark::hit!(insert_group_empty_inner_attr);
        ted::insert(ted::Position::after(&last_inner_element), use_item.syntax());
        ted::insert(ted::Position::after(last_inner_element), make::tokens::single_newline());
        return;
    }
    let l_curly = match scope {
        ImportScope::File(_) => {
            cov_mark::hit!(insert_group_empty_file);
            ted::insert(ted::Position::first_child_of(scope_syntax), make::tokens::blank_line());
            ted::insert(ted::Position::first_child_of(scope_syntax), use_item.syntax());
            return;
        }
        // don't insert the imports before the item list/block expr's opening curly brace
        ImportScope::Module(item_list) => item_list.l_curly_token(),
        // don't insert the imports before the item list's opening curly brace
        ImportScope::Block(block) => block.l_curly_token(),
    };
    match l_curly {
        Some(b) => {
            cov_mark::hit!(insert_group_empty_module);
            ted::insert(ted::Position::after(&b), make::tokens::single_newline());
            ted::insert(ted::Position::after(&b), use_item.syntax());
        }
        None => {
            // This should never happens, broken module syntax node
            ted::insert(ted::Position::first_child_of(scope_syntax), make::tokens::blank_line());
            ted::insert(ted::Position::first_child_of(scope_syntax), use_item.syntax());
        }
    }
}

fn is_inner_attribute(node: SyntaxNode) -> bool {
    ast::Attr::cast(node).map(|attr| attr.kind()) == Some(ast::AttrKind::Inner)
}

fn is_inner_comment(token: SyntaxToken) -> bool {
    ast::Comment::cast(token).and_then(|comment| comment.kind().doc)
        == Some(ast::CommentPlacement::Inner)
}
#[cfg(test)]
mod tests;
