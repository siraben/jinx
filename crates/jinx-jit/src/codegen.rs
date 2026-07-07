//! Chunk -> CLIF -> native-code compilation.

use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::Module;

use jinx_eval::chunk::Chunk;
use jinx_eval::jit::JitHook;

/// The Cranelift backend: owns the JIT module (code memory) for the process's
/// lifetime. Single-threaded, matching the evaluator.
pub struct Compiler {
    module: JITModule,
    ctx: cranelift_codegen::Context,
    fbc: cranelift_frontend::FunctionBuilderContext,
    /// Monotonic id for uniquely naming compiled functions.
    next_id: usize,
}

impl Compiler {
    pub fn new() -> Self {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").unwrap();
        // We manage our own trap-free lowering; use the default calling conv.
        flags.set("use_colocated_libcalls", "false").unwrap();
        flags.set("is_pic", "false").unwrap();
        let isa = cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
            .expect("host ISA")
            .finish(settings::Flags::new(flags))
            .expect("ISA finish");

        let mut builder =
            JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        for (name, addr) in crate::rt::symbols() {
            builder.symbol(name, addr);
        }
        let module = JITModule::new(builder);
        let ctx = module.make_context();
        Compiler {
            module,
            ctx,
            fbc: cranelift_frontend::FunctionBuilderContext::new(),
            next_id: 0,
        }
    }
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

impl JitHook for Compiler {
    fn compile(&mut self, chunk: &'static Chunk) -> Option<*const ()> {
        // Step 1: do-nothing backend — nothing is lowered yet, so every chunk
        // is reported uncompilable and the interpreter keeps running it. This
        // exercises the tier-up plumbing (counter, dispatch) with zero risk.
        let _ = chunk;
        let _ = (&mut self.module, &mut self.ctx, &mut self.fbc, &mut self.next_id);
        None
    }
}
