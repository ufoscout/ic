//! This module is responsible for instrumenting wasm binaries on the Internet
//! Computer.
//!
//! It exports the function [`instrument`] which takes a Wasm binary and
//! injects some instrumentation that allows to:
//!  * Quantify the amount of execution every function of that module conducts.
//!    This quantity is approximated by the sum of cost of instructions executed
//!    on the taken execution path.
//!  * Verify that no successful `memory.grow` results in exceeding the
//!    available memory allocated to the canister.
//!
//! Moreover, it exports the function referred to by the `start` section under
//! the name `canister_start` and removes the section. (This is needed so that
//! we can run the initialization after we have set the instructions counter to
//! some value).
//!
//! After instrumentation any function of that module will only be able to
//! execute as long as at every reentrant basic block of its execution path, the
//! counter is verified to be above zero. Otherwise, the function will trap (via
//! calling a special system API call). If the function returns before the
//! counter overflows, the value of the counter is the initial value minus the
//! sum of cost of all executed instructions.
//!
//! In more details, first, it inserts two System API functions:
//!
//! ```wasm
//! (import "__" "out_of_instructions" (func (;0;) (func)))
//! (import "__" "update_available_memory" (func (;1;) ((param i32 i32) (result i32))))
//! ```
//!
//! It then inserts (and exports) a global mutable counter:
//! ```wasm
//! (global (;0;) (mut i64) (i64.const 0))
//! (export "canister counter_instructions" (global 0)))
//! ```
//!
//! An additional function is also inserted to handle updates to the instruction
//! counter for bulk memory instructions whose cost can only be determined at
//! runtime:
//!
//! ```wasm
//! (func (;5;) (type 4) (param i32) (result i32)
//!   global.get 0
//!   local.get 0
//!   i64.extend_i32_u
//!   i64.sub
//!   global.set 0
//!   global.get 0
//!   i64.const 0
//!   i64.lt_s
//!   if  ;; label = @1
//!     call 0           # the `out_of_instructions` function
//!   end
//!   local.get 0)
//! ```
//!
//! The `counter_instructions` global should be set before the execution of
//! canister code. After execution the global can be read to determine the
//! number of instructions used.
//!
//! Moreover, it injects a decrementation of the instructions counter (by the
//! sum of cost of all instructions inside this block) at the beginning of every
//! non-reentrant block:
//!
//! ```wasm
//! global.get 0
//! i64.const 2
//! i64.sub
//! global.set 0
//! ```
//!
//! and a decrementation with a counter overflow check at the beginning of every
//! reentrant block (a function or a loop body):
//!
//! ```wasm
//! global.get 0
//! i64.const 8
//! i64.sub
//! global.set 0
//! global.get 0
//! i64.const 0
//! i64.lt_s
//! if  ;; label = @1
//!   (call x)
//! end
//! ```
//!
//! Before every bulk memory operation, a call is made to the function which
//! will decrement the instruction counter by the "size" argument of the bulk
//! memory instruction.
//!
//! Note that we omit checking for the counter overflow at the non-reentrant
//! blocks to optimize for performance. The maximal overflow in that case is
//! bound by the length of the longest execution path consisting of
//! non-reentrant basic blocks.

use super::{
    errors::into_parity_wasm_error, wasm_module_builder::WasmModuleBuilder, InstrumentationOutput,
    Segments,
};
use ic_replicated_state::NumWasmPages;
use ic_types::methods::WasmMethod;
use ic_types::NumInstructions;
use ic_wasm_types::{BinaryEncodedWasm, WasmInstrumentationError};

use parity_wasm::builder;
use parity_wasm::elements::{
    BlockType, BulkInstruction, ExportEntry, FuncBody, FunctionType, GlobalEntry, GlobalType,
    InitExpr, Instruction, Instructions, Internal, Local, Module, Section, Type, ValueType,
};
use std::convert::TryFrom;

// The indicies of injected functions.
enum InjectedImports {
    OutOfInstructionsFn = 0,
    UpdateAvailableMemoryFn = 1,
    Count = 2,
}

