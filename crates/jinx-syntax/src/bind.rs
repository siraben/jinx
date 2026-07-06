//! Variable binding (`bindVars` from nixexpr.cc), reduced to what `--parse`
//! needs: detecting undefined variables against the static base environment,
//! in the same traversal order as the C++ implementation.

use rustc_hash::FxHashSet;

use crate::ast::*;
use crate::error::ParseError;
use crate::symbol::{Symbol, SymbolTable};

/// Global names in `staticBaseEnv` (constants + primops from
/// libexpr/eval.cc and primops*.cc registration).
const GLOBALS: &[&str] = &[
    "builtins",
    "true",
    "false",
    "null",
    "derivation",
    "__currentSystem",
    "__currentTime",
    "__nixVersion",
    "__langVersion",
    "__storeDir",
    "__nixPath",
    // unprefixed primops
    "abort",
    "baseNameOf",
    "break",
    "derivationStrict",
    "dirOf",
    "fetchFinalTree",
    "fetchGit",
    "fetchMercurial",
    "fetchTarball",
    "fetchTree",
    "fromTOML",
    "import",
    "isNull",
    "map",
    "placeholder",
    "removeAttrs",
    "scopedImport",
    "throw",
    "toString",
    // "__"-prefixed primops
    "__add",
    "__addDrvOutputDependencies",
    "__addErrorContext",
    "__all",
    "__any",
    "__appendContext",
    "__attrNames",
    "__attrValues",
    "__bitAnd",
    "__bitOr",
    "__bitXor",
    "__catAttrs",
    "__ceil",
    "__compareVersions",
    "__concatLists",
    "__concatMap",
    "__concatStringsSep",
    "__convertHash",
    "__deepSeq",
    "__div",
    "__elem",
    "__elemAt",
    "__exec",
    "__fetchClosure",
    "__fetchurl",
    "__filter",
    "__filterSource",
    "__findFile",
    "__floor",
    "__forceLazyFetcherAttr",
    "__fromJSON",
    "__functionArgs",
    "__genList",
    "__genericClosure",
    "__getAttr",
    "__getContext",
    "__getEnv",
    "__groupBy",
    "__hasAttr",
    "__hasContext",
    "__hashFile",
    "__hashString",
    "__head",
    "__importNative",
    "__intersectAttrs",
    "__isAttrs",
    "__isBool",
    "__isFloat",
    "__isFunction",
    "__isInt",
    "__isList",
    "__isPath",
    "__isString",
    "__length",
    "__lessThan",
    "__listToAttrs",
    "__mapAttrs",
    "__match",
    "__mul",
    "__outputOf",
    "__parseDrvName",
    "__partition",
    "__path",
    "__pathExists",
    "__readDir",
    "__readFile",
    "__readFileType",
    "__replaceStrings",
    "__seq",
    "__sort",
    "__split",
    "__splitVersion",
    "__storePath",
    "__stringLength",
    "__sub",
    "__substring",
    "__tail",
    "__toFile",
    "__toJSON",
    "__toPath",
    "__toXML",
    "__trace",
    "__traceVerbose",
    "__tryEval",
    "__typeOf",
    "__unsafeDiscardOutputDependency",
    "__unsafeDiscardStringContext",
    "__unsafeGetAttrPos",
    "__warn",
    "__zipAttrsWith",
];

struct Env<'p> {
    up: Option<&'p Env<'p>>,
    is_with: bool,
    vars: FxHashSet<Symbol>,
}

impl<'p> Env<'p> {
    fn lookup(&self, name: Symbol) -> (bool, bool) {
        // returns (found, saw_with)
        let mut cur = Some(self);
        let mut saw_with = false;
        while let Some(env) = cur {
            if env.is_with {
                saw_with = true;
            } else if env.vars.contains(&name) {
                return (true, saw_with);
            }
            cur = env.up;
        }
        (false, saw_with)
    }
}

pub fn bind_vars(exprs: &Exprs, root: ExprId, symbols: &mut SymbolTable) -> Result<(), ParseError> {
    let mut base_vars = FxHashSet::default();
    for g in GLOBALS {
        base_vars.insert(symbols.create(g.as_bytes()));
    }
    let base = Env {
        up: None,
        is_with: false,
        vars: base_vars,
    };
    bind(exprs, symbols, root, &base)
}

