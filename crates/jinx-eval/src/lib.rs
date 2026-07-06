//! jinx-eval: value representation, GC'd heap, bytecode compiler, VM, builtins.

pub mod builtins;
pub mod chunk;
pub mod compile;
pub mod context;
pub mod error;
pub mod heap;
pub mod immortal;
pub mod json;
pub mod mem;
pub mod print;
pub mod regex;
pub mod value;
pub mod vm;
pub mod xml;