// Gets the cost of an instruction.
fn instruction_to_cost(i: &Instruction) -> u64 {
    match i {
        // The following instructions are mostly signaling the start/end of code blocks,
        // so we assign 0 cost to them.
        Instruction::Block(_bt) => 0,
        Instruction::Else => 0,
        Instruction::End => 0,
        Instruction::Loop(_bt) => 0,

        // Default cost of an instruction is 1.
        _ => 1,
    }
}

// Injects two system api functions:
//   * `out_of_instructions` which is called, whenever a message execution runs
//     out of instructions.
//   * `update_available_memory` which is called after a native `memory.grow` to
//     check whether the canister has enough available memory according to its
//     memory allocation.
//
// Note that these functions are injected as the first two imports, so that we
// can increment all function indices unconditionally by two. (If they would be
// added as the last two imports, we'd need to increment only non imported
// functions, since imported functions precede all others in the function index
// space, but this would be error-prone).
fn inject_helper_functions(module: Module) -> Module {
    let mut builder = builder::from_module(module);
    let import_sig = builder.push_signature(builder::signature().build_sig());

    builder.push_import(
        builder::import()
            .module("__")
            .field("out_of_instructions")
            .external()
            .func(import_sig)
            .build(),
    );

    let import_sig = builder.push_signature(
        builder::signature()
            .with_param(ValueType::I32)
            .with_param(ValueType::I32)
            .with_result(ValueType::I32)
            .build_sig(),
    );
    builder.push_import(
        builder::import()
            .module("__")
            .field("update_available_memory")
            .external()
            .func(import_sig)
            .build(),
    );

    let mut module = builder.build();
    // We know, we have at least two imports, because we pushed them above, now
    // let's move them to the first two positions respectively, so that we can
    // increase all other function indices unconditionally.
    let entries = module.import_section_mut().unwrap().entries_mut();
    let last = entries.pop().unwrap();
    debug_assert!(last.module() == "__" && last.field() == "update_available_memory");
    entries.insert(0, last);
    let last = entries.pop().unwrap();
    debug_assert!(last.module() == "__" && last.field() == "out_of_instructions");
    entries.insert(0, last);

    debug_assert!(
        entries[InjectedImports::OutOfInstructionsFn as usize].field() == "out_of_instructions"
    );
    debug_assert!(
        entries[InjectedImports::UpdateAvailableMemoryFn as usize].field()
            == "update_available_memory"
    );

    // We lift all call references by 2
    for section in module.sections_mut() {
        match section {
            Section::Code(ref mut code_section) => {
                for func_body in code_section.bodies_mut() {
                    let code = func_body.code_mut();
                    code.elements_mut().iter_mut().for_each(|instr| {
                        if let Instruction::Call(ref mut call_index) = instr {
                            *call_index += InjectedImports::Count as u32;
                        }
                    });
                }
            }
            Section::Export(ref mut export_section) => {
                for export in export_section.entries_mut() {
                    if let Internal::Function(ref mut func_index) = export.internal_mut() {
                        *func_index += InjectedImports::Count as u32;
                    }
                }
            }
            Section::Element(ref mut elements_section) => {
                for segment in elements_section.entries_mut() {
                    for func_index in segment.members_mut() {
                        *func_index += InjectedImports::Count as u32;
                    }
                }
            }
            Section::Start(ref mut func_index) => *func_index += InjectedImports::Count as u32,
            _ => {}
        }
    }
    module
}

#[derive(Default)]
pub struct ExportModuleData {
    pub instructions_counter_ix: u32,
    pub decr_instruction_counter_fn: u32,
    pub start_fn_ix: Option<u32>,
}

