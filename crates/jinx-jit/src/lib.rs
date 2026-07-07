//! Cranelift JIT tier for jinx (M7): compiles hot `Chunk`s to native code
//! that shares the interpreter's frame/operand-stack layout, so an
//! uncompilable chunk (any op we don't lower) transparently falls back to
//! `VM::run_top_frame`.

mod codegen;
mod rt;

pub use codegen::Compiler;

use jinx_eval::jit::JitHook;

/// Construct the Cranelift backend for installation into a `VM`.
pub fn new_compiler() -> Box<dyn JitHook> {
    Box::new(Compiler::new())
}

/// perf-jit experiment: spawn a background compile worker owning its own
/// Cranelift module. Receives `&'static CodeRef` addresses (as usize, since
/// raw-pointer-bearing types aren't Send) and publishes compiled entry
/// points into `chunk.jit.entry` with Release ordering; the eval thread
/// keeps interpreting until it observes the entry (Acquire).
pub fn spawn_bg_compiler() -> std::sync::mpsc::Sender<usize> {
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    std::thread::spawn(move || {
        let mut c = Compiler::new();
        while let Ok(addr) = rx.recv() {
            // SAFETY: addresses are leaked immortal CodeRefs sent by the VM.
            let code: &'static jinx_eval::chunk::CodeRef =
                unsafe { &*(addr as *const jinx_eval::chunk::CodeRef) };
            let chunk = code.chunk();
            let entry = match JitHook::compile(&mut c, code) {
                Some(p) => p as *mut (),
                None => jinx_eval::chunk::JIT_UNCOMPILABLE,
            };
            chunk
                .jit
                .entry
                .store(entry, std::sync::atomic::Ordering::Release);
        }
    });
    tx
}

#[cfg(test)]
mod smoke {
    use cranelift_codegen::ir::{types, AbiParam, InstBuilder};
    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::{Linkage, Module};

    #[test]
    fn jit_add_on_this_machine() {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").unwrap();
        let isa = cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
            .unwrap()
            .finish(settings::Flags::new(flags))
            .unwrap();
        let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        let mut module = JITModule::new(builder);

        let mut ctx = module.make_context();
        ctx.func.signature.params.push(AbiParam::new(types::I64));
        ctx.func.signature.params.push(AbiParam::new(types::I64));
        ctx.func.signature.returns.push(AbiParam::new(types::I64));

        let mut fbc = FunctionBuilderContext::new();
        let mut fb = FunctionBuilder::new(&mut ctx.func, &mut fbc);
        let block = fb.create_block();
        fb.append_block_params_for_function_params(block);
        fb.switch_to_block(block);
        let (a, b) = (fb.block_params(block)[0], fb.block_params(block)[1]);
        let sum = fb.ins().iadd(a, b);
        fb.ins().return_(&[sum]);
        fb.seal_block(block);
        fb.finalize();

        let id = module
            .declare_function("add", Linkage::Export, &ctx.func.signature)
            .unwrap();
        module.define_function(id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);
        module.finalize_definitions().unwrap();

        let code = module.get_finalized_function(id);
        let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(code) };
        assert_eq!(f(40, 2), 42);
        assert_eq!(f(i64::MAX, 1), i64::MIN); // wrapping in hardware
    }
}
