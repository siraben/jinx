//! Global u32 symbol interning (attr and variable names). Symbols are
//! immortal for the process lifetime, like C++ Nix's SymbolTable.

use bstr::BString;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Symbol(pub u32);

#[derive(Default)]
pub struct SymbolTable {
    map: FxHashMap<BString, u32>,
    names: Vec<BString>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, name: &[u8]) -> Symbol {
        if let Some(&id) = self.map.get(name) {
            return Symbol(id);
        }
        let id = self.names.len() as u32;
        self.names.push(BString::from(name));
        self.map.insert(BString::from(name), id);
        Symbol(id)
    }

    #[inline]
    pub fn name(&self, sym: Symbol) -> &[u8] {
        &self.names[sym.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}