/// Takes a Wasm binary and inserts the instructions metering and memory grow
/// instrumentation.
///
/// Returns an [`InstrumentationOutput`] or an error if the input binary could
/// not be instrumented.
pub(super) fn instrument(
    wasm: &BinaryEncodedWasm,
    cost_to_compile_wasm_instruction: NumInstructions,
) -> Result<InstrumentationOutput, WasmInstrumentationError> {
    let module = parity_wasm::deserialize_buffer::<Module>(wasm.as_slice()).map_err(|err| {
        WasmInstrumentationError::ParityDeserializeError(into_parity_wasm_error(err))
    })?;
    let mut module = inject_helper_functions(module);
    module = export_table(module);
    module = export_memory(module);
    module = export_mutable_globals(module);
    let num_functions = module.functions_space() as u32;
    let num_globals = module.globals_space() as u32;

    let export_module_data = ExportModuleData {
        instructions_counter_ix: num_globals,
        decr_instruction_counter_fn: num_functions,
        start_fn_ix: module.start_section(),
    };

    if export_module_data.start_fn_ix.is_some() {
        module.clear_start_section();
    }

    // inject instructions counter decrementation
    {
        if let Some(code_section) = module.code_section_mut() {
            for func_body in code_section.bodies_mut().iter_mut() {
                let code = func_body.code_mut();
                inject_metering(code, &export_module_data);
            }
        }
    }

    {
        // Collect all the function types of the locally defined functions inside the
        // module.
        //
        // The main reason to create this vector of function types is because we can't
        // mix a mutable (to inject instructions) and immutable (to look up the function
        // type) reference to the `code_section`.
        let mut func_types = Vec::new();
        if let Some(code_section) = module.code_section() {
            let functions = module.function_section().unwrap().entries();
            let types = module.type_section().unwrap().types();
            for i in 0..code_section.bodies().len() {
                let Type::Function(t) = &types[functions[i].type_ref() as usize];
                func_types.push(t.clone());
            }
        }
        // Inject `update_available_memory` to functions with `memory.grow`
        // instructions.
        if !func_types.is_empty() {
            let func_bodies = module.code_section_mut().unwrap().bodies_mut();
            for (func_ix, func_type) in func_types.into_iter().enumerate() {
                inject_update_available_memory(&mut func_bodies[func_ix], &func_type);
            }
        }
    }

    let mut module = export_additional_symbols(module, &export_module_data)?;
    let exported_functions = module
        .export_section()
        .unwrap() // because we definitely push exports above
        .entries()
        .iter()
        .filter_map(|export| WasmMethod::try_from(export.field().to_string()).ok())
        .collect();

    let initial_limit = match module.memory_section() {
        // if Wasm does not declare any memory section (mostly tests), use this default
        None => 0,
        Some(section) => {
            let entries = section.entries();
            if entries.len() != 1 {
                return Err(WasmInstrumentationError::IncorrectNumberMemorySections {
                    expected: 1,
                    got: entries.len(),
                });
            }
            let limits = entries[0].limits();
            limits.initial()
        }
    };

    // pull out the data from the data section
    let data = get_data(module.sections_mut());
    data.validate(NumWasmPages::from(initial_limit as usize))?;

    let wasm_instruction_count = (module
        .code_section()
        .map(|code| {
            code.bodies()
                .iter()
                .map(|body| body.code().elements().len())
                .sum()
        })
        .unwrap_or(0)
        + module
            .global_section()
            .map(|globals| {
                globals
                    .entries()
                    .iter()
                    .map(|global| global.init_expr().code().len())
                    .sum()
            })
            .unwrap_or(0)) as u64;

    let result = parity_wasm::serialize(module).map_err(|err| {
        WasmInstrumentationError::ParitySerializeError(into_parity_wasm_error(err))
    })?;
    Ok(InstrumentationOutput {
        exported_functions,
        data,
        binary: BinaryEncodedWasm::new(result),
        compilation_cost: cost_to_compile_wasm_instruction * wasm_instruction_count,
    })
}

