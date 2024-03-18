use std::fmt::Display;

use cairo_lang_casm::assembler::AssembledCairoProgram;
use cairo_lang_casm::instructions::{Instruction, InstructionBody, RetInstruction};
use cairo_lang_sierra::extensions::const_type::ConstConcreteLibfunc;
use cairo_lang_sierra::extensions::core::{
    CoreConcreteLibfunc, CoreLibfunc, CoreType, CoreTypeConcrete,
};
use cairo_lang_sierra::extensions::lib_func::SierraApChange;
use cairo_lang_sierra::extensions::ConcreteLibfunc;
use cairo_lang_sierra::ids::{ConcreteLibfuncId, ConcreteTypeId, VarId};
use cairo_lang_sierra::program::{
    BranchTarget, GenericArg, Invocation, Program, Statement, StatementIdx,
};
use cairo_lang_sierra::program_registry::{ProgramRegistry, ProgramRegistryError};
use cairo_lang_sierra_type_size::{get_type_size_map, TypeSizeMap};
use cairo_lang_utils::casts::IntoOrPanic;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_lang_utils::unordered_hash_map::UnorderedHashMap;
use itertools::{chain, zip_eq};
use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};
use thiserror::Error;

use crate::annotations::{AnnotationError, ProgramAnnotations, StatementAnnotations};
use crate::invocations::enm::get_variant_selector;
use crate::invocations::{
    check_references_on_stack, compile_invocation, BranchChanges, InvocationError, ProgramInfo,
};
use crate::metadata::Metadata;
use crate::references::{check_types_match, ReferenceValue, ReferencesError};
use crate::relocations::{relocate_instructions, RelocationEntry};

#[cfg(test)]
#[path = "compiler_test.rs"]
mod test;

#[derive(Error, Debug, Eq, PartialEq)]
pub enum CompilationError {
    #[error("Failed building type information")]
    FailedBuildingTypeInformation,
    #[error("Error from program registry: {0}")]
    ProgramRegistryError(Box<ProgramRegistryError>),
    #[error(transparent)]
    AnnotationError(#[from] AnnotationError),
    #[error("#{statement_idx}: {error}")]
    InvocationError { statement_idx: StatementIdx, error: InvocationError },
    #[error("#{statement_idx}: Return arguments are not on the stack.")]
    ReturnArgumentsNotOnStack { statement_idx: StatementIdx },
    #[error("#{statement_idx}: {error}")]
    ReferencesError { statement_idx: StatementIdx, error: ReferencesError },
    #[error("#{statement_idx}: Invocation mismatched to libfunc")]
    LibfuncInvocationMismatch { statement_idx: StatementIdx },
    #[error("{var_id} is dangling at #{statement_idx}.")]
    DanglingReferences { statement_idx: StatementIdx, var_id: VarId },
    #[error("#{source_statement_idx}->#{destination_statement_idx}: Expected branch align")]
    ExpectedBranchAlign {
        source_statement_idx: StatementIdx,
        destination_statement_idx: StatementIdx,
    },
    #[error("Const data does not match the declared const type.")]
    ConstDataMismatch,
    #[error("Unsupported const type.")]
    UnsupportedConstType,
    #[error("Const segments must appear in ascending order without holes.")]
    ConstSegmentsOutOfOrder,
    #[error("Code size limit exceeded.")]
    CodeSizeLimitExceeded,
}

/// Configuration for the Sierra to CASM compilation.
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct SierraToCasmConfig {
    /// Whether to check the gas usage of the program.
    pub gas_usage_check: bool,
    /// CASM bytecode size limit.
    pub max_bytecode_size: usize,
}

/// The casm program representation.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CairoProgram {
    pub instructions: Vec<Instruction>,
    pub debug_info: CairoProgramDebugInfo,
    pub consts_info: ConstsInfo,
}
impl Display for CairoProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if std::env::var("PRINT_CASM_BYTECODE_OFFSETS").is_ok() {
            let mut bytecode_offset = 0;
            for instruction in &self.instructions {
                writeln!(f, "{instruction}; // {bytecode_offset}")?;
                bytecode_offset += instruction.body.op_size();
            }
            for segment in self.consts_info.segments.values() {
                writeln!(f, "ret; // {bytecode_offset}")?;
                bytecode_offset += 1;
                for value in &segment.values {
                    writeln!(f, "dw {value}; // {bytecode_offset}")?;
                    bytecode_offset += 1;
                }
            }
        } else {
            for instruction in &self.instructions {
                writeln!(f, "{instruction};")?;
            }
            for segment in self.consts_info.segments.values() {
                writeln!(f, "ret;")?;
                for value in &segment.values {
                    writeln!(f, "dw {value};")?;
                }
            }
        }
        Ok(())
    }
}

