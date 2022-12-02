use super::{
    errors::exec_scope_errors::ExecScopeError, runners::builtin_runner::SignatureBuiltinRunner,
};
use crate::{
    bigint,
    hint_processor::{
        hint_processor_definition::HintProcessor, hint_processor_utils::bigint_to_usize,
    },
    serde::deserialize_program::{ApTracking, Attribute},
    types::{
        exec_scope::ExecutionScopes,
        instruction::{ApUpdate, FpUpdate, Instruction, Opcode, PcUpdate, Res},
        relocatable::{MaybeRelocatable, Relocatable},
    },
    vm::{
        context::run_context::RunContext,
        decoding::decoder::decode_instruction,
        errors::{memory_errors::MemoryError, vm_errors::VirtualMachineError},
        runners::builtin_runner::{BuiltinRunner, RangeCheckBuiltinRunner},
        trace::trace_entry::TraceEntry,
        vm_memory::{memory::Memory, memory_segments::MemorySegmentManager},
    },
};
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{ToPrimitive, Zero};
use std::{any::Any, borrow::Cow, collections::HashMap};

#[derive(PartialEq, Debug)]
pub struct Operands {
    dst: MaybeRelocatable,
    res: Option<MaybeRelocatable>,
    op0: MaybeRelocatable,
    op1: MaybeRelocatable,
}

#[derive(PartialEq, Debug)]
struct OperandsAddresses(Relocatable, Relocatable, Relocatable);

#[derive(Clone, Debug)]
pub struct HintData {
    pub hint_code: String,
    //Maps the name of the variable to its reference id
    pub ids: HashMap<String, usize>,
    pub ap_tracking_data: ApTracking,
}

pub struct VirtualMachine {
    pub(crate) run_context: RunContext,
    pub(crate) prime: BigInt,
    pub(crate) builtin_runners: Vec<(String, BuiltinRunner)>,
    pub(crate) segments: MemorySegmentManager,
    pub(crate) _program_base: Option<MaybeRelocatable>,
    pub(crate) memory: Memory,
    pub(crate) accessed_addresses: Option<Vec<Relocatable>>,
    pub(crate) trace: Option<Vec<TraceEntry>>,
    pub(crate) current_step: usize,
    pub(crate) error_message_attributes: Vec<Attribute>,
    skip_instruction_execution: bool,
}

impl HintData {
    pub fn new(
        hint_code: &str,
        ids: HashMap<String, usize>,
        ap_tracking_data: ApTracking,
    ) -> HintData {
        HintData {
            hint_code: hint_code.to_string(),
            ids,
            ap_tracking_data,
        }
    }
}

impl VirtualMachine {
    pub fn new(
        prime: BigInt,
        trace_enabled: bool,
        error_message_attributes: Vec<Attribute>,
    ) -> VirtualMachine {
        let run_context = RunContext {
            pc: Relocatable::from((0, 0)),
            ap: 0,
            fp: 0,
        };

        let trace = if trace_enabled {
            Some(Vec::<TraceEntry>::new())
        } else {
            None
        };

        VirtualMachine {
            run_context,
            prime,
            builtin_runners: Vec::new(),
            _program_base: None,
            memory: Memory::new(),
            // We had to change this from None to this Some because when calling run_from_entrypoint from cairo-rs-py
            // we could not change this value and faced an Error. This is the behaviour that the original VM implements also.
            accessed_addresses: Some(Vec::new()),
            trace,
            current_step: 0,
            skip_instruction_execution: false,
            segments: MemorySegmentManager::new(),
            error_message_attributes,
        }
    }

    //Simple getter for trace attribute
    pub fn get_trace(&self) -> Option<&Vec<TraceEntry>> {
        return self.trace.as_ref();
    }

    ///Returns the encoded instruction (the value at pc) and the immediate value (the value at pc + 1, if it exists in the memory).
    fn get_instruction_encoding(
        &self,
    ) -> Result<(Cow<BigInt>, Option<Cow<MaybeRelocatable>>), VirtualMachineError> {
        let encoding_ref = match self.memory.get(&self.run_context.pc) {
            Ok(Some(Cow::Owned(MaybeRelocatable::Int(encoding)))) => Cow::Owned(encoding),
            Ok(Some(Cow::Borrowed(MaybeRelocatable::Int(encoding)))) => Cow::Borrowed(encoding),
            _ => return Err(VirtualMachineError::InvalidInstructionEncoding),
        };

        let imm_addr = &self.run_context.pc + 1;

        if let Ok(optional_imm) = self.memory.get(&imm_addr) {
            Ok((encoding_ref, optional_imm))
        } else {
            Err(VirtualMachineError::InvalidInstructionEncoding)
        }
    }

    fn update_fp(
        &mut self,
        instruction: &Instruction,
        operands: &Operands,
    ) -> Result<(), VirtualMachineError> {
        let new_fp_offset: usize = match instruction.fp_update {
            FpUpdate::APPlus2 => self.run_context.ap + 2,
            FpUpdate::Dst => match operands.dst {
                MaybeRelocatable::RelocatableValue(ref rel) => rel.offset,
                MaybeRelocatable::Int(ref num) => bigint_to_usize(num)?,
            },
            FpUpdate::Regular => return Ok(()),
        };
        self.run_context.fp = new_fp_offset;
        Ok(())
    }

    fn update_ap(
        &mut self,
        instruction: &Instruction,
        operands: &Operands,
    ) -> Result<(), VirtualMachineError> {
        let new_ap: Relocatable = match instruction.ap_update {
            ApUpdate::Add => match operands.res.clone() {
                Some(res) => self.run_context.get_ap().add_maybe_mod(&res, &self.prime)?,
                None => return Err(VirtualMachineError::UnconstrainedResAdd),
            },
            ApUpdate::Add1 => self.run_context.get_ap() + 1,
            ApUpdate::Add2 => self.run_context.get_ap() + 2,
            ApUpdate::Regular => return Ok(()),
        };
        self.run_context.ap = new_ap.offset;
        Ok(())
    }

    fn update_pc(
        &mut self,
        instruction: &Instruction,
        operands: &Operands,
    ) -> Result<(), VirtualMachineError> {
        let new_pc: Relocatable = match instruction.pc_update {
            PcUpdate::Regular => &self.run_context.pc + instruction.size(),
            PcUpdate::Jump => match &operands.res {
                Some(ref res) => res.get_relocatable()?.clone(),
                None => return Err(VirtualMachineError::UnconstrainedResJump),
            },
            PcUpdate::JumpRel => match &operands.res {
                Some(ref res) => match res {
                    MaybeRelocatable::Int(num_res) => {
                        self.run_context.pc.add_int_mod(num_res, &self.prime)?
                    }

                    _ => return Err(VirtualMachineError::PureValue),
                },
                None => return Err(VirtualMachineError::UnconstrainedResJumpRel),
            },
            PcUpdate::Jnz => match VirtualMachine::is_zero(&operands.dst)? {
                true => &self.run_context.pc + instruction.size(),
                false => {
                    (self
                        .run_context
                        .pc
                        .add_maybe_mod(&operands.op1, &self.prime))?
                }
            },
        };
        self.run_context.pc = new_pc;
        Ok(())
    }

    fn update_registers(
        &mut self,
        instruction: Instruction,
        operands: Operands,
    ) -> Result<(), VirtualMachineError> {
        self.update_fp(&instruction, &operands)?;
        self.update_ap(&instruction, &operands)?;
        self.update_pc(&instruction, &operands)?;
        Ok(())
    }

    /// Returns true if the value is zero
    /// Used for JNZ instructions
    fn is_zero(addr: &MaybeRelocatable) -> Result<bool, VirtualMachineError> {
        match addr {
            MaybeRelocatable::Int(num) => Ok(num.is_zero()),
            MaybeRelocatable::RelocatableValue(_rel_value) => Err(VirtualMachineError::PureValue),
        }
    }

    ///Returns a tuple (deduced_op0, deduced_res).
    ///Deduces the value of op0 if possible (based on dst and op1). Otherwise, returns None.
    ///If res was already deduced, returns its deduced value as well.
    fn deduce_op0(
        &self,
        instruction: &Instruction,
        dst: Option<&MaybeRelocatable>,
        op1: Option<&MaybeRelocatable>,
    ) -> Result<(Option<MaybeRelocatable>, Option<MaybeRelocatable>), VirtualMachineError> {
        match instruction.opcode {
            Opcode::Call => {
                return Ok((
                    Some(MaybeRelocatable::from(
                        &self.run_context.pc + instruction.size(),
                    )),
                    None,
                ))
            }
            Opcode::AssertEq => {
                match instruction.res {
                    Res::Add => {
                        if let (Some(dst_addr), Some(op1_addr)) = (dst, op1) {
                            return Ok((
                                Some((dst_addr.sub(op1_addr, &self.prime))?),
                                Some(dst_addr.clone()),
                            ));
                        }
                    }
                    Res::Mul => {
                        if let (Some(dst_addr), Some(op1_addr)) = (dst, op1) {
                            if let (
                                MaybeRelocatable::Int(num_dst),
                                MaybeRelocatable::Int(ref num_op1_ref),
                            ) = (dst_addr, op1_addr)
                            {
                                let num_op1 = Clone::clone(num_op1_ref);
                                if num_op1 != bigint!(0) {
                                    return Ok((
                                        Some(MaybeRelocatable::Int(
                                            (num_dst / num_op1).mod_floor(&self.prime),
                                        )),
                                        Some(dst_addr.clone()),
                                    ));
                                }
                            }
                        }
                    }
                    _ => (),
                };
            }
            _ => (),
        };
        Ok((None, None))
    }

    /// Returns a tuple (deduced_op1, deduced_res).
    ///Deduces the value of op1 if possible (based on dst and op0). Otherwise, returns None.
    ///If res was already deduced, returns its deduced value as well.
    fn deduce_op1(
        &self,
        instruction: &Instruction,
        dst: Option<&MaybeRelocatable>,
        op0: Option<MaybeRelocatable>,
    ) -> Result<(Option<MaybeRelocatable>, Option<MaybeRelocatable>), VirtualMachineError> {
        if let Opcode::AssertEq = instruction.opcode {
            match instruction.res {
                Res::Op1 => {
                    if let Some(dst_addr) = dst {
                        return Ok((Some(dst_addr.clone()), Some(dst_addr.clone())));
                    }
                }
                Res::Add => {
                    if let (Some(dst_addr), Some(op0_addr)) = (dst, op0) {
                        return Ok((
                            Some((dst_addr.sub(&op0_addr, &self.prime))?),
                            Some(dst_addr.clone()),
                        ));
                    }
                }
                Res::Mul => {
                    if let (Some(dst_addr), Some(op0_addr)) = (dst, op0) {
                        if let (MaybeRelocatable::Int(num_dst), MaybeRelocatable::Int(num_op0)) =
                            (dst_addr, op0_addr)
                        {
                            if num_op0 != bigint!(0) {
                                return Ok((
                                    Some(MaybeRelocatable::Int(
                                        (num_dst / num_op0).mod_floor(&self.prime),
                                    )),
                                    Some(dst_addr.clone()),
                                ));
                            }
                        }
                    }
                }
                _ => (),
            };
        };
        Ok((None, None))
    }

    fn deduce_memory_cell(
        &mut self,
        address: &Relocatable,
    ) -> Result<Option<MaybeRelocatable>, VirtualMachineError> {
        for (_, builtin) in self.builtin_runners.iter_mut() {
            if builtin.base() == address.segment_index {
                match builtin.deduce_memory_cell(address, &self.memory) {
                    Ok(maybe_reloc) => return Ok(maybe_reloc),
                    Err(error) => return Err(VirtualMachineError::RunnerError(error)),
                };
            }
        }
        Ok(None)
    }