// Helper function used by instrumentation to export additional symbols.
//
// Returns the new module or an error if a symbol is not reserved.
#[doc(hidden)] // pub for usage in tests
pub fn export_additional_symbols(
    module: Module,
    export_module_data: &ExportModuleData,
) -> Result<Module, WasmInstrumentationError> {
    let mut mbuilder = WasmModuleBuilder::new(builder::from_module(module));

    // push function to decrement the instruction counter
    mbuilder.push_function(
        builder::function()
            .with_signature(
                builder::signature()
                    .with_param(ValueType::I32) // amount to decrement by
                    .with_result(ValueType::I32) // argument is returned so stack remains unchanged
                    .build_sig(),
            )
            .body()
            .with_instructions(Instructions::new(vec![
                // Subtract the parameter amount from the instruction counter
                Instruction::GetGlobal(export_module_data.instructions_counter_ix),
                Instruction::GetLocal(0),
                Instruction::I64ExtendUI32,
                Instruction::I64Sub,
                Instruction::SetGlobal(export_module_data.instructions_counter_ix),
                // Call out_of_instructions() if `counter < 0`.
                Instruction::GetGlobal(export_module_data.instructions_counter_ix),
                Instruction::I64Const(0),
                Instruction::I64LtS,
                Instruction::If(BlockType::NoResult),
                Instruction::Call(InjectedImports::OutOfInstructionsFn as u32),
                Instruction::End,
                // Return the original param so this function doesn't alter the stack
                Instruction::GetLocal(0),
                Instruction::End,
            ]))
            .build()
            .build(),
    );

    // globals must be exported to be accessible to hypervisor or persisted
    mbuilder.push_export(
        "canister counter_instructions",
        Internal::Global(export_module_data.instructions_counter_ix),
    )?;

    if let Some(ix) = export_module_data.start_fn_ix {
        // push canister_start
        mbuilder.push_export("canister_start", Internal::Function(ix))?;
    }

    // push the instructions counter
    let module = mbuilder
        .with_global(GlobalEntry::new(
            GlobalType::new(ValueType::I64, true),
            InitExpr::new(vec![Instruction::I64Const(0), Instruction::End]),
        ))
        .build();

    Ok(module)
}

// Represents a hint about the context of each static cost injection point in
// wasm.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Scope {
    ReentrantBlockStart,
    NonReentrantBlockStart,
    BlockEnd,
}

// Describes how to calculate the instruction cost at this injection point.
// `StaticCost` injection points contain information about the cost of the
// following basic block. `DynamicCost` injection points assume there is an i32
// on the stack which should be decremented from the instruction counter.
#[derive(Copy, Clone, Debug, PartialEq)]
enum InjectionPointCostDetail {
    StaticCost { scope: Scope, cost: u64 },
    DynamicCost,
}

impl InjectionPointCostDetail {
    /// If the cost is statically known, increment it by the given amount.
    /// Otherwise do nothing.
    fn increment_cost(&mut self, additonal_cost: u64) {
        match self {
            Self::StaticCost { scope: _, cost } => *cost += additonal_cost,
            Self::DynamicCost => {}
        }
    }
}

// Represents a instructions metering injection point.
#[derive(Copy, Clone, Debug)]
struct InjectionPoint {
    cost_detail: InjectionPointCostDetail,
    position: usize,
}

impl InjectionPoint {
    fn new_static_cost(position: usize, scope: Scope) -> Self {
        InjectionPoint {
            cost_detail: InjectionPointCostDetail::StaticCost { scope, cost: 0 },
            position,
        }
    }

    fn new_dynamic_cost(position: usize) -> Self {
        InjectionPoint {
            cost_detail: InjectionPointCostDetail::DynamicCost,
            position,
        }
    }
}

// This function iterates over the injection points, and inserts three different
// pieces of Wasm code:
// - we insert a simple instructions counter decrementation in a beginning of
//   every non-reentrant block
// - we insert a counter decrementation and an overflow check at the beginning
//   of every reentrant block (a loop or a function call).
// - we insert a function call before each dynamic cost instruction which
//   performs an overflow check and then decrements the counter by the value at
//   the top of the stack.
fn inject_metering(code: &mut Instructions, export_data_module: &ExportModuleData) {
    let points = injections(code.elements());
    let points = points.iter().filter(|point| match point.cost_detail {
        InjectionPointCostDetail::StaticCost {
            scope: Scope::ReentrantBlockStart,
            cost: _,
        } => true,
        InjectionPointCostDetail::StaticCost { scope: _, cost } => cost > 0,
        InjectionPointCostDetail::DynamicCost => true,
    });
    let orig_elems = code.elements();
    let mut elems: Vec<Instruction> = Vec::new();
    let mut last_injection_position = 0;
    for point in points {
        elems.extend_from_slice(&orig_elems[last_injection_position..point.position]);
        match point.cost_detail {
            InjectionPointCostDetail::StaticCost { scope, cost } => {
                elems.extend_from_slice(&[
                    Instruction::GetGlobal(export_data_module.instructions_counter_ix),
                    Instruction::I64Const(cost as i64),
                    Instruction::I64Sub,
                    Instruction::SetGlobal(export_data_module.instructions_counter_ix),
                ]);
                if scope == Scope::ReentrantBlockStart {
                    elems.extend_from_slice(&[
                        Instruction::GetGlobal(export_data_module.instructions_counter_ix),
                        Instruction::I64Const(0),
                        Instruction::I64LtS,
                        Instruction::If(BlockType::NoResult),
                        Instruction::Call(InjectedImports::OutOfInstructionsFn as u32),
                        Instruction::End,
                    ]);
                }
            }
            InjectionPointCostDetail::DynamicCost => {
                elems.extend_from_slice(&[Instruction::Call(
                    export_data_module.decr_instruction_counter_fn,
                )]);
            }
        }
        last_injection_position = point.position;
    }
    elems.extend_from_slice(&orig_elems[last_injection_position..]);
    *code.elements_mut() = elems;
}

