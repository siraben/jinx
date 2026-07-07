//! Registration table mapping runtime-helper names to their addresses. The
//! helpers themselves live in `jinx_eval::jit_rt` (they need the VM's private
//! op methods); here we only expose their addresses to the `JITBuilder` and
//! their CLIF signatures to the code generator.

use cranelift_codegen::ir::{types, Type};
use jinx_eval::jit_rt as h;

/// (symbol name, address).
pub type Sym = (&'static str, *const u8);

macro_rules! table {
    ($( $name:ident ( $($ty:expr),* ) ),* $(,)?) => {
        /// All runtime helper symbols to register with the `JITBuilder`.
        pub fn symbols() -> Vec<Sym> {
            vec![ $( (stringify!($name), h::$name as *const u8) ),* ]
        }
        /// CLIF parameter types for each helper (all return `I64`).
        pub fn signatures() -> Vec<(&'static str, Vec<Type>)> {
            vec![ $( (stringify!($name), vec![ $($ty),* ]) ),* ]
        }
    };
}

const P: Type = types::I64; // pointer / u64 / usize
const W: Type = types::I32; // u32 operand

table! {
    jinx_setup(P, P, P),
    jinx_base(P, P),
    jinx_upvals(P, P),
    jinx_force_top(P, W),
    jinx_force_bool_top(P, W, W),
    jinx_force_attrs_top(P, W, W),
    jinx_force_list_top(P, W, W),
    jinx_resolve_with(P, P, W, W),
    jinx_alloc_cell(P),
    jinx_alloc_int(P, P),
    jinx_alloc_bool(P, W),
    jinx_store_local(P, P, W),
    jinx_make_thunk(P, P, W, W),
    jinx_make_list(P, W),
    jinx_make_attrs(P, P, W),
    jinx_dyn_attr(P, W),
    jinx_rec_overrides(P, P, W, W),
    jinx_eq(P, W, W),
    jinx_not(P),
    jinx_update(P),
    jinx_concat_lists(P),
    jinx_concat_strings(P, P, W),
    jinx_select(P, W, W),
    jinx_select_force(P, P, W),
    jinx_select_or(P, W, W),
    jinx_select_dyn(P, P, W, W),
    jinx_select_dyn_or(P, W),
    jinx_has_attr_path(P, P, W, W),
    jinx_call(P, W, W),
    jinx_cur_pos(P, W),
    jinx_assert_fail(P, P, W, W),
    jinx_assert_eq(P, P, W, W),
    jinx_push_with(P, P),
    jinx_pop_with(P, P),
}
