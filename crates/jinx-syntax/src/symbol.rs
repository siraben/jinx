//! Interned symbols, mirroring C++ `SymbolTable` / `Symbol`.
//!
//! `Symbol` is a u32 id; 0 means "no symbol" (like the default-constructed
//! C++ `Symbol`). Ordering of symbols is creation order, which matters:
//! `ExprAttrs::attrs` is a map ordered by symbol id in C++.

use rustc_hash::FxHashMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Symbol(pub u32);

impl Symbol {
    pub fn is_set(self) -> bool {
        self.0 != 0
    }
}

#[derive(Default)]
pub struct SymbolTable {
    map: FxHashMap<Vec<u8>, Symbol>,
    names: Vec<Vec<u8>>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, s: &[u8]) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }
        self.names.push(s.to_vec());
        let sym = Symbol(self.names.len() as u32);
        self.map.insert(s.to_vec(), sym);
        sym
    }

    pub fn resolve(&self, sym: Symbol) -> &[u8] {
        assert!(sym.is_set());
        &self.names[(sym.0 - 1) as usize]
    }

    /// The symbol's name as an owned, lossily-decoded `String` — the common
    /// path when a name is needed for an error message, label, or JSON key.
    pub fn resolve_str_lossy(&self, sym: Symbol) -> String {
        String::from_utf8_lossy(self.resolve(sym)).into_owned()
    }
}