impl CairoProgram {
    /// Creates an assembled representation of the program.
    pub fn assemble(&self) -> AssembledCairoProgram {
        self.assemble_ex(&[], &[])
    }

    /// Creates an assembled representation of the program preceded by `header` and followed by
    /// `footer`.
    pub fn assemble_ex(
        &self,
        header: &[Instruction],
        footer: &[Instruction],
    ) -> AssembledCairoProgram {
        let mut bytecode = vec![];
        let mut hints = vec![];
        for instruction in chain!(header, &self.instructions) {
            if !instruction.hints.is_empty() {
                hints.push((bytecode.len(), instruction.hints.clone()))
            }
            bytecode.extend(instruction.assemble().encode().into_iter())
        }
        let [ref ret_bytecode] = Instruction::new(InstructionBody::Ret(RetInstruction {}), false)
            .assemble()
            .encode()[..]
        else {
            panic!("`ret` instruction should be a single word.")
        };
        for segment in self.consts_info.segments.values() {
            bytecode.push(ret_bytecode.clone());
            bytecode.extend(segment.values.clone());
        }
        for instruction in footer {
            assert!(
                instruction.hints.is_empty(),
                "All footer instructions must have no hints since these cannot be added to the \
                 hints dict."
            );
            bytecode.extend(instruction.assemble().encode().into_iter())
        }
        AssembledCairoProgram { bytecode, hints }
    }
}

/// The debug information of a compilation from Sierra to casm.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct SierraStatementDebugInfo {
    /// The offset of the sierra statement within the bytecode.
    pub code_offset: usize,
    /// The index of the sierra statement in the instructions vector.
    pub instruction_idx: usize,
    /// Statement-kind-dependent information.
    pub additional_kind_info: StatementKindDebugInfo,
}

/// Additional debug information for a Sierra statement, depending on its kind
/// (invoke/return/dummy).
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum StatementKindDebugInfo {
    Return(ReturnStatementDebugInfo),
    Invoke(InvokeStatementDebugInfo),
    /// Dummy marker for the end of the program. It is used for a fake statement that contains the
    /// final offset (the size of the program code segment).
    EndMarker,
}

/// Additional debug information for a return Sierra statement.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct ReturnStatementDebugInfo {
    /// The references of a Sierra return statement.
    pub ref_values: Vec<ReferenceValue>,
}

/// Additional debug information for an invoke Sierra statement.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct InvokeStatementDebugInfo {
    /// The result branch changes of a Sierra invoke statement.
    pub result_branch_changes: Vec<BranchChanges>,
    /// The references of a Sierra invoke statement.
    pub ref_values: Vec<ReferenceValue>,
}

/// The debug information of a compilation from Sierra to casm.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CairoProgramDebugInfo {
    /// The debug information per Sierra statement.
    pub sierra_statement_info: Vec<SierraStatementDebugInfo>,
}