    ///Computes the value of res if possible
    fn compute_res(
        &self,
        instruction: &Instruction,
        op0: &MaybeRelocatable,
        op1: &MaybeRelocatable,
    ) -> Result<Option<MaybeRelocatable>, VirtualMachineError> {
        match instruction.res {
            Res::Op1 => Ok(Some(op1.clone())),
            Res::Add => Ok(Some(op0.add_mod(op1, &self.prime)?)),
            Res::Mul => {
                if let (MaybeRelocatable::Int(num_op0), MaybeRelocatable::Int(num_op1)) = (op0, op1)
                {
                    return Ok(Some(MaybeRelocatable::Int(
                        (num_op0 * num_op1).mod_floor(&self.prime),
                    )));
                }
                Err(VirtualMachineError::PureValue)
            }
            Res::Unconstrained => Ok(None),
        }
    }

    fn deduce_dst(
        &self,
        instruction: &Instruction,
        res: Option<&MaybeRelocatable>,
    ) -> Option<MaybeRelocatable> {
        match instruction.opcode {
            Opcode::AssertEq => {
                if let Some(res_addr) = res {
                    return Some(res_addr.clone());
                }
            }
            Opcode::Call => return Some(MaybeRelocatable::from(self.run_context.get_fp())),
            _ => (),
        };
        None
    }

    fn opcode_assertions(
        &self,
        instruction: &Instruction,
        operands: &Operands,
    ) -> Result<(), VirtualMachineError> {
        match instruction.opcode {
            Opcode::AssertEq => {
                match &operands.res {
                    None => return Err(VirtualMachineError::UnconstrainedResAssertEq),
                    Some(res) => {
                        if let (MaybeRelocatable::Int(res_num), MaybeRelocatable::Int(dst_num)) =
                            (res, &operands.dst)
                        {
                            if res_num != dst_num {
                                return Err(VirtualMachineError::DiffAssertValues(
                                    dst_num.clone(),
                                    res_num.clone(),
                                ));
                            };
                        };
                    }
                };
                Ok(())
            }
            Opcode::Call => {
                let return_pc = MaybeRelocatable::from(&self.run_context.pc + instruction.size());
                if operands.op0 != return_pc {
                    return Err(VirtualMachineError::CantWriteReturnPc(
                        operands.op0.clone(),
                        return_pc,
                    ));
                };

                if MaybeRelocatable::from(self.run_context.get_fp()) != operands.dst {
                    return Err(VirtualMachineError::CantWriteReturnFp(
                        operands.dst.clone(),
                        MaybeRelocatable::from(self.run_context.get_fp()),
                    ));
                };
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn run_instruction(&mut self, instruction: Instruction) -> Result<(), VirtualMachineError> {
        let (operands, operands_mem_addresses) = self.compute_operands(&instruction)?;
        self.opcode_assertions(&instruction, &operands)?;

        if let Some(ref mut trace) = &mut self.trace {
            trace.push(TraceEntry {
                pc: self.run_context.pc.clone(),
                ap: self.run_context.get_ap(),
                fp: self.run_context.get_fp(),
            });
        }

        if let Some(ref mut accessed_addresses) = self.accessed_addresses {
            let op_addrs =
                operands_mem_addresses.ok_or(VirtualMachineError::InvalidInstructionEncoding)?;
            let addresses = [
                op_addrs.0,
                op_addrs.1,
                op_addrs.2,
                self.run_context.pc.clone(),
            ];
            accessed_addresses.extend(addresses.into_iter());
        }

        self.update_registers(instruction, operands)?;
        self.current_step += 1;
        Ok(())
    }

    fn decode_current_instruction(&self) -> Result<Instruction, VirtualMachineError> {
        let (instruction_ref, imm) = self.get_instruction_encoding()?;
        match instruction_ref.to_i64() {
            Some(instruction) => {
                if let Some(MaybeRelocatable::Int(imm_ref)) = imm.as_ref().map(|x| x.as_ref()) {
                    let decoded_instruction =
                        decode_instruction(instruction, Some(imm_ref.clone()))?;
                    return Ok(decoded_instruction);
                }
                let decoded_instruction = decode_instruction(instruction, None)?;
                Ok(decoded_instruction)
            }
            None => Err(VirtualMachineError::InvalidInstructionEncoding),
        }
    }

    pub fn step_hint(
        &mut self,
        hint_executor: &dyn HintProcessor,
        exec_scopes: &mut ExecutionScopes,
        hint_data_dictionary: &HashMap<usize, Vec<Box<dyn Any>>>,
        constants: &HashMap<String, BigInt>,
    ) -> Result<(), VirtualMachineError> {
        if let Some(hint_list) = hint_data_dictionary.get(&self.run_context.pc.offset) {
            for hint_data in hint_list.iter() {
                hint_executor.execute_hint(self, exec_scopes, hint_data, constants)?
            }
        }
        Ok(())
    }

    pub fn step_instruction(&mut self) -> Result<(), VirtualMachineError> {
        let instruction = self.decode_current_instruction()?;
        self.run_instruction(instruction).map_err(|err| {
            let pc = &self.get_pc().offset;
            let attr_error_msg = &self
                .error_message_attributes
                .iter()
                .find(|attr| attr.start_pc <= *pc && attr.end_pc >= *pc);
            match attr_error_msg {
                Some(attr) => VirtualMachineError::ErrorMessageAttribute(
                    attr.value.to_string(),
                    Box::new(err),
                ),
                _ => err,
            }
        })?;
        self.skip_instruction_execution = false;
        Ok(())
    }

    pub fn step(
        &mut self,
        hint_executor: &dyn HintProcessor,
        exec_scopes: &mut ExecutionScopes,
        hint_data_dictionary: &HashMap<usize, Vec<Box<dyn Any>>>,
        constants: &HashMap<String, BigInt>,
    ) -> Result<(), VirtualMachineError> {
        self.step_hint(hint_executor, exec_scopes, hint_data_dictionary, constants)?;
        self.step_instruction()
    }

    fn compute_op0_deductions(
        &mut self,
        op0_addr: &Relocatable,
        res: &mut Option<MaybeRelocatable>,
        instruction: &Instruction,
        dst_op: &Option<MaybeRelocatable>,
        op1_op: &Option<MaybeRelocatable>,
    ) -> Result<MaybeRelocatable, VirtualMachineError> {
        let op0_op = match self.deduce_memory_cell(op0_addr)? {
            None => {
                let op0;
                (op0, *res) = self.deduce_op0(instruction, dst_op.as_ref(), op1_op.as_ref())?;
                op0
            }
            deduced_memory_cell => deduced_memory_cell,
        };
        let op0 = op0_op.ok_or(VirtualMachineError::FailedToComputeOperands)?;
        self.memory
            .insert(op0_addr, &op0)
            .map_err(VirtualMachineError::MemoryError)?;
        Ok(op0)
    }

    fn compute_op1_deductions(
        &mut self,
        op1_addr: &Relocatable,
        res: &mut Option<MaybeRelocatable>,
        instruction: &Instruction,
        dst_op: &Option<MaybeRelocatable>,
        op0: &MaybeRelocatable,
    ) -> Result<MaybeRelocatable, VirtualMachineError> {
        let op1_op = match self.deduce_memory_cell(op1_addr)? {
            None => {
                let (op1, deduced_res) =
                    self.deduce_op1(instruction, dst_op.as_ref(), Some(op0.clone()))?;
                if res.is_none() {
                    *res = deduced_res
                }
                op1
            }
            deduced_memory_cell => deduced_memory_cell,
        };
        let op1 = op1_op.ok_or(VirtualMachineError::FailedToComputeOperands)?;
        self.memory
            .insert(op1_addr, &op1)
            .map_err(VirtualMachineError::MemoryError)?;
        Ok(op1)
    }

    fn compute_dst_deductions(
        &mut self,
        dst_addr: &Relocatable,
        instruction: &Instruction,
        res: &Option<MaybeRelocatable>,
    ) -> Result<MaybeRelocatable, VirtualMachineError> {
        let dst_op = match instruction.opcode {
            Opcode::AssertEq if res.is_some() => Option::clone(res),
            Opcode::Call => Some(MaybeRelocatable::from(self.run_context.get_fp())),
            _ => self.deduce_dst(instruction, res.as_ref()),
        };
        let dst = dst_op.ok_or(VirtualMachineError::NoDst)?;
        self.memory
            .insert(dst_addr, &dst)
            .map_err(VirtualMachineError::MemoryError)?;
        Ok(dst)
    }

    /// Compute operands and result, trying to deduce them if normal memory access returns a None
    /// value.
    fn compute_operands(
        &mut self,
        instruction: &Instruction,
    ) -> Result<(Operands, Option<OperandsAddresses>), VirtualMachineError> {
        //Get operands from memory
        let dst_addr = self.run_context.compute_dst_addr(instruction)?;
        let dst_op = self
            .memory
            .get(&dst_addr)
            .map_err(VirtualMachineError::MemoryError)?
            .map(Cow::into_owned);

        let op0_addr = self.run_context.compute_op0_addr(instruction)?;
        let op0_op = self
            .memory
            .get(&op0_addr)
            .map_err(VirtualMachineError::MemoryError)?
            .map(Cow::into_owned);

        let op1_addr = self
            .run_context
            .compute_op1_addr(instruction, op0_op.as_ref())?;
        let op1_op = self
            .memory
            .get(&op1_addr)
            .map_err(VirtualMachineError::MemoryError)?
            .map(Cow::into_owned);

        let mut res: Option<MaybeRelocatable> = None;

        //Deduce op0 if it wasnt previously computed
        let op0 = match op0_op {
            Some(op0) => op0,
            None => {
                self.compute_op0_deductions(&op0_addr, &mut res, instruction, &dst_op, &op1_op)?
            }
        };

        //Deduce op1 if it wasnt previously computed
        let op1 = match op1_op {
            Some(op1) => op1,
            None => self.compute_op1_deductions(&op1_addr, &mut res, instruction, &dst_op, &op0)?,
        };

        //Compute res if it wasnt previously deduced
        if res.is_none() {
            res = self.compute_res(instruction, &op0, &op1)?;
        }

        //Deduce dst if it wasnt previously computed
        let dst = match dst_op {
            Some(dst) => dst,
            None => self.compute_dst_deductions(&dst_addr, instruction, &res)?,
        };
        let accessed_addresses = if self.accessed_addresses.is_some() {
            Some(OperandsAddresses(dst_addr, op0_addr, op1_addr))
        } else {
            None
        };
        Ok((Operands { dst, op0, op1, res }, accessed_addresses))
    }

    ///Makes sure that all assigned memory cells are consistent with their auto deduction rules.
    pub fn verify_auto_deductions(&mut self) -> Result<(), VirtualMachineError> {
        for (name, builtin) in self.builtin_runners.iter_mut() {
            let index: usize = builtin
                .base()
                .try_into()
                .map_err(|_| MemoryError::AddressInTemporarySegment(builtin.base()))?;
            for (offset, value) in self.memory.data[index].iter().enumerate() {
                if let Some(deduced_memory_cell) = builtin
                    .deduce_memory_cell(&Relocatable::from((index as isize, offset)), &self.memory)
                    .map_err(VirtualMachineError::RunnerError)?
                {
                    if Some(&deduced_memory_cell) != value.as_ref() && value != &None {
                        return Err(VirtualMachineError::InconsistentAutoDeduction(
                            name.to_owned(),
                            deduced_memory_cell,
                            value.to_owned(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn end_run(&mut self, exec_scopes: &ExecutionScopes) -> Result<(), VirtualMachineError> {
        self.verify_auto_deductions()?;
        match exec_scopes.data.len() {
            1 => Ok(()),
            _ => Err(ExecScopeError::NoScopeError.into()),
        }
    }

    ///Adds a new segment and to the VirtualMachine.memory returns its starting location as a RelocatableValue.
    pub fn add_memory_segment(&mut self) -> Relocatable {
        self.segments.add(&mut self.memory)
    }

    pub fn get_ap(&self) -> Relocatable {
        self.run_context.get_ap()
    }

    pub fn get_fp(&self) -> Relocatable {
        self.run_context.get_fp()
    }

    pub fn get_pc(&self) -> &Relocatable {
        self.run_context.get_pc()
    }

    pub fn get_prime(&self) -> &BigInt {
        &self.prime
    }

    ///Gets the integer value corresponding to the Relocatable address
    pub fn get_integer(&self, key: &Relocatable) -> Result<Cow<BigInt>, VirtualMachineError> {
        self.memory.get_integer(key)
    }

    ///Gets the relocatable value corresponding to the Relocatable address
    pub fn get_relocatable(
        &self,
        key: &Relocatable,
    ) -> Result<Cow<Relocatable>, VirtualMachineError> {
        self.memory.get_relocatable(key)
    }

    ///Gets a MaybeRelocatable value from memory indicated by a generic address
    pub fn get_maybe<'a, 'b: 'a, K: 'a>(
        &'b self,
        key: &'a K,
    ) -> Result<Option<MaybeRelocatable>, MemoryError>
    where
        Relocatable: TryFrom<&'a K>,
    {
        match self.memory.get(key) {
            Ok(Some(cow)) => Ok(Some(cow.into_owned())),
            Ok(None) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Returns a reference to the vector with all builtins present in the virtual machine
    pub fn get_builtin_runners(&self) -> &Vec<(String, BuiltinRunner)> {
        &self.builtin_runners
    }

    pub fn get_builtin_runners_as_mut(&mut self) -> &mut Vec<(String, BuiltinRunner)> {
        &mut self.builtin_runners
    }

    ///Inserts a value into a memory address given by a Relocatable value
    pub fn insert_value<T: Into<MaybeRelocatable>>(
        &mut self,
        key: &Relocatable,
        val: T,
    ) -> Result<(), VirtualMachineError> {
        self.memory.insert_value(key, val)
    }

    ///Writes data into the memory at address ptr and returns the first address after the data.
    pub fn load_data(
        &mut self,
        ptr: &MaybeRelocatable,
        data: Vec<MaybeRelocatable>,
    ) -> Result<MaybeRelocatable, MemoryError> {
        self.segments.load_data(&mut self.memory, ptr, data)
    }

    /// Writes args into the memory at address ptr and returns the first address after the data.
    /// Perfroms modulo on each element
    pub fn write_arg(
        &mut self,
        ptr: &Relocatable,
        arg: &dyn Any,
    ) -> Result<MaybeRelocatable, MemoryError> {
        self.segments
            .write_arg(&mut self.memory, ptr, arg, Some(&self.prime))
    }

    ///Gets `n_ret` return values from memory
    pub fn get_return_values(&self, n_ret: usize) -> Result<Vec<MaybeRelocatable>, MemoryError> {
        let addr = &self
            .run_context
            .get_ap()
            .sub(n_ret)
            .map_err(|_| MemoryError::NumOutOfBounds)?;
        self.memory.get_continuous_range(&addr.into(), n_ret)
    }

    ///Gets n elements from memory starting from addr (n being size)
    pub fn get_range(
        &self,
        addr: &MaybeRelocatable,
        size: usize,
    ) -> Result<Vec<Option<Cow<MaybeRelocatable>>>, MemoryError> {
        self.memory.get_range(addr, size)
    }

    ///Gets n elements from memory starting from addr (n being size)
    pub fn get_continuous_range(
        &self,
        addr: &MaybeRelocatable,
        size: usize,
    ) -> Result<Vec<MaybeRelocatable>, MemoryError> {
        self.memory.get_continuous_range(addr, size)
    }

    ///Gets n integer values from memory starting from addr (n being size),
    pub fn get_integer_range(
        &self,
        addr: &Relocatable,
        size: usize,
    ) -> Result<Vec<Cow<BigInt>>, VirtualMachineError> {
        self.memory.get_integer_range(addr, size)
    }

    pub fn get_range_check_builtin(&self) -> Result<&RangeCheckBuiltinRunner, VirtualMachineError> {
        for (name, builtin) in &self.builtin_runners {
            if name == &String::from("range_check") {
                if let BuiltinRunner::RangeCheck(range_check_builtin) = builtin {
                    return Ok(range_check_builtin);
                };
            }
        }
        Err(VirtualMachineError::NoRangeCheckBuiltin)
    }

    pub fn get_signature_builtin(
        &mut self,
    ) -> Result<&mut SignatureBuiltinRunner, VirtualMachineError> {
        for (name, builtin) in self.get_builtin_runners_as_mut() {
            if name == &String::from("ecdsa") {
                if let BuiltinRunner::Signature(signature_builtin) = builtin {
                    return Ok(signature_builtin);
                };
            }
        }

        Err(VirtualMachineError::NoSignatureBuiltin)
    }
    pub fn disable_trace(&mut self) {
        self.trace = None
    }

    #[doc(hidden)]
    pub fn set_ap(&mut self, ap: usize) {
        self.run_context.set_ap(ap)
    }

    #[doc(hidden)]
    pub fn set_fp(&mut self, fp: usize) {
        self.run_context.set_fp(fp)
    }

    #[doc(hidden)]
    pub fn set_pc(&mut self, pc: Relocatable) {
        self.run_context.set_pc(pc)
    }

    pub fn get_segment_used_size(&self, index: usize) -> Option<usize> {
        self.segments.get_segment_used_size(index)
    }

    pub fn add_temporary_segment(&mut self) -> Relocatable {
        self.segments.add_temporary_segment(&mut self.memory)
    }

    /// Add a new relocation rule.
    ///
    /// Will return an error if any of the following conditions are not met:
    ///   - Source address's segment must be negative (temporary).
    ///   - Source address's offset must be zero.
    ///   - There shouldn't already be relocation at the source segment.
    pub fn add_relocation_rule(
        &mut self,
        src_ptr: Relocatable,
        dst_ptr: Relocatable,
    ) -> Result<(), MemoryError> {
        self.memory.add_relocation_rule(src_ptr, dst_ptr)
    }

    pub fn gen_typed_args(
        &self,
        args: Vec<&dyn Any>,
    ) -> Result<Vec<MaybeRelocatable>, VirtualMachineError> {
        self.segments.gen_typed_args(args, self)
    }

    pub fn gen_arg(
        &mut self,
        arg: &dyn Any,
        prime: Option<&BigInt>,
    ) -> Result<MaybeRelocatable, VirtualMachineError> {
        self.segments.gen_arg(arg, prime, &mut self.memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        any_box, bigint_str,
        hint_processor::builtin_hint_processor::builtin_hint_processor_definition::{
            BuiltinHintProcessor, HintProcessorData,
        },
        relocatable,
        types::{
            instance_definitions::{
                bitwise_instance_def::BitwiseInstanceDef, ec_op_instance_def::EcOpInstanceDef,
            },
            instruction::{Op1Addr, Register},
            relocatable::Relocatable,
        },
        utils::test_utils::*,
        vm::{
            errors::{memory_errors::MemoryError, runner_errors::RunnerError},
            runners::builtin_runner::{BitwiseBuiltinRunner, EcOpBuiltinRunner, HashBuiltinRunner},
        },
    };

    use crate::bigint;
    use num_bigint::Sign;
    use std::collections::HashSet;

    from_bigint_str![18, 75, 76];

    #[test]
    fn get_instruction_encoding_successful_without_imm() {
        let mut vm = vm!();
        vm.memory = memory![((0, 0), 5)];
        assert_eq!((bigint!(5), None), {
            let value = vm.get_instruction_encoding().unwrap();
            (value.0.into_owned(), value.1)
        });
    }

    #[test]
    fn get_instruction_encoding_successful_with_imm() {
        let mut vm = vm!();

        vm.memory = memory![((0, 0), 5), ((0, 1), 6)];

        let (num, imm) = vm
            .get_instruction_encoding()
            .expect("Unexpected error on get_instruction_encoding");
        assert_eq!(num.as_ref(), &bigint!(5));
        assert_eq!(
            imm.map(Cow::into_owned),
            Some(MaybeRelocatable::Int(bigint!(6)))
        );
    }

    #[test]
    fn get_instruction_encoding_unsuccesful() {
        let vm = vm!();
        assert_eq!(
            vm.get_instruction_encoding(),
            Err(VirtualMachineError::InvalidInstructionEncoding)
        );
    }

    #[test]
    fn update_fp_ap_plus2() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::APPlus2,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        run_context!(vm, 4, 5, 6);

        assert_eq!(Ok(()), vm.update_fp(&instruction, &operands));
        assert_eq!(vm.run_context.fp, 7)
    }

    #[test]
    fn update_fp_dst() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Dst,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: mayberelocatable!(1, 6),
            res: Some(mayberelocatable!(8)),
            op0: mayberelocatable!(9),
            op1: mayberelocatable!(10),
        };

        let mut vm = vm!();

        assert_eq!(Ok(()), vm.update_fp(&instruction, &operands));
        assert_eq!(vm.run_context.fp, 6)
    }

    #[test]
    fn update_fp_regular() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();

        assert_eq!(Ok(()), vm.update_fp(&instruction, &operands));
        assert_eq!(vm.run_context.fp, 0)
    }

    #[test]
    fn update_fp_dst_num() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Dst,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        run_context!(vm, 4, 5, 6);

        assert_eq!(Ok(()), vm.update_fp(&instruction, &operands));
        assert_eq!(vm.run_context.fp, 11)
    }

    #[test]
    fn update_ap_add_with_res() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Add,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = VirtualMachine::new(bigint!(39), false, Vec::new());
        vm.run_context.pc = Relocatable::from((0, 4));
        vm.run_context.ap = 5;
        vm.run_context.fp = 6;

        assert_eq!(Ok(()), vm.update_ap(&instruction, &operands));
        assert_eq!(vm.run_context.ap, 13);
    }

    #[test]
    fn update_ap_add_without_res() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Add,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: None,
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        vm.run_context.pc = Relocatable::from((0, 4));
        vm.run_context.ap = 5;
        vm.run_context.fp = 6;

        assert_eq!(
            vm.update_ap(&instruction, &operands),
            Err(VirtualMachineError::UnconstrainedResAdd)
        );
    }

    #[test]
    fn update_ap_add1() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Add1,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        vm.run_context.pc = Relocatable::from((0, 4));
        vm.run_context.ap = 5;
        vm.run_context.fp = 6;

        assert_eq!(Ok(()), vm.update_ap(&instruction, &operands));
        assert_eq!(vm.run_context.ap, 6);
    }

    #[test]
    fn update_ap_add2() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Add2,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        vm.run_context.pc = Relocatable::from((0, 4));
        vm.run_context.ap = 5;
        vm.run_context.fp = 6;

        assert_eq!(Ok(()), vm.update_ap(&instruction, &operands));
        assert_eq!(vm.run_context.ap, 7);
    }

    #[test]
    fn update_ap_regular() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        vm.run_context.pc = Relocatable::from((0, 4));
        vm.run_context.ap = 5;
        vm.run_context.fp = 6;

        assert_eq!(Ok(()), vm.update_ap(&instruction, &operands));
        assert_eq!(vm.run_context.ap, 5);
    }

    #[test]
    fn update_pc_regular_instruction_no_imm() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();

        assert_eq!(Ok(()), vm.update_pc(&instruction, &operands));
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 1)));
    }