// Scans through a function and adds instrumentation after each `memory.grow`
// instruction to make sure that there's enough available memory left to support
// the requested extra memory. If no `memory.grow` instructions are present then
// the function's code remains unchanged.
fn inject_update_available_memory(func_body: &mut FuncBody, func_type: &FunctionType) {
    let mut injection_points: Vec<usize> = Vec::new();
    {
        let code = func_body.code();
        for (idx, instr) in code.elements().iter().enumerate() {
            // TODO(EXC-222): Once `table.grow` is supported we should extend the list of
            // injections here.
            if let Instruction::GrowMemory(_) = instr {
                injection_points.push(idx);
            }
        }
    }

    // If we found any injection points, we need to instrument the code.
    if !injection_points.is_empty() {
        // We inject a local to cache the argument to `memory.grow`.
        let n_locals: u32 = func_body.locals().iter().map(Local::count).sum();
        let memory_local_ix = func_type.params().len() as u32 + n_locals;
        func_body.locals_mut().push(Local::new(1, ValueType::I32));
        let code = func_body.code_mut();
        let orig_elems = code.elements_mut();
        let mut elems: Vec<Instruction> = Vec::new();
        let mut last_injection_position = 0;
        for point in injection_points {
            let update_available_memory_instr = orig_elems[point].clone();
            elems.extend_from_slice(&orig_elems[last_injection_position..point]);
            // At this point we have a memory.grow so the argument to it will be on top of
            // the stack, which we just assign to `memory_local_ix` with a local.tee
            // instruction.
            elems.extend_from_slice(&[
                Instruction::TeeLocal(memory_local_ix),
                update_available_memory_instr,
                Instruction::GetLocal(memory_local_ix),
                Instruction::Call(InjectedImports::UpdateAvailableMemoryFn as u32),
            ]);
            last_injection_position = point + 1;
        }
        elems.extend_from_slice(&orig_elems[last_injection_position..]);
        *orig_elems = elems;
    }
}

// This function scans through the Wasm code and creates an injection point
// at the beginning of every basic block (straight-line sequence of instructions
// with no branches) and before each bulk memory instruction. An injection point
// contains a "hint" about the context of every basic block, specifically if
// it's re-entrant or not.
fn injections(code: &[Instruction]) -> Vec<InjectionPoint> {
    let mut res = Vec::new();
    let mut stack = Vec::new();
    use Instruction::*;
    // The function itself is a re-entrant code block.
    let mut curr = InjectionPoint::new_static_cost(0, Scope::ReentrantBlockStart);
    for (position, i) in code.iter().enumerate() {
        curr.cost_detail.increment_cost(instruction_to_cost(i));
        match i {
            // Start of a re-entrant code block.
            Loop(_) => {
                stack.push(curr);
                curr = InjectionPoint::new_static_cost(position + 1, Scope::ReentrantBlockStart);
            }
            // Start of a non re-entrant code block.
            If(_) | Block(_) => {
                stack.push(curr);
                curr = InjectionPoint::new_static_cost(position + 1, Scope::NonReentrantBlockStart);
            }
            // End of a code block but still more code left.
            Else | Br(_) | BrIf(_) | BrTable(_) => {
                res.push(curr);
                curr = InjectionPoint::new_static_cost(position + 1, Scope::BlockEnd);
            }
            // `End` signals the end of a code block. If there's nothing more on the stack, we've
            // gone through all the code.
            End => {
                res.push(curr);
                curr = match stack.pop() {
                    Some(val) => val,
                    None => break,
                };
            }
            // Bulk memory instructions require injected metering __before__ the instruction
            // executes so that size arguments can be read from the stack at runtime.
            Bulk(BulkInstruction::MemoryFill)
            | Bulk(BulkInstruction::MemoryCopy)
            | Bulk(BulkInstruction::MemoryInit(_))
            | Bulk(BulkInstruction::TableCopy)
            | Bulk(BulkInstruction::TableInit(_)) => {
                res.push(InjectionPoint::new_dynamic_cost(position));
            }
            // Nothing special to be done for other instructions.
            _ => (),
        }
    }
    res.sort_by_key(|k| k.position);
    res
}