/// The information about the constants used in the program.
#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct ConstsInfo {
    pub segments: OrderedHashMap<u32, ConstSegment>,
    pub total_segments_size: usize,
}
impl ConstsInfo {
    /// Creates a new `ConstSegmentsInfo` from the given libfuncs.
    pub fn new<'a>(
        registry: &ProgramRegistry<CoreType, CoreLibfunc>,
        type_sizes: &TypeSizeMap,
        libfunc_ids: impl Iterator<Item = &'a ConcreteLibfuncId>,
        const_segments_max_size: usize,
    ) -> Result<Self, CompilationError> {
        let mut segments_data_size = 0;
        let mut segments = OrderedHashMap::default();
        for id in libfunc_ids {
            if let CoreConcreteLibfunc::Const(ConstConcreteLibfunc::AsBox(as_box)) =
                registry.get_libfunc(id).unwrap()
            {
                let segment: &mut ConstSegment = segments.entry(as_box.segment_id).or_default();
                let const_data =
                    extract_const_value(registry, type_sizes, &as_box.const_type).unwrap();
                segments_data_size += const_data.len();
                segment.const_offset.insert(as_box.const_type.clone(), segment.values.len());
                segment.values.extend(const_data);
                if segments_data_size + segments.len() > const_segments_max_size {
                    return Err(CompilationError::CodeSizeLimitExceeded);
                }
            }
        }
        // Check that the segments were declared in order and without holes.
        if segments
            .keys()
            .enumerate()
            .any(|(i, segment_id)| i != segment_id.into_or_panic::<usize>())
        {
            return Err(CompilationError::ConstSegmentsOutOfOrder);
        }

        let mut total_segments_size = 0;
        for (_, segment) in segments.iter_mut() {
            segment.segment_offset = total_segments_size;
            // Add 1 for the `ret` instruction.
            total_segments_size += 1 + segment.values.len();
        }
        Ok(Self { segments, total_segments_size })
    }
}

/// The data for a single segment.
#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct ConstSegment {
    /// The values in the segment.
    pub values: Vec<BigInt>,
    /// The offset of each const within the segment.
    pub const_offset: UnorderedHashMap<ConcreteTypeId, usize>,
    /// The offset of the segment relative to the end of the code segment.
    pub segment_offset: usize,
}

/// Gets a concrete type, if it is a const type returns a vector of the values to be stored in the
/// const segment.
fn extract_const_value(
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    type_sizes: &TypeSizeMap,
    ty: &ConcreteTypeId,
) -> Result<Vec<BigInt>, CompilationError> {
    let mut values = Vec::new();
    let mut types_stack = vec![ty.clone()];
    while let Some(ty) = types_stack.pop() {
        let CoreTypeConcrete::Const(const_type) = registry.get_type(&ty).unwrap() else {
            return Err(CompilationError::UnsupportedConstType);
        };
        let inner_type = registry.get_type(&const_type.inner_ty).unwrap();
        match inner_type {
            CoreTypeConcrete::Struct(_) => {
                // Add the struct members' types to the stack in reverse order.
                for arg in const_type.inner_data.iter().rev() {
                    match arg {
                        GenericArg::Type(arg_ty) => types_stack.push(arg_ty.clone()),
                        _ => return Err(CompilationError::ConstDataMismatch),
                    }
                }
            }
            CoreTypeConcrete::Enum(enm) => {
                // The first argument is the variant selector, the second is the variant data.
                match &const_type.inner_data[..] {
                    [GenericArg::Value(variant_index), GenericArg::Type(ty)] => {
                        let variant_index = variant_index.to_usize().unwrap();
                        values.push(
                            get_variant_selector(enm.variants.len(), variant_index).unwrap().into(),
                        );
                        let full_enum_size: usize =
                            type_sizes[&const_type.inner_ty].into_or_panic();
                        let variant_size: usize =
                            type_sizes[&enm.variants[variant_index]].into_or_panic();
                        // Padding with zeros to full enum size.
                        values.extend(itertools::repeat_n(
                            BigInt::zero(),
                            // Subtract 1 due to the variant selector.
                            full_enum_size - variant_size - 1,
                        ));
                        types_stack.push(ty.clone());
                    }
                    _ => return Err(CompilationError::ConstDataMismatch),
                }
            }
            _ => match &const_type.inner_data[..] {
                [GenericArg::Value(value)] => {
                    values.push(value.clone());
                }
                _ => return Err(CompilationError::ConstDataMismatch),
            },
        };
    }
    Ok(values)
}

