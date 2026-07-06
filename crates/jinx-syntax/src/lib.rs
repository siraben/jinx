//! Nix language syntax: lexer, parser, AST and `Expr::show`, ported 1:1 from
//! the C++ implementation in nix/src/libexpr (lexer.l, parser.y,
//! parser-state.hh, nixexpr.{hh,cc}).

pub mod ast;
pub mod bind;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod pos;
pub mod show;
pub mod symbol;

pub use ast::{AttrDef, AttrDefKind, AttrName, Expr, ExprId, Exprs, Formal, Formals};
pub use error::ParseError;
pub use pos::{Origin, Pos, PosIdx, PosTable};
pub use symbol::{Symbol, SymbolTable};

/// Result of parsing a complete Nix expression.
pub struct ParseResult {
    pub exprs: Exprs,
    pub root: ExprId,
    pub symbols: SymbolTable,
}

/// Parse `source` with the given origin, mimicking `EvalState::parse` +
/// `bindVars` against the static base environment (as `nix-instantiate
/// --parse` does, which reports undefined variables at parse time).
/// The caller owns the `PosTable` (for error rendering) and the warnings
/// sink (deprecation warnings emitted during parsing).
pub fn parse_and_bind(
    source: &[u8],
    origin: Origin,
    base_path: &str,
    home: Option<&str>,
    positions: &mut PosTable,
    warnings: &mut Vec<Vec<u8>>,
) -> Result<ParseResult, ParseError> {
    let mut symbols = SymbolTable::new();
    let root = parse_and_bind_with(
        source, origin, base_path, home, positions, &mut symbols, warnings,
    )?;
    Ok(ParseResult {
        exprs: root.0,
        root: root.1,
        symbols,
    })
}

/// Like `parse_and_bind`, but interning into a caller-owned `SymbolTable`
/// so multiple files (e.g. via `import`) share one symbol space.
#[allow(clippy::too_many_arguments)]
pub fn parse_and_bind_with(
    source: &[u8],
    origin: Origin,
    base_path: &str,
    home: Option<&str>,
    positions: &mut PosTable,
    symbols: &mut SymbolTable,
    warnings: &mut Vec<Vec<u8>>,
) -> Result<(Exprs, ExprId), ParseError> {
    let origin_id = positions.add_origin(origin, source.len());
    let mut exprs = Exprs::new();
    let root = parser::parse(
        source,
        origin_id,
        &mut exprs,
        symbols,
        positions,
        base_path,
        home,
        warnings,
    )?;
    bind::bind_vars(&exprs, root, symbols)?;
    Ok((exprs, root))
}

/// `parse_and_bind_with` plus extra global names visible during binding
/// (for `scopedImport`).
#[allow(clippy::too_many_arguments)]
pub fn parse_and_bind_scoped(
    source: &[u8],
    origin: Origin,
    base_path: &str,
    home: Option<&str>,
    positions: &mut PosTable,
    symbols: &mut SymbolTable,
    warnings: &mut Vec<Vec<u8>>,
    extra_globals: &[Symbol],
) -> Result<(Exprs, ExprId), ParseError> {
    let origin_id = positions.add_origin(origin, source.len());
    let mut exprs = Exprs::new();
    let root = parser::parse(
        source,
        origin_id,
        &mut exprs,
        symbols,
        positions,
        base_path,
        home,
        warnings,
    )?;
    bind::bind_vars_extra(&exprs, root, symbols, extra_globals)?;
    Ok((exprs, root))
}