fn bind(
    exprs: &Exprs,
    symbols: &SymbolTable,
    id: ExprId,
    env: &Env<'_>,
) -> Result<(), ParseError> {
    match exprs.get(id) {
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::String(_)
        | Expr::Path(_)
        | Expr::CurPos(_)
        | Expr::InheritFrom { .. } => Ok(()),
        Expr::Var { pos, name } => {
            let (found, saw_with) = env.lookup(*name);
            if !found && !saw_with {
                let n = crate::show::print_identifier_str(symbols.resolve(*name));
                let mut msg: Vec<u8> = b"undefined variable '".to_vec();
                msg.extend_from_slice(&n);
                msg.push(b'\'');
                return Err(ParseError::new(msg, *pos));
            }
            Ok(())
        }
        Expr::Select {
            e, attrpath, def, ..
        } => {
            bind(exprs, symbols, *e, env)?;
            if let Some(def) = def {
                bind(exprs, symbols, *def, env)?;
            }
            for an in attrpath {
                if let Some(de) = an.expr {
                    bind(exprs, symbols, de, env)?;
                }
            }
            Ok(())
        }
        Expr::OpHasAttr { e, attrpath } => {
            bind(exprs, symbols, *e, env)?;
            for an in attrpath {
                if let Some(de) = an.expr {
                    bind(exprs, symbols, de, env)?;
                }
            }
            Ok(())
        }
        Expr::Attrs(a) => bind_attrs(exprs, symbols, a, env, a.recursive),
        Expr::List(elems) => {
            for e in elems {
                bind(exprs, symbols, *e, env)?;
            }
            Ok(())
        }
        Expr::Lambda(l) => {
            let mut vars = FxHashSet::default();
            if l.arg.is_set() {
                vars.insert(l.arg);
            }
            if let Some(formals) = &l.formals {
                for f in &formals.formals {
                    vars.insert(f.name);
                }
            }
            let new_env = Env {
                up: Some(env),
                is_with: false,
                vars,
            };
            if let Some(formals) = &l.formals {
                for f in &formals.formals {
                    if let Some(def) = f.def {
                        bind(exprs, symbols, def, &new_env)?;
                    }
                }
            }
            bind(exprs, symbols, l.body, &new_env)
        }
        Expr::Call { fun, args, .. } => {
            bind(exprs, symbols, *fun, env)?;
            for a in args {
                bind(exprs, symbols, *a, env)?;
            }
            Ok(())
        }
        Expr::Let { attrs, body } => {
            let a = exprs.attrs(*attrs);
            let vars: FxHashSet<Symbol> = a.attrs.keys().copied().collect();
            let new_env = Env {
                up: Some(env),
                is_with: false,
                vars,
            };
            for from in &a.inherit_from_exprs {
                bind(exprs, symbols, *from, &new_env)?;
            }
            for def in a.attrs.values() {
                match def.kind {
                    AttrDefKind::Inherited => bind(exprs, symbols, def.e, env)?,
                    _ => bind(exprs, symbols, def.e, &new_env)?,
                }
            }
            bind(exprs, symbols, *body, &new_env)
        }
        Expr::With { attrs, body, .. } => {
            bind(exprs, symbols, *attrs, env)?;
            let new_env = Env {
                up: Some(env),
                is_with: true,
                vars: FxHashSet::default(),
            };
            bind(exprs, symbols, *body, &new_env)
        }
        Expr::If {
            cond, then, else_, ..
        } => {
            bind(exprs, symbols, *cond, env)?;
            bind(exprs, symbols, *then, env)?;
            bind(exprs, symbols, *else_, env)
        }
        Expr::Assert { cond, body, .. } => {
            bind(exprs, symbols, *cond, env)?;
            bind(exprs, symbols, *body, env)
        }
        Expr::OpNot(e) => bind(exprs, symbols, *e, env),
        Expr::OpEq(a, b)
        | Expr::OpNEq(a, b)
        | Expr::OpAnd(_, a, b)
        | Expr::OpOr(_, a, b)
        | Expr::OpImpl(_, a, b)
        | Expr::OpUpdate(_, a, b)
        | Expr::OpConcatLists(_, a, b) => {
            bind(exprs, symbols, *a, env)?;
            bind(exprs, symbols, *b, env)
        }
        Expr::ConcatStrings { es, .. } => {
            for (_, e) in es {
                bind(exprs, symbols, *e, env)?;
            }
            Ok(())
        }
    }
}

fn bind_attrs(
    exprs: &Exprs,
    symbols: &SymbolTable,
    a: &ExprAttrs,
    env: &Env<'_>,
    recursive: bool,
) -> Result<(), ParseError> {
    if recursive {
        let vars: FxHashSet<Symbol> = a.attrs.keys().copied().collect();
        let new_env = Env {
            up: Some(env),
            is_with: false,
            vars,
        };
        for from in &a.inherit_from_exprs {
            bind(exprs, symbols, *from, &new_env)?;
        }
        for def in a.attrs.values() {
            match def.kind {
                AttrDefKind::Plain => bind(exprs, symbols, def.e, &new_env)?,
                AttrDefKind::Inherited => bind(exprs, symbols, def.e, env)?,
                AttrDefKind::InheritedFrom => bind(exprs, symbols, def.e, &new_env)?,
            }
        }
        for d in &a.dynamic_attrs {
            bind(exprs, symbols, d.name_expr, &new_env)?;
            bind(exprs, symbols, d.value_expr, &new_env)?;
        }
    } else {
        for from in &a.inherit_from_exprs {
            bind(exprs, symbols, *from, env)?;
        }
        for def in a.attrs.values() {
            bind(exprs, symbols, def.e, env)?;
        }
        for d in &a.dynamic_attrs {
            bind(exprs, symbols, d.name_expr, env)?;
            bind(exprs, symbols, d.value_expr, env)?;
        }
    }
    Ok(())
}
