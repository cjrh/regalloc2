//! Compiles each function in the given CLIF files with the files' own
//! `set` and `target` lines and prints its disassembly, like
//! `clif-util compile -D`.
use cranelift_codegen::Context;
use cranelift_control::ControlPlane;
use cranelift_reader::{parse_test, IsaSpec, ParseOptions};

fn main() {
    for path in std::env::args().skip(1) {
        let text = std::fs::read_to_string(&path).unwrap();
        let test = parse_test(&text, ParseOptions::default()).unwrap();
        let isa = match &test.isa_spec {
            IsaSpec::Some(isas) => isas[0].clone(),
            IsaSpec::None(_) => panic!("{path}: needs a `target` line"),
        };
        for (function, _) in test.functions {
            let name = function.name.clone();
            let mut context = Context::for_function(function);
            let code = context
                .compile(&*isa, &mut ControlPlane::default())
                .unwrap_or_else(|e| panic!("{path}: {:?}", e.inner));
            println!("==== {name}");
            let capstone = isa.to_capstone().unwrap();
            for insn in capstone.disasm_all(code.code_buffer(), 0).unwrap().iter() {
                println!(
                    "{:6x}:  {} {}",
                    insn.address(),
                    insn.mnemonic().unwrap_or(""),
                    insn.op_str().unwrap_or("")
                );
            }
        }
    }
}