    #[test]
    fn update_pc_regular_instruction_has_imm() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: Some(bigint!(5)),
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();

        assert_eq!(Ok(()), vm.update_pc(&instruction, &operands));
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 2)));
    }

    #[test]
    fn update_pc_jump_with_res() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: mayberelocatable!(1, 11),
            res: Some(mayberelocatable!(0, 8)),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();

        assert_eq!(Ok(()), vm.update_pc(&instruction, &operands));
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 8)));
    }

    #[test]
    fn update_pc_jump_without_res() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: None,
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        vm.run_context.pc = Relocatable::from((0, 4));
        vm.run_context.ap = 5;
        vm.run_context.fp = 6;

        assert_eq!(
            vm.update_pc(&instruction, &operands),
            Err(VirtualMachineError::UnconstrainedResJump)
        );
    }

    #[test]
    fn update_pc_jump_rel_with_int_res() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::JumpRel,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        run_context!(vm, 1, 1, 1);

        assert_eq!(Ok(()), vm.update_pc(&instruction, &operands));
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 9)));
    }

    #[test]
    fn update_pc_jump_rel_without_res() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::JumpRel,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: None,
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();

        assert_eq!(
            vm.update_pc(&instruction, &operands),
            Err(VirtualMachineError::UnconstrainedResJumpRel)
        );
    }

    #[test]
    fn update_pc_jump_rel_with_non_int_res() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::JumpRel,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::from((1, 4))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();

        assert_eq!(
            Err(VirtualMachineError::PureValue),
            vm.update_pc(&instruction, &operands)
        );
    }

    #[test]
    fn update_pc_jnz_dst_is_zero() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jnz,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(0)),
            res: Some(MaybeRelocatable::Int(bigint!(0))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();

        assert_eq!(Ok(()), vm.update_pc(&instruction, &operands));
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 1)));
    }

    #[test]
    fn update_pc_jnz_dst_is_not_zero() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jnz,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();

        assert_eq!(Ok(()), vm.update_pc(&instruction, &operands));
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 10)));
    }

    #[test]
    fn update_registers_all_regular() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        vm.run_context.pc = Relocatable::from((0, 4));
        vm.run_context.ap = 5;
        vm.run_context.fp = 6;

        assert_eq!(Ok(()), vm.update_registers(instruction, operands));
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 5)));
        assert_eq!(vm.run_context.ap, 5);
        assert_eq!(vm.run_context.fp, 6);
    }

    #[test]
    fn update_registers_mixed_types() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::JumpRel,
            ap_update: ApUpdate::Add2,
            fp_update: FpUpdate::Dst,
            opcode: Opcode::NOp,
        };

        let operands = Operands {
            dst: MaybeRelocatable::from((1, 11)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let mut vm = vm!();
        run_context!(vm, 4, 5, 6);

        assert_eq!(Ok(()), vm.update_registers(instruction, operands));
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 12)));
        assert_eq!(vm.run_context.ap, 7);
        assert_eq!(vm.run_context.fp, 11);
    }

    #[test]
    fn is_zero_int_value() {
        let value = MaybeRelocatable::Int(bigint!(1));
        assert_eq!(Ok(false), VirtualMachine::is_zero(&value));
    }

    #[test]
    fn is_zero_relocatable_value() {
        let value = MaybeRelocatable::from((1, 2));
        assert_eq!(
            Err(VirtualMachineError::PureValue),
            VirtualMachine::is_zero(&value)
        );
    }

    #[test]
    fn is_zero_relocatable_value_negative() {
        let value = MaybeRelocatable::from((1, 1));
        assert_eq!(
            Err(VirtualMachineError::PureValue),
            VirtualMachine::is_zero(&value)
        );
    }

    #[test]
    fn deduce_op0_opcode_call() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::Call,
        };

        let vm = vm!();

        assert_eq!(
            Ok((Some(MaybeRelocatable::from((0, 1))), None)),
            vm.deduce_op0(&instruction, None, None)
        );
    }

    #[test]
    fn deduce_op0_opcode_assert_eq_res_add_with_optionals() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let dst = MaybeRelocatable::Int(bigint!(3));
        let op1 = MaybeRelocatable::Int(bigint!(2));
        assert_eq!(
            Ok((
                Some(MaybeRelocatable::Int(bigint!(1))),
                Some(MaybeRelocatable::Int(bigint!(3)))
            )),
            vm.deduce_op0(&instruction, Some(&dst), Some(&op1))
        );
    }

    #[test]
    fn deduce_op0_opcode_assert_eq_res_add_without_optionals() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        assert_eq!(Ok((None, None)), vm.deduce_op0(&instruction, None, None));
    }

    #[test]
    fn deduce_op0_opcode_assert_eq_res_mul_non_zero_op1() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Mul,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let dst = MaybeRelocatable::Int(bigint!(4));
        let op1 = MaybeRelocatable::Int(bigint!(2));
        assert_eq!(
            Ok((
                Some(MaybeRelocatable::Int(bigint!(2))),
                Some(MaybeRelocatable::Int(bigint!(4)))
            )),
            vm.deduce_op0(&instruction, Some(&dst), Some(&op1))
        );
    }

    #[test]
    fn deduce_op0_opcode_assert_eq_res_mul_zero_op1() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Mul,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let dst = MaybeRelocatable::Int(bigint!(4));
        let op1 = MaybeRelocatable::Int(bigint!(0));
        assert_eq!(
            Ok((None, None)),
            vm.deduce_op0(&instruction, Some(&dst), Some(&op1))
        );
    }

    #[test]
    fn deduce_op0_opcode_assert_eq_res_op1() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Op1,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let dst = MaybeRelocatable::Int(bigint!(4));
        let op1 = MaybeRelocatable::Int(bigint!(0));
        assert_eq!(
            Ok((None, None)),
            vm.deduce_op0(&instruction, Some(&dst), Some(&op1))
        );
    }

    #[test]
    fn deduce_op0_opcode_ret() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Mul,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::Ret,
        };

        let vm = vm!();

        let dst = MaybeRelocatable::Int(bigint!(4));
        let op1 = MaybeRelocatable::Int(bigint!(0));
        assert_eq!(
            Ok((None, None)),
            vm.deduce_op0(&instruction, Some(&dst), Some(&op1))
        );
    }

    #[test]
    fn deduce_op1_opcode_call() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::Call,
        };

        let vm = vm!();

        assert_eq!(Ok((None, None)), vm.deduce_op1(&instruction, None, None));
    }

    #[test]
    fn deduce_op1_opcode_assert_eq_res_add_with_optionals() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let dst = MaybeRelocatable::Int(bigint!(3));
        let op0 = MaybeRelocatable::Int(bigint!(2));
        assert_eq!(
            Ok((
                Some(MaybeRelocatable::Int(bigint!(1))),
                Some(MaybeRelocatable::Int(bigint!(3)))
            )),
            vm.deduce_op1(&instruction, Some(&dst), Some(op0))
        );
    }

    #[test]
    fn deduce_op1_opcode_assert_eq_res_add_without_optionals() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        assert_eq!(Ok((None, None)), vm.deduce_op1(&instruction, None, None));
    }

    #[test]
    fn deduce_op1_opcode_assert_eq_res_mul_non_zero_op0() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Mul,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let dst = MaybeRelocatable::Int(bigint!(4));
        let op0 = MaybeRelocatable::Int(bigint!(2));
        assert_eq!(
            Ok((
                Some(MaybeRelocatable::Int(bigint!(2))),
                Some(MaybeRelocatable::Int(bigint!(4)))
            )),
            vm.deduce_op1(&instruction, Some(&dst), Some(op0))
        );
    }

    #[test]
    fn deduce_op1_opcode_assert_eq_res_mul_zero_op0() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Mul,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let dst = MaybeRelocatable::Int(bigint!(4));
        let op0 = MaybeRelocatable::Int(bigint!(0));
        assert_eq!(
            Ok((None, None)),
            vm.deduce_op1(&instruction, Some(&dst), Some(op0))
        );
    }

    #[test]
    fn deduce_op1_opcode_assert_eq_res_op1_without_dst() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Op1,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let op0 = MaybeRelocatable::Int(bigint!(0));
        assert_eq!(
            Ok((None, None)),
            vm.deduce_op1(&instruction, None, Some(op0))
        );
    }

    #[test]
    fn deduce_op1_opcode_assert_eq_res_op1_with_dst() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Op1,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let dst = MaybeRelocatable::Int(bigint!(7));
        assert_eq!(
            Ok((
                Some(MaybeRelocatable::Int(bigint!(7))),
                Some(MaybeRelocatable::Int(bigint!(7)))
            )),
            vm.deduce_op1(&instruction, Some(&dst), None)
        );
    }

    #[test]
    fn compute_res_op1() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Op1,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let op1 = MaybeRelocatable::Int(bigint!(7));
        let op0 = MaybeRelocatable::Int(bigint!(9));
        assert_eq!(
            Ok(Some(MaybeRelocatable::Int(bigint!(7)))),
            vm.compute_res(&instruction, &op0, &op1)
        );
    }

    #[test]
    fn compute_res_add() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let op1 = MaybeRelocatable::Int(bigint!(7));
        let op0 = MaybeRelocatable::Int(bigint!(9));
        assert_eq!(
            Ok(Some(MaybeRelocatable::Int(bigint!(16)))),
            vm.compute_res(&instruction, &op0, &op1)
        );
    }

    #[test]
    fn compute_res_mul_int_operands() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Mul,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let op1 = MaybeRelocatable::Int(bigint!(7));
        let op0 = MaybeRelocatable::Int(bigint!(9));
        assert_eq!(
            Ok(Some(MaybeRelocatable::Int(bigint!(63)))),
            vm.compute_res(&instruction, &op0, &op1)
        );
    }

    #[test]
    fn compute_res_mul_relocatable_values() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Mul,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let op1 = MaybeRelocatable::from((2, 3));
        let op0 = MaybeRelocatable::from((2, 6));
        assert_eq!(
            Err(VirtualMachineError::PureValue),
            vm.compute_res(&instruction, &op0, &op1)
        );
    }

    #[test]
    fn compute_res_unconstrained() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Unconstrained,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let op1 = MaybeRelocatable::Int(bigint!(7));
        let op0 = MaybeRelocatable::Int(bigint!(9));
        assert_eq!(Ok(None), vm.compute_res(&instruction, &op0, &op1));
    }

    #[test]
    fn deduce_dst_opcode_assert_eq_with_res() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Unconstrained,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        let res = MaybeRelocatable::Int(bigint!(7));
        assert_eq!(
            Some(MaybeRelocatable::Int(bigint!(7))),
            vm.deduce_dst(&instruction, Some(&res))
        );
    }

    #[test]
    fn deduce_dst_opcode_assert_eq_without_res() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Unconstrained,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let vm = vm!();

        assert_eq!(None, vm.deduce_dst(&instruction, None));
    }

    #[test]
    fn deduce_dst_opcode_call() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Unconstrained,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::Call,
        };

        let vm = vm!();

        assert_eq!(
            Some(MaybeRelocatable::from((1, 0))),
            vm.deduce_dst(&instruction, None)
        );
    }

    #[test]
    fn deduce_dst_opcode_ret() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Unconstrained,
            pc_update: PcUpdate::Jump,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::Ret,
        };

        let vm = vm!();

        assert_eq!(None, vm.deduce_dst(&instruction, None));
    }

    #[test]
    fn compute_operands_add_ap() {
        let inst = Instruction {
            off0: 0,
            off1: 1,
            off2: 2,
            imm: None,
            dst_register: Register::AP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let mut vm = vm!();
        vm.accessed_addresses = Some(Vec::new());
        for _ in 0..2 {
            vm.segments.add(&mut vm.memory);
        }

        vm.memory.data.push(Vec::new());
        let dst_addr = MaybeRelocatable::from((1, 0));
        let dst_addr_value = MaybeRelocatable::Int(bigint!(5));
        let op0_addr = MaybeRelocatable::from((1, 1));
        let op0_addr_value = MaybeRelocatable::Int(bigint!(2));
        let op1_addr = MaybeRelocatable::from((1, 2));
        let op1_addr_value = MaybeRelocatable::Int(bigint!(3));
        vm.memory.insert(&dst_addr, &dst_addr_value).unwrap();
        vm.memory.insert(&op0_addr, &op0_addr_value).unwrap();
        vm.memory.insert(&op1_addr, &op1_addr_value).unwrap();

        let expected_operands = Operands {
            dst: dst_addr_value.clone(),
            res: Some(dst_addr_value.clone()),
            op0: op0_addr_value.clone(),
            op1: op1_addr_value.clone(),
        };

        let expected_addresses = Some(OperandsAddresses(
            dst_addr.get_relocatable().unwrap().clone(),
            op0_addr.get_relocatable().unwrap().clone(),
            op1_addr.get_relocatable().unwrap().clone(),
        ));

        let (operands, addresses) = vm.compute_operands(&inst).unwrap();
        assert!(operands == expected_operands);
        assert!(addresses == expected_addresses);
    }

    #[test]
    fn compute_operands_mul_fp() {
        let inst = Instruction {
            off0: 0,
            off1: 1,
            off2: 2,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::FP,
            op1_addr: Op1Addr::FP,
            res: Res::Mul,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };
        let mut vm = vm!();
        //Create program and execution segments
        for _ in 0..2 {
            vm.segments.add(&mut vm.memory);
        }
        vm.accessed_addresses = Some(Vec::new());
        vm.memory.data.push(Vec::new());
        let dst_addr = mayberelocatable!(1, 0);
        let dst_addr_value = mayberelocatable!(6);
        let op0_addr = mayberelocatable!(1, 1);
        let op0_addr_value = mayberelocatable!(2);
        let op1_addr = mayberelocatable!(1, 2);
        let op1_addr_value = mayberelocatable!(3);
        vm.memory.insert(&dst_addr, &dst_addr_value).unwrap();
        vm.memory.insert(&op0_addr, &op0_addr_value).unwrap();
        vm.memory.insert(&op1_addr, &op1_addr_value).unwrap();

        let expected_operands = Operands {
            dst: dst_addr_value.clone(),
            res: Some(dst_addr_value.clone()),
            op0: op0_addr_value.clone(),
            op1: op1_addr_value.clone(),
        };

        let expected_addresses = Some(OperandsAddresses(
            dst_addr.get_relocatable().unwrap().clone(),
            op0_addr.get_relocatable().unwrap().clone(),
            op1_addr.get_relocatable().unwrap().clone(),
        ));

        let (operands, addresses) = vm.compute_operands(&inst).unwrap();
        assert!(operands == expected_operands);
        assert!(addresses == expected_addresses);
    }

    #[test]
    fn compute_jnz() {
        let instruction = Instruction {
            off0: 1,
            off1: 1,
            off2: 1,
            imm: Some(bigint!(4)),
            dst_register: Register::AP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::Imm,
            res: Res::Unconstrained,
            pc_update: PcUpdate::Jnz,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let mut vm = vm!();
        vm.accessed_addresses = Some(Vec::new());
        vm.memory = memory![
            ((0, 0), 0x206800180018001_i64),
            ((1, 1), 0x4),
            ((0, 1), 0x4)
        ];

        let expected_operands = Operands {
            dst: mayberelocatable!(4),
            res: None,
            op0: mayberelocatable!(4),
            op1: mayberelocatable!(4),
        };

        let expected_addresses = Some(OperandsAddresses(
            relocatable!(1, 1),
            relocatable!(1, 1),
            relocatable!(0, 1),
        ));

        let (operands, addresses) = vm.compute_operands(&instruction).unwrap();
        assert!(operands == expected_operands);
        assert!(addresses == expected_addresses);
        let hint_processor = BuiltinHintProcessor::new_empty();
        assert_eq!(
            vm.step(
                &hint_processor,
                exec_scopes_ref!(),
                &HashMap::new(),
                &HashMap::new()
            ),
            Ok(())
        );
        assert_eq!(vm.run_context.pc, relocatable!(0, 4));
    }

    #[test]
    fn compute_operands_deduce_dst_none() {
        let instruction = Instruction {
            off0: 2,
            off1: 0,
            off2: 0,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Unconstrained,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::NOp,
        };

        let mut vm = vm!();

        vm.memory = memory!(((1, 0), 145944781867024385_i64));

        let error = vm.compute_operands(&instruction);
        assert_eq!(error, Err(VirtualMachineError::NoDst));
    }

    #[test]
    fn opcode_assertions_res_unconstrained() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::APPlus2,
            opcode: Opcode::AssertEq,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(8)),
            res: None,
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let vm = vm!();

        let error = vm.opcode_assertions(&instruction, &operands);
        assert_eq!(error, Err(VirtualMachineError::UnconstrainedResAssertEq));
    }

    #[test]
    fn opcode_assertions_instruction_failed() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::APPlus2,
            opcode: Opcode::AssertEq,
        };

        let operands = Operands {
            dst: MaybeRelocatable::Int(bigint!(9)),
            res: Some(MaybeRelocatable::Int(bigint!(8))),
            op0: MaybeRelocatable::Int(bigint!(9)),
            op1: MaybeRelocatable::Int(bigint!(10)),
        };

        let vm = vm!();

        assert_eq!(
            vm.opcode_assertions(&instruction, &operands),
            Err(VirtualMachineError::DiffAssertValues(
                bigint!(9),
                bigint!(8)
            ))
        );
    }

    #[test]
    fn opcode_assertions_inconsistent_op0() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::APPlus2,
            opcode: Opcode::Call,
        };

        let operands = Operands {
            dst: mayberelocatable!(0, 8),
            res: Some(mayberelocatable!(8)),
            op0: mayberelocatable!(9),
            op1: mayberelocatable!(10),
        };

        let mut vm = vm!();
        vm.run_context.pc = relocatable!(0, 4);

        assert_eq!(
            vm.opcode_assertions(&instruction, &operands),
            Err(VirtualMachineError::CantWriteReturnPc(
                mayberelocatable!(9),
                mayberelocatable!(0, 5),
            ))
        );
    }

    #[test]
    fn opcode_assertions_inconsistent_dst() {
        let instruction = Instruction {
            off0: 1,
            off1: 2,
            off2: 3,
            imm: None,
            dst_register: Register::FP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::APPlus2,
            opcode: Opcode::Call,
        };

        let operands = Operands {
            dst: mayberelocatable!(8),
            res: Some(mayberelocatable!(8)),
            op0: mayberelocatable!(0, 1),
            op1: mayberelocatable!(10),
        };
        let mut vm = vm!();
        vm.run_context.fp = 6;

        assert_eq!(
            vm.opcode_assertions(&instruction, &operands),
            Err(VirtualMachineError::CantWriteReturnFp(
                mayberelocatable!(8),
                mayberelocatable!(1, 6)
            ))
        );
    }

    #[test]
    /// Test for a simple program execution
    /// Used program code:
    /// func main():
    ///     let a = 1
    ///     let b = 2
    ///     let c = a + b
    ///     return()
    /// end
    /// Memory taken from original vm
    /// {RelocatableValue(segment_index=0, offset=0): 2345108766317314046,
    ///  RelocatableValue(segment_index=1, offset=0): RelocatableValue(segment_index=2, offset=0),
    ///  RelocatableValue(segment_index=1, offset=1): RelocatableValue(segment_index=3, offset=0)}
    /// Current register values:
    /// AP 1:2
    /// FP 1:2
    /// PC 0:0
    fn test_step_for_preset_memory() {
        let mut vm = vm!(true);
        vm.accessed_addresses = Some(Vec::new());

        let hint_processor = BuiltinHintProcessor::new_empty();

        run_context!(vm, 0, 2, 2);

        vm.memory = memory![
            ((0, 0), 2345108766317314046_u64),
            ((1, 0), (2, 0)),
            ((1, 1), (3, 0))
        ];

        assert_eq!(
            vm.step(
                &hint_processor,
                exec_scopes_ref!(),
                &HashMap::new(),
                &HashMap::new()
            ),
            Ok(())
        );
        let trace = vm.trace.unwrap();
        trace_check!(trace, [((0, 0), (1, 2), (1, 2))]);

        assert_eq!(vm.run_context.pc, Relocatable::from((3, 0)));
        assert_eq!(vm.run_context.ap, 2);
        assert_eq!(vm.run_context.fp, 0);

        let accessed_addresses = vm.accessed_addresses.as_ref().unwrap();
        assert!(accessed_addresses.contains(&Relocatable::from((1, 0))));
        assert!(accessed_addresses.contains(&Relocatable::from((1, 1))));
        assert!(accessed_addresses.contains(&Relocatable::from((0, 0))));
    }

    #[test]
    /*
    Test for a simple program execution
    Used program code:
        func myfunc(a: felt) -> (r: felt):
            let b = a * 2
            return(b)
        end
        func main():
            let a = 1
            let b = myfunc(a)
            return()
        end
    Memory taken from original vm:
    {RelocatableValue(segment_index=0, offset=0): 5207990763031199744,
    RelocatableValue(segment_index=0, offset=1): 2,
    RelocatableValue(segment_index=0, offset=2): 2345108766317314046,
    RelocatableValue(segment_index=0, offset=3): 5189976364521848832,
    RelocatableValue(segment_index=0, offset=4): 1,
    RelocatableValue(segment_index=0, offset=5): 1226245742482522112,
    RelocatableValue(segment_index=0, offset=6): 3618502788666131213697322783095070105623107215331596699973092056135872020476,
    RelocatableValue(segment_index=0, offset=7): 2345108766317314046,
    RelocatableValue(segment_index=1, offset=0): RelocatableValue(segment_index=2, offset=0),
    RelocatableValue(segment_index=1, offset=1): RelocatableValue(segment_index=3, offset=0)}
    Current register values:
    AP 1:2
    FP 1:2
    PC 0:3
    Final Pc (not executed): 3:0
    This program consists of 5 steps
    */
    fn test_step_for_preset_memory_function_call() {
        let mut vm = vm!(true);
        vm.accessed_addresses = Some(Vec::new());

        run_context!(vm, 3, 2, 2);

        //Insert values into memory
        vm.memory =
            memory![
            ((0, 0), 5207990763031199744_i64),
            ((0, 1), 2),
            ((0, 2), 2345108766317314046_i64),
            ((0, 3), 5189976364521848832_i64),
            ((0, 4), 1),
            ((0, 5), 1226245742482522112_i64),
            (
                (0, 6),
                (b"3618502788666131213697322783095070105623107215331596699973092056135872020476",10)
            ),
            ((0, 7), 2345108766317314046_i64),
            ((1, 0), (2, 0)),
            ((1, 1), (3, 0))
        ];

        let final_pc = Relocatable::from((3, 0));
        let hint_processor = BuiltinHintProcessor::new_empty();
        //Run steps
        while vm.run_context.pc != final_pc {
            assert_eq!(
                vm.step(
                    &hint_processor,
                    exec_scopes_ref!(),
                    &HashMap::new(),
                    &HashMap::new()
                ),
                Ok(())
            );
        }

        //Check final register values
        assert_eq!(vm.run_context.pc, Relocatable::from((3, 0)));

        assert_eq!(vm.run_context.ap, 6);

        assert_eq!(vm.run_context.fp, 0);
        //Check each TraceEntry in trace
        let trace = vm.trace.unwrap();
        assert_eq!(trace.len(), 5);
        trace_check!(
            trace,
            [
                ((0, 3), (1, 2), (1, 2)),
                ((0, 5), (1, 3), (1, 2)),
                ((0, 0), (1, 5), (1, 5)),
                ((0, 2), (1, 6), (1, 5)),
                ((0, 7), (1, 6), (1, 2))
            ]
        );
        //Check accessed_addresses
        //Order will differ from python vm execution, (due to python version using set's update() method)
        //We will instead check that all elements are contained and not duplicated
        let accessed_addresses = vm
            .accessed_addresses
            .unwrap()
            .into_iter()
            .collect::<HashSet<Relocatable>>();
        assert_eq!(accessed_addresses.len(), 14);
        //Check each element individually
        assert!(accessed_addresses.contains(&Relocatable::from((0, 1))));
        assert!(accessed_addresses.contains(&Relocatable::from((0, 7))));
        assert!(accessed_addresses.contains(&Relocatable::from((1, 2))));
        assert!(accessed_addresses.contains(&Relocatable::from((0, 4))));
        assert!(accessed_addresses.contains(&Relocatable::from((0, 0))));
        assert!(accessed_addresses.contains(&Relocatable::from((1, 5))));
        assert!(accessed_addresses.contains(&Relocatable::from((1, 1))));
        assert!(accessed_addresses.contains(&Relocatable::from((0, 3))));
        assert!(accessed_addresses.contains(&Relocatable::from((1, 4))));
        assert!(accessed_addresses.contains(&Relocatable::from((0, 6))));
        assert!(accessed_addresses.contains(&Relocatable::from((0, 2))));
        assert!(accessed_addresses.contains(&Relocatable::from((0, 5))));
        assert!(accessed_addresses.contains(&Relocatable::from((1, 0))));
        assert!(accessed_addresses.contains(&Relocatable::from((1, 3))));
    }

    #[test]
    /// Test the following program:
    /// ...
    /// [ap] = 4
    /// ap += 1
    /// [ap] = 5; ap++
    /// [ap] = [ap - 1] * [ap - 2]
    /// ...
    /// Original vm memory:
    /// RelocatableValue(segment_index=0, offset=0): '0x400680017fff8000',
    /// RelocatableValue(segment_index=0, offset=1): '0x4',
    /// RelocatableValue(segment_index=0, offset=2): '0x40780017fff7fff',
    /// RelocatableValue(segment_index=0, offset=3): '0x1',
    /// RelocatableValue(segment_index=0, offset=4): '0x480680017fff8000',
    /// RelocatableValue(segment_index=0, offset=5): '0x5',
    /// RelocatableValue(segment_index=0, offset=6): '0x40507ffe7fff8000',
    /// RelocatableValue(segment_index=0, offset=7): '0x208b7fff7fff7ffe',
    /// RelocatableValue(segment_index=1, offset=0): RelocatableValue(segment_index=2, offset=0),
    /// RelocatableValue(segment_index=1, offset=1): RelocatableValue(segment_index=3, offset=0),
    /// RelocatableValue(segment_index=1, offset=2): '0x4',
    /// RelocatableValue(segment_index=1, offset=3): '0x5',
    /// RelocatableValue(segment_index=1, offset=4): '0x14'
    fn multiplication_and_different_ap_increase() {
        let mut vm = vm!();
        vm.memory = memory![
            ((0, 0), 0x400680017fff8000_i64),
            ((0, 1), 0x4),
            ((0, 2), 0x40780017fff7fff_i64),
            ((0, 3), 0x1),
            ((0, 4), 0x480680017fff8000_i64),
            ((0, 5), 0x5),
            ((0, 6), 0x40507ffe7fff8000_i64),
            ((0, 7), 0x208b7fff7fff7ffe_i64),
            ((1, 0), (2, 0)),
            ((1, 1), (3, 0)),
            ((1, 2), 0x4),
            ((1, 3), 0x5),
            ((1, 4), 0x14)
        ];

        run_context!(vm, 0, 2, 2);

        assert_eq!(vm.run_context.pc, Relocatable::from((0, 0)));
        assert_eq!(vm.run_context.ap, 2);
        let hint_processor = BuiltinHintProcessor::new_empty();
        assert_eq!(
            vm.step(
                &hint_processor,
                exec_scopes_ref!(),
                &HashMap::new(),
                &HashMap::new()
            ),
            Ok(())
        );
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 2)));
        assert_eq!(vm.run_context.ap, 2);

        assert_eq!(
            vm.memory
                .get(&vm.run_context.get_ap())
                .unwrap()
                .unwrap()
                .as_ref(),
            &MaybeRelocatable::Int(bigint!(0x4)),
        );
        let hint_processor = BuiltinHintProcessor::new_empty();
        assert_eq!(
            vm.step(
                &hint_processor,
                exec_scopes_ref!(),
                &HashMap::new(),
                &HashMap::new()
            ),
            Ok(())
        );
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 4)));
        assert_eq!(vm.run_context.ap, 3);

        assert_eq!(
            vm.memory
                .get(&vm.run_context.get_ap())
                .unwrap()
                .unwrap()
                .as_ref(),
            &MaybeRelocatable::Int(bigint!(0x5))
        );

        let hint_processor = BuiltinHintProcessor::new_empty();
        assert_eq!(
            vm.step(
                &hint_processor,
                exec_scopes_ref!(),
                &HashMap::new(),
                &HashMap::new()
            ),
            Ok(())
        );
        assert_eq!(vm.run_context.pc, Relocatable::from((0, 6)));
        assert_eq!(vm.run_context.ap, 4);

        assert_eq!(
            vm.memory
                .get(&vm.run_context.get_ap())
                .unwrap()
                .unwrap()
                .as_ref(),
            &MaybeRelocatable::Int(bigint!(0x14)),
        );
    }

    #[test]
    fn deduce_memory_cell_no_pedersen_builtin() {
        let mut vm = vm!();
        assert_eq!(vm.deduce_memory_cell(&Relocatable::from((0, 0))), Ok(None));
    }

    #[test]
    fn deduce_memory_cell_pedersen_builtin_valid() {
        let mut vm = vm!();
        let builtin = HashBuiltinRunner::new(8, true);
        vm.builtin_runners
            .push((String::from("pedersen"), builtin.into()));
        vm.memory = memory![((0, 3), 32), ((0, 4), 72), ((0, 5), 0)];
        assert_eq!(
            vm.deduce_memory_cell(&Relocatable::from((0, 5))),
            Ok(Some(MaybeRelocatable::from(bigint_str!(
                b"3270867057177188607814717243084834301278723532952411121381966378910183338911"
            ))))
        );
    }

    #[test]
    /* Program used:
    %builtins output pedersen
    from starkware.cairo.common.cairo_builtins import HashBuiltin
    from starkware.cairo.common.hash import hash2
    from starkware.cairo.common.serialize import serialize_word

    func foo(hash_ptr : HashBuiltin*) -> (
        hash_ptr : HashBuiltin*, z
    ):
        # Use a with-statement, since 'hash_ptr' is not an
        # implicit argument.
        with hash_ptr:
            let (z) = hash2(32, 72)
        end
        return (hash_ptr=hash_ptr, z=z)
    end

    func main{output_ptr: felt*, pedersen_ptr: HashBuiltin*}():
        let (pedersen_ptr, a) = foo(pedersen_ptr)
        serialize_word(a)
        return()
    end
     */
    fn compute_operands_pedersen() {
        let instruction = Instruction {
            off0: 0,
            off1: -5,
            off2: 2,
            imm: None,
            dst_register: Register::AP,
            op0_register: Register::FP,
            op1_addr: Op1Addr::Op0,
            res: Res::Op1,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Add1,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };
        let mut builtin = HashBuiltinRunner::new(8, true);
        builtin.base = 3;
        let mut vm = vm!();
        vm.accessed_addresses = Some(Vec::new());
        vm.builtin_runners
            .push((String::from("pedersen"), builtin.into()));
        run_context!(vm, 0, 13, 12);

        //Insert values into memory (excluding those from the program segment (instructions))
        vm.memory = memory![
            ((3, 0), 32),
            ((3, 1), 72),
            ((1, 0), (2, 0)),
            ((1, 1), (3, 0)),
            ((1, 2), (4, 0)),
            ((1, 3), (5, 0)),
            ((1, 4), (3, 0)),
            ((1, 5), (1, 4)),
            ((1, 6), (0, 21)),
            ((1, 7), (3, 0)),
            ((1, 8), 32),
            ((1, 9), 72),
            ((1, 10), (1, 7)),
            ((1, 11), (0, 17)),
            ((1, 12), (3, 3))
        ];

        let expected_operands = Operands {
            dst: MaybeRelocatable::from(bigint_str!(
                b"3270867057177188607814717243084834301278723532952411121381966378910183338911"
            )),
            res: Some(MaybeRelocatable::from(bigint_str!(
                b"3270867057177188607814717243084834301278723532952411121381966378910183338911"
            ))),
            op0: MaybeRelocatable::from((3, 0)),
            op1: MaybeRelocatable::from(bigint_str!(
                b"3270867057177188607814717243084834301278723532952411121381966378910183338911"
            )),
        };
        let expected_operands_mem_addresses = Some(OperandsAddresses(
            Relocatable::from((1, 13)),
            Relocatable::from((1, 7)),
            Relocatable::from((3, 2)),
        ));
        assert_eq!(
            Ok((expected_operands, expected_operands_mem_addresses)),
            vm.compute_operands(&instruction)
        );
    }

    #[test]
    fn deduce_memory_cell_bitwise_builtin_valid_and() {
        let mut vm = vm!();
        let builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);
        vm.builtin_runners
            .push((String::from("bitwise"), builtin.into()));
        vm.memory = memory![((0, 5), 10), ((0, 6), 12), ((0, 7), 0)];
        assert_eq!(
            vm.deduce_memory_cell(&Relocatable::from((0, 7))),
            Ok(Some(MaybeRelocatable::from(bigint!(8))))
        );
    }

    #[test]
    /* Program used:
    %builtins bitwise
    from starkware.cairo.common.bitwise import bitwise_and
    from starkware.cairo.common.cairo_builtins import BitwiseBuiltin


    func main{bitwise_ptr: BitwiseBuiltin*}():
        let (result) = bitwise_and(12, 10)  # Binary (1100, 1010).
        assert result = 8  # Binary 1000.
        return()
    end
    */
    fn compute_operands_bitwise() {
        let instruction = Instruction {
            off0: 0,
            off1: -5,
            off2: 2,
            imm: None,
            dst_register: Register::AP,
            op0_register: Register::FP,
            op1_addr: Op1Addr::Op0,
            res: Res::Op1,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Add1,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        let mut builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);
        builtin.base = 2;
        let mut vm = vm!();

        vm.accessed_addresses = Some(Vec::new());
        vm.builtin_runners
            .push((String::from("bitwise"), builtin.into()));
        run_context!(vm, 0, 9, 8);

        //Insert values into memory (excluding those from the program segment (instructions))
        vm.memory = memory![
            ((2, 0), 12),
            ((2, 1), 10),
            ((1, 0), (2, 0)),
            ((1, 1), (3, 0)),
            ((1, 2), (4, 0)),
            ((1, 3), (2, 0)),
            ((1, 4), 12),
            ((1, 5), 10),
            ((1, 6), (1, 3)),
            ((1, 7), (0, 13))
        ];

        let expected_operands = Operands {
            dst: MaybeRelocatable::from(bigint!(8)),
            res: Some(MaybeRelocatable::from(bigint!(8))),
            op0: MaybeRelocatable::from((2, 0)),
            op1: MaybeRelocatable::from(bigint!(8)),
        };
        let expected_operands_mem_addresses = Some(OperandsAddresses(
            Relocatable::from((1, 9)),
            Relocatable::from((1, 3)),
            Relocatable::from((2, 2)),
        ));
        assert_eq!(
            Ok((expected_operands, expected_operands_mem_addresses)),
            vm.compute_operands(&instruction)
        );
    }

    #[test]
    fn deduce_memory_cell_ec_op_builtin_valid() {
        let mut vm = vm!();
        let builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true);
        vm.builtin_runners
            .push((String::from("ec_op"), builtin.into()));

        vm.memory = memory![
            (
                (0, 0),
                (
                    b"2962412995502985605007699495352191122971573493113767820301112397466445942584",
                    10
                )
            ),
            (
                (0, 1),
                (
                    b"214950771763870898744428659242275426967582168179217139798831865603966154129",
                    10
                )
            ),
            (
                (0, 2),
                (
                    b"874739451078007766457464989774322083649278607533249481151382481072868806602",
                    10
                )
            ),
            (
                (0, 3),
                (
                    b"152666792071518830868575557812948353041420400780739481342941381225525861407",
                    10
                )
            ),
            ((0, 4), 34),
            (
                (0, 5),
                (
                    b"2778063437308421278851140253538604815869848682781135193774472480292420096757",
                    10
                )
            )
        ];

        let result = vm.deduce_memory_cell(&Relocatable::from((0, 6)));
        assert_eq!(
            result,
            Ok(Some(MaybeRelocatable::from(bigint_str!(
                b"3598390311618116577316045819420613574162151407434885460365915347732568210029"
            ))))
        );
    }

    #[test]
    /* Data taken from this program execution:
       %builtins output ec_op
       from starkware.cairo.common.cairo_builtins import EcOpBuiltin
       from starkware.cairo.common.serialize import serialize_word
       from starkware.cairo.common.ec_point import EcPoint
       from starkware.cairo.common.ec import ec_op

       func main{output_ptr: felt*, ec_op_ptr: EcOpBuiltin*}():
           let x: EcPoint = EcPoint(2089986280348253421170679821480865132823066470938446095505822317253594081284, 1713931329540660377023406109199410414810705867260802078187082345529207694986)

           let y: EcPoint = EcPoint(874739451078007766457464989774322083649278607533249481151382481072868806602,152666792071518830868575557812948353041420400780739481342941381225525861407)
           let z: EcPoint = ec_op(x,34, y)
           serialize_word(z.x)
           return()
           end
    */
    fn verify_auto_deductions_for_ec_op_builtin_valid() {
        let mut builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true);
        builtin.base = 3;
        let mut vm = vm!();
        vm.builtin_runners
            .push((String::from("ec_op"), builtin.into()));
        vm.memory = memory![
            (
                (3, 0),
                (
                    b"2962412995502985605007699495352191122971573493113767820301112397466445942584",
                    10
                )
            ),
            (
                (3, 1),
                (
                    b"214950771763870898744428659242275426967582168179217139798831865603966154129",
                    10
                )
            ),
            (
                (3, 2),
                (
                    b"874739451078007766457464989774322083649278607533249481151382481072868806602",
                    10
                )
            ),
            (
                (3, 3),
                (
                    b"152666792071518830868575557812948353041420400780739481342941381225525861407",
                    10
                )
            ),
            ((3, 4), 34),
            (
                (3, 5),
                (
                    b"2778063437308421278851140253538604815869848682781135193774472480292420096757",
                    10
                )
            )
        ];
        assert_eq!(vm.verify_auto_deductions(), Ok(()));
    }

    #[test]
    fn verify_auto_deductions_for_ec_op_builtin_valid_points_invalid_result() {
        let mut builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true);
        builtin.base = 3;
        let mut vm = vm!();
        vm.builtin_runners
            .push((String::from("ec_op"), builtin.into()));
        vm.memory = memory![
            (
                (3, 0),
                (
                    b"2962412995502985605007699495352191122971573493113767820301112397466445942584",
                    10
                )
            ),
            (
                (3, 1),
                (
                    b"214950771763870898744428659242275426967582168179217139798831865603966154129",
                    10
                )
            ),
            (
                (3, 2),
                (
                    b"2089986280348253421170679821480865132823066470938446095505822317253594081284",
                    10
                )
            ),
            (
                (3, 3),
                (
                    b"1713931329540660377023406109199410414810705867260802078187082345529207694986",
                    10
                )
            ),
            ((3, 4), 34),
            (
                (3, 5),
                (
                    b"2778063437308421278851140253538604815869848682781135193774472480292420096757",
                    10
                )
            )
        ];
        let error = vm.verify_auto_deductions();
        assert_eq!(
            error,
            Err(VirtualMachineError::InconsistentAutoDeduction(
                String::from("ec_op"),
                MaybeRelocatable::Int(bigint_str!(
                    b"2739017437753868763038285897969098325279422804143820990343394856167768859289"
                )),
                Some(MaybeRelocatable::Int(bigint_str!(
                    b"2778063437308421278851140253538604815869848682781135193774472480292420096757"
                )))
            ))
        );
        assert_eq!(error.unwrap_err().to_string(), "Inconsistent auto-deduction for builtin ec_op, expected Int(2739017437753868763038285897969098325279422804143820990343394856167768859289), got Some(Int(2778063437308421278851140253538604815869848682781135193774472480292420096757))");
    }

    #[test]
    /* Program used:
    %builtins bitwise
    from starkware.cairo.common.bitwise import bitwise_and
    from starkware.cairo.common.cairo_builtins import BitwiseBuiltin


    func main{bitwise_ptr: BitwiseBuiltin*}():
        let (result) = bitwise_and(12, 10)  # Binary (1100, 1010).
        assert result = 8  # Binary 1000.
        return()
    end
    */
    fn verify_auto_deductions_bitwise() {
        let mut builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);
        builtin.base = 2;
        let mut vm = vm!();
        vm.builtin_runners
            .push((String::from("bitwise"), builtin.into()));
        vm.memory = memory![((2, 0), 12), ((2, 1), 10)];
        assert_eq!(vm.verify_auto_deductions(), Ok(()));
    }

    #[test]
    /* Program used:
    %builtins output pedersen
    from starkware.cairo.common.cairo_builtins import HashBuiltin
    from starkware.cairo.common.hash import hash2
    from starkware.cairo.common.serialize import serialize_word

    func foo(hash_ptr : HashBuiltin*) -> (
        hash_ptr : HashBuiltin*, z
    ):
        # Use a with-statement, since 'hash_ptr' is not an
        # implicit argument.
        with hash_ptr:
            let (z) = hash2(32, 72)
        end
        return (hash_ptr=hash_ptr, z=z)
    end

    func main{output_ptr: felt*, pedersen_ptr: HashBuiltin*}():
        let (pedersen_ptr, a) = foo(pedersen_ptr)
        serialize_word(a)
        return()
    end
     */
    fn verify_auto_deductions_pedersen() {
        let mut builtin = HashBuiltinRunner::new(8, true);
        builtin.base = 3;
        let mut vm = vm!();
        vm.builtin_runners
            .push((String::from("pedersen"), builtin.into()));
        vm.memory = memory![((3, 0), 32), ((3, 1), 72)];
        assert_eq!(vm.verify_auto_deductions(), Ok(()));
    }

    #[test]
    fn can_get_return_values() {
        let mut vm = vm!();
        vm.set_ap(4);
        vm.memory = memory![((1, 0), 1), ((1, 1), 2), ((1, 2), 3), ((1, 3), 4)];
        let expected = vec![
            MaybeRelocatable::Int(1u32.into()),
            MaybeRelocatable::Int(2u32.into()),
            MaybeRelocatable::Int(3u32.into()),
            MaybeRelocatable::Int(4u32.into()),
        ];
        assert_eq!(vm.get_return_values(4).unwrap(), expected);
    }

    #[test]
    fn get_return_values_fails_when_ap_is_0() {
        let mut vm = vm!();
        vm.memory = memory![((1, 0), 1), ((1, 1), 2), ((1, 2), 3), ((1, 3), 4)];
        assert!(matches!(
            vm.get_return_values(3),
            Err(MemoryError::NumOutOfBounds)
        ));
    }

    /*
    Program used for this test:
    from starkware.cairo.common.alloc import alloc
    func main{}():
        let vec: felt* = alloc()
        assert vec[0] = 1
        return()
    end
    Memory: {RelocatableValue(segment_index=0, offset=0): 290341444919459839,
        RelocatableValue(segment_index=0, offset=1): 1,
        RelocatableValue(segment_index=0, offset=2): 2345108766317314046,
        RelocatableValue(segment_index=0, offset=3): 1226245742482522112,
        RelocatableValue(segment_index=0, offset=4): 3618502788666131213697322783095070105623107215331596699973092056135872020478,
        RelocatableValue(segment_index=0, offset=5): 5189976364521848832,
        RelocatableValue(segment_index=0, offset=6): 1,
        RelocatableValue(segment_index=0, offset=7): 4611826758063128575,
        RelocatableValue(segment_index=0, offset=8): 2345108766317314046,
        RelocatableValue(segment_index=1, offset=0): RelocatableValue(segment_index=2, offset=0),
        RelocatableValue(segment_index=1, offset=1): RelocatableValue(segment_index=3, offset=0)}
     */

    #[test]
    fn test_step_for_preset_memory_with_alloc_hint() {
        let mut vm = vm!(true);
        let hint_data_dictionary = HashMap::from([(
            0_usize,
            vec![any_box!(HintProcessorData::new_default(
                "memory[ap] = segments.add()".to_string(),
                HashMap::new(),
            ))],
        )]);

        //Initialzie registers
        run_context!(vm, 3, 2, 2);

        //Create program and execution segments
        for _ in 0..2 {
            vm.segments.add(&mut vm.memory);
        }
        //Initialize memory

        let hint_processor = BuiltinHintProcessor::new_empty();

        vm.memory = memory![
            ((0, 0), 290341444919459839_i64),
            ((0, 1), 1),
            ((0, 2), 2345108766317314046_i64),
            ((0, 3), 1226245742482522112_i64),
            (
                (0, 4),
                (
                    b"3618502788666131213697322783095070105623107215331596699973092056135872020478",
                    10
                )
            ),
            ((0, 5), 5189976364521848832_i64),
            ((0, 6), 1),
            ((0, 7), 4611826758063128575_i64),
            ((0, 8), 2345108766317314046_i64),
            ((1, 0), (2, 0)),
            ((1, 1), (3, 0))
        ];

        //Run Steps
        for _ in 0..6 {
            assert_eq!(
                vm.step(
                    &hint_processor,
                    exec_scopes_ref!(),
                    &hint_data_dictionary,
                    &HashMap::new()
                ),
                Ok(())
            );
        }
        //Compare trace
        let trace = vm.trace.unwrap();
        trace_check!(
            trace,
            [
                ((0, 3), (1, 2), (1, 2)),
                ((0, 0), (1, 4), (1, 4)),
                ((0, 2), (1, 5), (1, 4)),
                ((0, 5), (1, 5), (1, 2)),
                ((0, 7), (1, 6), (1, 2)),
                ((0, 8), (1, 6), (1, 2))
            ]
        );

        //Compare final register values
        assert_eq!(vm.run_context.pc, Relocatable::from((3, 0)));
        assert_eq!(vm.run_context.ap, 6);
        assert_eq!(vm.run_context.fp, 0);

        //Check that the array created through alloc contains the element we inserted
        //As there are no builtins present, the next segment crated will have the index 2
        assert_eq!(
            vm.memory.data[2],
            vec![Some(MaybeRelocatable::from(bigint!(1)))]
        );
    }

    #[test]
    fn test_get_builtin_runners() {
        let mut vm = vm!();
        let hash_builtin = HashBuiltinRunner::new(8, true);
        let bitwise_builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);
        vm.builtin_runners
            .push((String::from("pedersen"), hash_builtin.into()));
        vm.builtin_runners
            .push((String::from("bitwise"), bitwise_builtin.into()));

        let builtins = vm.get_builtin_runners();

        assert_eq!(builtins[0].0, "pedersen");
        assert_eq!(builtins[1].0, "bitwise");
    }

    #[test]
    fn disable_trace() {
        let mut vm = VirtualMachine::new(
            BigInt::new(Sign::Plus, vec![1, 0, 0, 0, 0, 0, 17, 134217728]),
            true,
            Vec::new(),
        );
        assert!(vm.trace.is_some());
        vm.disable_trace();
        assert!(vm.trace.is_none());
    }

    #[test]
    fn get_range_for_continuous_memory() {
        let mut vm = vm!();
        vm.memory = memory![((1, 0), 2), ((1, 1), 3), ((1, 2), 4)];

        let value1 = MaybeRelocatable::from(bigint!(2));
        let value2 = MaybeRelocatable::from(bigint!(3));
        let value3 = MaybeRelocatable::from(bigint!(4));

        let expected_vec = vec![
            Some(Cow::Borrowed(&value1)),
            Some(Cow::Borrowed(&value2)),
            Some(Cow::Borrowed(&value3)),
        ];
        assert_eq!(
            vm.get_range(&MaybeRelocatable::from((1, 0)), 3),
            Ok(expected_vec)
        );
    }

    #[test]
    fn get_range_for_non_continuous_memory() {
        let mut vm = vm!();
        vm.memory = memory![((1, 0), 2), ((1, 1), 3), ((1, 3), 4)];

        let value1 = MaybeRelocatable::from(bigint!(2));
        let value2 = MaybeRelocatable::from(bigint!(3));
        let value3 = MaybeRelocatable::from(bigint!(4));

        let expected_vec = vec![
            Some(Cow::Borrowed(&value1)),
            Some(Cow::Borrowed(&value2)),
            None,
            Some(Cow::Borrowed(&value3)),
        ];
        assert_eq!(
            vm.get_range(&MaybeRelocatable::from((1, 0)), 4),
            Ok(expected_vec)
        );
    }

    #[test]
    fn get_continuous_range_for_continuous_memory() {
        let mut vm = vm!();
        vm.memory = memory![((1, 0), 2), ((1, 1), 3), ((1, 2), 4)];

        let value1 = MaybeRelocatable::from(bigint!(2));
        let value2 = MaybeRelocatable::from(bigint!(3));
        let value3 = MaybeRelocatable::from(bigint!(4));

        let expected_vec = vec![value1, value2, value3];
        assert_eq!(
            vm.get_continuous_range(&MaybeRelocatable::from((1, 0)), 3),
            Ok(expected_vec)
        );
    }

    #[test]
    fn get_continuous_range_for_non_continuous_memory() {
        let mut vm = vm!();
        vm.memory = memory![((1, 0), 2), ((1, 1), 3), ((1, 3), 4)];

        assert_eq!(
            vm.get_continuous_range(&MaybeRelocatable::from((1, 0)), 3),
            Err(MemoryError::GetRangeMemoryGap)
        );
    }

    #[test]
    fn get_segment_used_size_after_computing_used() {
        let mut vm = vm!();
        vm.memory = memory![
            ((0, 2), 1),
            ((0, 5), 1),
            ((0, 7), 1),
            ((1, 1), 1),
            ((2, 2), 1),
            ((2, 4), 1),
            ((2, 7), 1)
        ];
        vm.segments.compute_effective_sizes(&vm.memory);
        assert_eq!(Some(8), vm.get_segment_used_size(2));
    }

    #[test]
    fn get_segment_used_size_before_computing_used() {
        let vm = vm!();
        assert_eq!(None, vm.get_segment_used_size(2));
    }

    #[test]
    fn get_and_set_pc() {
        let mut vm = vm!();
        vm.set_pc(Relocatable {
            segment_index: 3,
            offset: 4,
        });
        assert_eq!(
            vm.get_pc(),
            &Relocatable {
                segment_index: 3,
                offset: 4
            }
        )
    }

    #[test]
    fn get_and_set_fp() {
        let mut vm = vm!();
        vm.set_fp(3);
        assert_eq!(
            vm.get_fp(),
            Relocatable {
                segment_index: 1,
                offset: 3
            }
        )
    }

    #[test]
    fn get_maybe_key_not_in_memory() {
        let vm = vm!();
        assert_eq!(
            vm.get_maybe(&Relocatable {
                segment_index: 5,
                offset: 2
            }),
            Ok(None)
        );
    }

    #[test]
    fn get_maybe_error() {
        let vm = vm!();
        assert_eq!(
            vm.get_maybe(&MaybeRelocatable::Int(bigint!(0))),
            Err(MemoryError::AddressNotRelocatable)
        );
    }

    #[test]
    fn end_run_error() {
        let mut vm = vm!();
        let scopes = exec_scopes_ref!();
        scopes.enter_scope(HashMap::new());

        assert_eq!(
            vm.end_run(scopes),
            Err(VirtualMachineError::MainScopeError(
                ExecScopeError::NoScopeError
            ))
        );
    }

    #[test]
    fn add_temporary_segments() {
        let mut vm = vm!();
        let mut _base = vm.add_temporary_segment();
        assert_eq!(
            _base,
            Relocatable {
                segment_index: -1,
                offset: 0
            }
        );
        let mut _base = vm.add_temporary_segment();
        assert_eq!(
            _base,
            Relocatable {
                segment_index: -2,
                offset: 0
            }
        );
    }

    #[test]
    fn deduce_memory_cell_error_from_dec_str() {
        let mut vm = vm!();
        vm.builtin_runners.push((
            "pedersen".to_string(),
            HashBuiltinRunner::new(256, true).into(),
        ));

        vm.memory = memory![((0, 0), 0xa8), ((0, 2), 0)];

        // Insert a number that will fail when converting from str to dec.
        let _ = vm.memory.insert(
            &Relocatable::from((0, 1)),
            &MaybeRelocatable::Int(bigint!(1) << 255),
        );

        assert_eq!(
            vm.deduce_memory_cell(&Relocatable::from((0, 2))),
            Err(VirtualMachineError::RunnerError(
                RunnerError::FailedStringConversion
            ))
        )
    }

    #[test]
    fn compute_dst_deductions_insert_into_written_mem() {
        let mut vm = vm!();
        vm.memory = memory![((0, 0), 1), ((1, 0), 4)];
        let dst_addr = Relocatable::from((1, 0));
        let res = MaybeRelocatable::Int(bigint!(5));
        let instruction = Instruction {
            off0: 0,
            off1: 0,
            off2: 0,
            imm: None,
            dst_register: Register::AP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        assert_eq!(
            vm.compute_dst_deductions(&dst_addr, &instruction, &Some(res)),
            Err(VirtualMachineError::MemoryError(
                MemoryError::InconsistentMemory(
                    MaybeRelocatable::from(dst_addr),
                    MaybeRelocatable::Int(bigint!(4)),
                    MaybeRelocatable::Int(bigint!(5))
                )
            ))
        );
    }

    #[test]
    fn compute_op1_deductions_insert_into_written_mem() {
        let mut vm = vm!();
        vm.memory = memory![((0, 0), 1), ((1, 0), 4)];
        let op1_addr = Relocatable::from((1, 0));
        let dst_op = MaybeRelocatable::Int(bigint!(10));
        let op0 = MaybeRelocatable::Int(bigint!(10));
        let res = MaybeRelocatable::Int(bigint!(5));
        let instruction = Instruction {
            off0: 0,
            off1: 0,
            off2: 0,
            imm: None,
            dst_register: Register::AP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Op1,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        assert_eq!(
            vm.compute_op1_deductions(&op1_addr, &mut Some(res), &instruction, &Some(dst_op), &op0),
            Err(VirtualMachineError::MemoryError(
                MemoryError::InconsistentMemory(
                    MaybeRelocatable::from(op1_addr),
                    MaybeRelocatable::Int(bigint!(4)),
                    MaybeRelocatable::Int(bigint!(10))
                )
            ))
        );
    }

    #[test]
    fn compute_op0_deductions_insert_into_written_mem() {
        let mut vm = vm!();
        vm.memory = memory![((0, 0), 1), ((1, 0), 4)];
        let op0_addr = Relocatable::from((1, 0));
        let res = MaybeRelocatable::Int(bigint!(5));
        let dst_op = MaybeRelocatable::Int(bigint!(20));
        let op1_op = MaybeRelocatable::Int(bigint!(10));
        let instruction = Instruction {
            off0: 0,
            off1: 0,
            off2: 0,
            imm: None,
            dst_register: Register::AP,
            op0_register: Register::AP,
            op1_addr: Op1Addr::AP,
            res: Res::Add,
            pc_update: PcUpdate::Regular,
            ap_update: ApUpdate::Regular,
            fp_update: FpUpdate::Regular,
            opcode: Opcode::AssertEq,
        };

        assert_eq!(
            vm.compute_op0_deductions(
                &op0_addr,
                &mut Some(res),
                &instruction,
                &Some(dst_op),
                &Some(op1_op)
            ),
            Err(VirtualMachineError::MemoryError(
                MemoryError::InconsistentMemory(
                    MaybeRelocatable::from(op0_addr),
                    MaybeRelocatable::Int(bigint!(4)),
                    MaybeRelocatable::Int(bigint!(10))
                )
            ))
        );
    }

    #[test]
    fn decode_current_instruction_invalid_encoding() {
        let mut vm = vm!();
        vm.memory = memory![((0, 0), (b"112233445566778899", 16))];
        assert_eq!(
            vm.decode_current_instruction(),
            Err(VirtualMachineError::InvalidInstructionEncoding)
        );
    }

    #[test]
    fn add_relocation_rule_test() {
        let mut vm = vm!();

        assert_eq!(
            vm.add_relocation_rule((-1, 0).into(), (1, 2).into()),
            Ok(()),
        );
        assert_eq!(
            vm.add_relocation_rule((-2, 0).into(), (-1, 1).into()),
            Ok(()),
        );
        assert_eq!(
            vm.add_relocation_rule((5, 0).into(), (0, 0).into()),
            Err(MemoryError::AddressNotInTemporarySegment(5)),
        );
        assert_eq!(
            vm.add_relocation_rule((-3, 6).into(), (0, 0).into()),
            Err(MemoryError::NonZeroOffset(6)),
        );
        assert_eq!(
            vm.add_relocation_rule((-1, 0).into(), (0, 0).into()),
            Err(MemoryError::DuplicatedRelocation(-1)),
        );
    }

    #[test]
    fn gen_arg_relocatable() {
        let mut vm = vm!();

        assert_eq!(
            vm.gen_arg(&mayberelocatable!(0, 0), None),
            Ok(mayberelocatable!(0, 0)),
        );
    }

    /// Test that the call to .gen_arg() with a bigint and no prime number just
    /// passes the value through.
    #[test]
    fn gen_arg_bigint() {
        let mut vm = vm!();

        assert_eq!(
            vm.gen_arg(&mayberelocatable!(1234), None),
            Ok(mayberelocatable!(1234)),
        );
    }

    /// Test that the call to .gen_arg() with a bigint and a prime number passes
    /// the value through after applying the modulo.
    #[test]
    fn gen_arg_bigint_prime() {
        let mut vm = vm!();

        assert_eq!(
            vm.gen_arg(&mayberelocatable!(1234), Some(&bigint!(1234)),),
            Ok(mayberelocatable!(0)),
        );
    }

    /// Test that the call to .gen_arg() with a Vec<MaybeRelocatable> writes its
    /// contents into a new segment and returns a pointer to it.
    #[test]
    fn gen_arg_vec() {
        let mut vm = vm!();

        assert_eq!(
            vm.gen_arg(
                &vec![
                    mayberelocatable!(0),
                    mayberelocatable!(1),
                    mayberelocatable!(2),
                    mayberelocatable!(3),
                    mayberelocatable!(0, 0),
                    mayberelocatable!(0, 1),
                    mayberelocatable!(0, 2),
                    mayberelocatable!(0, 3),
                ],
                Some(&bigint!(1234)),
            ),
            Ok(mayberelocatable!(0, 0)),
        );
    }

    /// Test that the call to .gen_arg() with any other argument returns a not
    /// implemented error.
    #[test]
    fn gen_arg_not_implemented() {
        let mut vm = vm!();

        assert_eq!(
            vm.gen_arg(&"", None),
            Err(VirtualMachineError::NotImplemented),
        );
    }

    #[test]
    fn gen_typed_args_empty() {
        let vm = vm!();

        assert_eq!(vm.gen_typed_args(vec![]), Ok(vec![]));
    }

    /// Test that the call to .gen_typed_args() with an unsupported vector
    /// returns a not implemented error.
    #[test]
    fn gen_typed_args_not_implemented() {
        let vm = vm!();

        assert_eq!(
            vm.gen_typed_args(vec![&0usize]),
            Err(VirtualMachineError::NotImplemented),
        );
    }

    /// Test that the call to .gen_typed_args() with a Vec<MaybeRelocatable>
    /// with a bigint returns the contents after applying the modulo.
    #[test]
    fn gen_typed_args_bigint() {
        let vm = vm!();

        assert_eq!(
            vm.gen_typed_args(vec![&MaybeRelocatable::Int(vm.get_prime() + &bigint!(1))]),
            Ok(vec![mayberelocatable!(1)]),
        );
    }

    /// Test that the call to .gen_typed_args() with a Vec<MaybeRelocatable>
    /// with a relocatables returns the original contents.
    #[test]
    fn gen_typed_args_relocatable_slice() {
        let vm = vm!();

        assert_eq!(
            vm.gen_typed_args(vec![&[
                mayberelocatable!(0, 0),
                mayberelocatable!(0, 1),
                mayberelocatable!(0, 2),
            ]
            .into_iter()
            .collect::<Vec<MaybeRelocatable>>(),]),
            Ok(vec![
                mayberelocatable!(0, 0),
                mayberelocatable!(0, 1),
                mayberelocatable!(0, 2),
            ]),
        );
    }
}