// Looks for the data section and if it is present, converts it to a vector of
// tuples (heap offset, bytes) and then deletes the section.
fn get_data(sections: &mut Vec<Section>) -> Segments {
    let mut res = Segments::default();
    let mut data_section_idx = sections.len();
    for (i, section) in sections.iter_mut().enumerate() {
        if let Section::Data(section) = section {
            data_section_idx = i;
            res = section
                .entries_mut()
                .iter_mut()
                .map(|segment| {
                    let offset = match segment.offset() {
                        None => panic!("no offset found for the data segment"),
                        Some(exp) => {
                            match exp.code() {
                                [
                                    Instruction::I32Const(val),
                                    Instruction::End
                               ] => ((*val) as u32) as usize, // Convert via `u32` to avoid 64-bit sign-extension.
                                _ => panic!(
                                    "complex initialization expressions for data segments are not supported!"
                                    ),
                            }
                        }
                    };
                    (offset, std::mem::take(segment.value_mut()))
                })
                .collect();
        }
    }
    if data_section_idx < sections.len() {
        sections.remove(data_section_idx);
    }
    res
}

fn rename_export(export_entry: &mut ExportEntry, name: &str) {
    *export_entry.field_mut() = name.to_string();
}

fn export_table(mut module: Module) -> Module {
    let mut table_already_exported = false;
    if let Some(export_section) = module.export_section_mut() {
        for e in export_section.entries_mut() {
            if let Internal::Table(_) = e.internal() {
                table_already_exported = true;
                rename_export(e, "table");
            }
        }
    }

    if table_already_exported || module.table_section().is_none() {
        module
    } else {
        let mut mbuilder = builder::from_module(module);
        mbuilder.push_export(ExportEntry::new("table".to_string(), Internal::Table(0)));
        mbuilder.build()
    }
}

fn export_memory(mut module: Module) -> Module {
    let mut memory_already_exported = false;
    if let Some(export_section) = module.export_section_mut() {
        for e in export_section.entries_mut() {
            if let Internal::Memory(_) = e.internal() {
                memory_already_exported = true;
                rename_export(e, "memory");
            }
        }
    }

    if memory_already_exported || module.memory_section().is_none() {
        module
    } else {
        let mut mbuilder = builder::from_module(module);
        mbuilder.push_export(ExportEntry::new("memory".to_string(), Internal::Memory(0)));
        mbuilder.build()
    }
}

// Mutable globals must be exported to be persisted.
fn export_mutable_globals(module: Module) -> Module {
    if let Some(global_section) = module.global_section() {
        let mut mutable_exported: Vec<(bool, bool)> = global_section
            .entries()
            .iter()
            .map(|g| g.global_type().is_mutable())
            .zip(std::iter::repeat(false))
            .collect();

        if let Some(export_section) = module.export_section() {
            for e in export_section.entries() {
                if let Internal::Global(ix) = e.internal() {
                    mutable_exported[*ix as usize].1 = true;
                }
            }
        }

        let mut mbuilder = builder::from_module(module);
        for (ix, (mutable, exported)) in mutable_exported.into_iter().enumerate() {
            if mutable && !exported {
                mbuilder.push_export(ExportEntry::new(
                    format!("__persistent_mutable_global_{}", ix),
                    Internal::Global(ix as u32),
                ));
            }
        }
        mbuilder.build()
    } else {
        module
    }
}