/// Ensure the basic structure of the invocation is the same as the library function.
pub fn check_basic_structure(
    statement_idx: StatementIdx,
    invocation: &Invocation,
    libfunc: &CoreConcreteLibfunc,
) -> Result<(), CompilationError> {
    if invocation.args.len() != libfunc.param_signatures().len()
        || !itertools::equal(
            invocation.branches.iter().map(|branch| branch.results.len()),
            libfunc.output_types().iter().map(|types| types.len()),
        )
        || match libfunc.fallthrough() {
            Some(expected_fallthrough) => {
                invocation.branches[expected_fallthrough].target != BranchTarget::Fallthrough
            }
            None => false,
        }
    {
        Err(CompilationError::LibfuncInvocationMismatch { statement_idx })
    } else {
        Ok(())
    }
}

/// Compiles `program` from Sierra to CASM using `metadata` for information regarding AP changes
/// and gas usage, and config additional compilation flavours.
pub fn compile(
    program: &Program,
    metadata: &Metadata,
    config: SierraToCasmConfig,
) -> Result<CairoProgram, Box<CompilationError>> {
    let mut instructions = Vec::new();
    let mut relocations: Vec<RelocationEntry> = Vec::new();

    // Maps statement_idx to its debug info.
    // The last value (for statement_idx=number-of-statements)
    // contains the final offset (the size of the program code segment).
    let mut sierra_statement_info: Vec<SierraStatementDebugInfo> =
        Vec::with_capacity(program.statements.len());

    let registry = ProgramRegistry::<CoreType, CoreLibfunc>::new_with_ap_change(
        program,
        metadata.ap_change_info.function_ap_change.clone(),
    )
    .map_err(CompilationError::ProgramRegistryError)?;
    let type_sizes = get_type_size_map(program, &registry)
        .ok_or(CompilationError::FailedBuildingTypeInformation)?;
    let mut program_annotations = ProgramAnnotations::create(
        program.statements.len(),
        &program.funcs,
        metadata,
        config.gas_usage_check,
        &type_sizes,
    )
    .map_err(|err| Box::new(err.into()))?;

    let mut program_offset: usize = 0;

    for (statement_id, statement) in program.statements.iter().enumerate() {
        let statement_idx = StatementIdx(statement_id);

        if program_offset > config.max_bytecode_size {
            return Err(Box::new(CompilationError::CodeSizeLimitExceeded));
        }
        match statement {
            Statement::Return(ref_ids) => {
                let (annotations, return_refs) = program_annotations
                    .get_annotations_after_take_args(statement_idx, ref_ids.iter())
                    .map_err(|err| Box::new(err.into()))?;
                return_refs.iter().for_each(|r| r.validate(&type_sizes));

                if let Some(var_id) = annotations.refs.keys().next() {
                    return Err(Box::new(CompilationError::DanglingReferences {
                        statement_idx,
                        var_id: var_id.clone(),
                    }));
                };

                program_annotations
                    .validate_final_annotations(
                        statement_idx,
                        &annotations,
                        &program.funcs,
                        metadata,
                        &return_refs,
                    )
                    .map_err(|err| Box::new(err.into()))?;
                check_references_on_stack(&return_refs).map_err(|error| match error {
                    InvocationError::InvalidReferenceExpressionForArgument => {
                        CompilationError::ReturnArgumentsNotOnStack { statement_idx }
                    }
                    _ => CompilationError::InvocationError { statement_idx, error },
                })?;

                let ret_instruction = RetInstruction {};
                program_offset += ret_instruction.op_size();
                instructions.push(Instruction::new(InstructionBody::Ret(ret_instruction), false));

                sierra_statement_info.push(SierraStatementDebugInfo {
                    code_offset: program_offset,
                    instruction_idx: instructions.len(),
                    additional_kind_info: StatementKindDebugInfo::Return(
                        ReturnStatementDebugInfo { ref_values: return_refs },
                    ),
                });
            }
            Statement::Invocation(invocation) => {
                let (annotations, invoke_refs) = program_annotations
                    .get_annotations_after_take_args(statement_idx, invocation.args.iter())
                    .map_err(|err| Box::new(err.into()))?;

                let libfunc = registry
                    .get_libfunc(&invocation.libfunc_id)
                    .map_err(CompilationError::ProgramRegistryError)?;
                check_basic_structure(statement_idx, invocation, libfunc)?;

                let param_types: Vec<_> = libfunc
                    .param_signatures()
                    .iter()
                    .map(|param_signature| param_signature.ty.clone())
                    .collect();
                check_types_match(&invoke_refs, &param_types).map_err(|error| {
                    Box::new(AnnotationError::ReferencesError { statement_idx, error }.into())
                })?;
                invoke_refs.iter().for_each(|r| r.validate(&type_sizes));
                let compiled_invocation = compile_invocation(
                    ProgramInfo { metadata, type_sizes: &type_sizes },
                    invocation,
                    libfunc,
                    statement_idx,
                    &invoke_refs,
                    annotations.environment,
                )
                .map_err(|error| CompilationError::InvocationError { statement_idx, error })?;

                for instruction in &compiled_invocation.instructions {
                    program_offset += instruction.body.op_size();
                }

                for entry in compiled_invocation.relocations {
                    relocations.push(RelocationEntry {
                        instruction_idx: instructions.len() + entry.instruction_idx,
                        relocation: entry.relocation,
                    });
                }
                instructions.extend(compiled_invocation.instructions);

                let updated_annotations = StatementAnnotations {
                    environment: compiled_invocation.environment,
                    ..annotations
                };

                sierra_statement_info.push(SierraStatementDebugInfo {
                    code_offset: program_offset,
                    instruction_idx: instructions.len(),
                    additional_kind_info: StatementKindDebugInfo::Invoke(
                        InvokeStatementDebugInfo {
                            result_branch_changes: compiled_invocation.results.clone(),
                            ref_values: invoke_refs,
                        },
                    ),
                });

                let branching_libfunc = compiled_invocation.results.len() > 1;

                for (branch_info, branch_changes) in
                    zip_eq(&invocation.branches, compiled_invocation.results)
                {
                    let destination_statement_idx = statement_idx.next(&branch_info.target);
                    if branching_libfunc
                        && !is_branch_align(
                            &registry,
                            &program.statements[destination_statement_idx.0],
                        )?
                    {
                        return Err(Box::new(CompilationError::ExpectedBranchAlign {
                            source_statement_idx: statement_idx,
                            destination_statement_idx,
                        }));
                    }

                    program_annotations
                        .propagate_annotations(
                            statement_idx,
                            destination_statement_idx,
                            &updated_annotations,
                            branch_info,
                            branch_changes,
                            branching_libfunc,
                        )
                        .map_err(|err| Box::new(err.into()))?;
                }
            }
        }
    }
    // Push the final offset and index at the end of the vectors.
    sierra_statement_info.push(SierraStatementDebugInfo {
        code_offset: program_offset,
        instruction_idx: instructions.len(),
        additional_kind_info: StatementKindDebugInfo::EndMarker,
    });

    let statement_offsets: Vec<usize> =
        sierra_statement_info.iter().map(|s: &SierraStatementDebugInfo| s.code_offset).collect();

    let const_segments_max_size = config
        .max_bytecode_size
        .checked_sub(program_offset)
        .ok_or_else(|| Box::new(CompilationError::CodeSizeLimitExceeded))?;
    let consts_info = ConstsInfo::new(
        &registry,
        &type_sizes,
        program.libfunc_declarations.iter().map(|ld| &ld.id),
        const_segments_max_size,
    )?;
    relocate_instructions(&relocations, &statement_offsets, &consts_info, &mut instructions);

    Ok(CairoProgram {
        instructions,
        consts_info,
        debug_info: CairoProgramDebugInfo { sierra_statement_info },
    })
}

/// Returns true if `statement` is an invocation of the branch_align libfunc.
fn is_branch_align(
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    statement: &Statement,
) -> Result<bool, CompilationError> {
    if let Statement::Invocation(invocation) = statement {
        let libfunc = registry
            .get_libfunc(&invocation.libfunc_id)
            .map_err(CompilationError::ProgramRegistryError)?;
        if let [branch_signature] = libfunc.branch_signatures() {
            if branch_signature.ap_change == SierraApChange::BranchAlign {
                return Ok(true);
            }
        }
    }

    Ok(false)
}
