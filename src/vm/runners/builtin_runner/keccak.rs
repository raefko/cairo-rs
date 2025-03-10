use crate::bigint;
use crate::hint_processor::builtin_hint_processor::cairo_keccak::keccak_hints::{
    maybe_reloc_vec_to_u64_array, u64_array_to_mayberelocatable_vec,
};
use crate::hint_processor::builtin_hint_processor::keccak_utils::left_pad_u64;
use crate::math_utils::safe_div_usize;
use crate::types::instance_definitions::keccak_instance_def::KeccakInstanceDef;
use crate::types::relocatable::{MaybeRelocatable, Relocatable};
use crate::vm::errors::memory_errors::MemoryError;
use crate::vm::errors::runner_errors::RunnerError;
use crate::vm::vm_core::VirtualMachine;
use crate::vm::vm_memory::memory::Memory;
use crate::vm::vm_memory::memory_segments::MemorySegmentManager;
use num_bigint::BigInt;
use num_integer::div_ceil;

const KECCAK_ARRAY_LEN: usize = 25;

#[derive(Debug, Clone)]
pub struct KeccakBuiltinRunner {
    ratio: u32,
    pub base: isize,
    pub(crate) cells_per_instance: u32,
    pub(crate) n_input_cells: u32,
    verified_addresses: Vec<Relocatable>,
    pub(crate) stop_ptr: Option<usize>,
    _included: bool,
    state_rep: Vec<u32>,
    instances_per_component: u32,
}

impl KeccakBuiltinRunner {
    pub(crate) fn new(
        instance_def: &KeccakInstanceDef,
        included: bool,
    ) -> Result<Self, RunnerError> {
        Ok(KeccakBuiltinRunner {
            base: 0,
            ratio: instance_def._ratio,
            n_input_cells: instance_def._state_rep.len() as u32,
            cells_per_instance: instance_def._cells_per_builtin(),
            stop_ptr: None,
            verified_addresses: Vec::new(),
            _included: included,
            instances_per_component: instance_def._instance_per_component,
            state_rep: instance_def._state_rep.clone(),
        })
    }

    pub fn initialize_segments(
        &mut self,
        segments: &mut MemorySegmentManager,
        memory: &mut Memory,
    ) {
        self.base = segments.add(memory).segment_index
    }

    pub fn initial_stack(&self) -> Vec<MaybeRelocatable> {
        if self._included {
            vec![MaybeRelocatable::from((self.base, 0))]
        } else {
            vec![]
        }
    }

    pub fn base(&self) -> isize {
        self.base
    }

    pub fn ratio(&self) -> u32 {
        self.ratio
    }

    pub fn add_validation_rule(&self, _memory: &mut Memory) -> Result<(), RunnerError> {
        Ok(())
    }

    pub fn deduce_memory_cell(
        &self,
        address: &Relocatable,
        memory: &Memory,
    ) -> Result<Option<MaybeRelocatable>, RunnerError> {
        let index = address.offset % self.cells_per_instance as usize;
        if index < self.n_input_cells as usize {
            return Ok(None);
        }

        let first_input_addr = address
            .clone()
            .sub(index)
            .map_err(|_| RunnerError::BaseNotFinished)?;

        if self.verified_addresses.contains(&first_input_addr) {
            return Ok(None);
        }

        for i in 0..self.n_input_cells {
            match memory.get(&(&first_input_addr + i as usize)) {
                Err(_err) => return Ok(None),
                Ok(None) => return Ok(None),
                _ok => (),
            };
        }

        if let Some((i, bits)) = self.state_rep.iter().enumerate().next() {
            let value1 = memory
                .get(&(&first_input_addr + i))
                .map_err(RunnerError::FailedMemoryGet)?
                .ok_or(RunnerError::NonRelocatableAddress)?;

            let val = match value1.clone().into_owned() {
                MaybeRelocatable::Int(val) => val,
                _ => return Err(RunnerError::FoundNonInt),
            };

            if !(bigint!(0_i32) <= val && val < (bigint!(2_i32)).pow(*bits as u32)) {
                return Err(RunnerError::IntegerBiggerThanPowerOfTwo(
                    value1.into_owned(),
                    *bits as u32,
                    val,
                ));
            }

            let mut input_felts = vec![];

            for i in 0..self.n_input_cells {
                let value2 = memory
                    .get(&(first_input_addr.clone() + i as usize))
                    .map_err(RunnerError::FailedMemoryGet)?;

                input_felts.push(value2)
            }

            let mut input_felts_u64 = maybe_reloc_vec_to_u64_array(&input_felts)
                .map_err(|_| RunnerError::MaybeRelocVecToU64ArrayError)?;

            let len = input_felts_u64.len();
            let mut input_felts_u64 = left_pad_u64(&mut input_felts_u64, KECCAK_ARRAY_LEN - len)
                .try_into()
                .map_err(|_| RunnerError::SliceToArrayError)?;

            keccak::f1600(&mut input_felts_u64);

            let bigint_values = u64_array_to_mayberelocatable_vec(&input_felts_u64);

            return Ok(Some(bigint_values[address.offset - 1].clone()));
        }
        Ok(None)
    }

    pub fn get_allocated_memory_units(&self, vm: &VirtualMachine) -> Result<usize, MemoryError> {
        let value = safe_div_usize(vm.current_step, self.ratio as usize)
            .map_err(|_| MemoryError::ErrorCalculatingMemoryUnits)?;
        Ok(self.cells_per_instance as usize * value)
    }

    pub fn get_memory_segment_addresses(&self) -> (&'static str, (isize, Option<usize>)) {
        ("keccak", (self.base, self.stop_ptr))
    }

    pub fn get_used_cells(&self, vm: &VirtualMachine) -> Result<usize, MemoryError> {
        let base = self.base();
        vm.segments
            .get_segment_used_size(
                base.try_into()
                    .map_err(|_| MemoryError::AddressInTemporarySegment(base))?,
            )
            .ok_or(MemoryError::MissingSegmentUsedSizes)
    }

    pub fn get_used_cells_and_allocated_size(
        &self,
        vm: &VirtualMachine,
    ) -> Result<(usize, usize), MemoryError> {
        let ratio = self.ratio as usize;

        let cells_per_instance = self.cells_per_instance;
        let min_step = ratio * self.instances_per_component as usize;
        if vm.current_step < min_step {
            Err(MemoryError::InsufficientAllocatedCells)
        } else {
            let used = self.get_used_cells(vm)?;
            let size = cells_per_instance as usize
                * safe_div_usize(vm.current_step, ratio)
                    .map_err(|_| MemoryError::InsufficientAllocatedCells)?;
            Ok((used, size))
        }
    }

    pub fn get_used_instances(&self, vm: &VirtualMachine) -> Result<usize, MemoryError> {
        let used_cells = self.get_used_cells(vm)?;
        Ok(div_ceil(used_cells, self.cells_per_instance as usize))
    }

    pub fn final_stack(
        &self,
        vm: &VirtualMachine,
        pointer: Relocatable,
    ) -> Result<(Relocatable, usize), RunnerError> {
        if self._included {
            if let Ok(stop_pointer) = vm
                .get_relocatable(&(pointer.sub(1)).map_err(|_| RunnerError::FinalStack)?)
                .as_deref()
            {
                if self.base() != stop_pointer.segment_index {
                    return Err(RunnerError::InvalidStopPointer("keccak".to_string()));
                }
                let stop_ptr = stop_pointer.offset;
                let num_instances = self
                    .get_used_instances(vm)
                    .map_err(|_| RunnerError::FinalStack)?;
                let used_cells = num_instances * self.cells_per_instance as usize;
                if stop_ptr != used_cells {
                    return Err(RunnerError::InvalidStopPointer("keccak".to_string()));
                }

                Ok((
                    pointer.sub(1).map_err(|_| RunnerError::FinalStack)?,
                    stop_ptr,
                ))
            } else {
                Err(RunnerError::FinalStack)
            }
        } else {
            let stop_ptr = self.base() as usize;
            Ok((pointer, stop_ptr))
        }
    }

    pub fn get_memory_accesses(
        &self,
        vm: &VirtualMachine,
    ) -> Result<Vec<Relocatable>, MemoryError> {
        let base = self.base();
        let segment_size = vm
            .segments
            .get_segment_size(
                base.try_into()
                    .map_err(|_| MemoryError::AddressInTemporarySegment(base))?,
            )
            .ok_or(MemoryError::MissingSegmentUsedSizes)?;

        Ok((0..segment_size).map(|i| (base, i).into()).collect())
    }

    pub fn get_used_diluted_check_units(&self, diluted_n_bits: u32) -> usize {
        // The diluted cells are:
        // state - 25 rounds times 1600 elements.
        // parity - 24 rounds times 1600/5 elements times 3 auxiliaries.
        // after_theta_rho_pi - 24 rounds times 1600 elements.
        // theta_aux - 24 rounds times 1600 elements.
        // chi_iota_aux - 24 rounds times 1600 elements times 2 auxiliaries.
        // In total 25 * 1600 + 24 * 320 * 3 + 24 * 1600 + 24 * 1600 + 24 * 1600 * 2 = 216640.
        // But we actually allocate 4 virtual columns, of dimensions 64 * 1024, in which we embed the
        // real cells, and we don't free the unused ones.
        // So the real number is 4 * 64 * 1024 = 262144.
        safe_div_usize(262144_usize, diluted_n_bits as usize).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::bigint_str;
    use crate::hint_processor::builtin_hint_processor::builtin_hint_processor_definition::BuiltinHintProcessor;
    use crate::types::program::Program;
    use crate::utils::test_utils::*;
    use crate::vm::runners::cairo_runner::CairoRunner;
    use crate::vm::{
        errors::{memory_errors::MemoryError, runner_errors::RunnerError},
        runners::builtin_runner::BuiltinRunner,
        vm_core::VirtualMachine,
    };
    use num_bigint::Sign;

    #[test]
    fn get_used_instances() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::new(10), true).unwrap();

        let mut vm = vm!();

        vm.memory = memory![
            ((0, 0), (0, 0)),
            ((0, 1), (0, 1)),
            ((2, 0), (0, 0)),
            ((2, 1), (0, 0))
        ];

        vm.segments.segment_used_sizes = Some(vec![1]);

        assert_eq!(builtin.get_used_instances(&vm), Ok(1));
    }

    #[test]
    fn final_stack() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::new(10), true).unwrap();

        let mut vm = vm!();

        vm.memory = memory![
            ((0, 0), (0, 0)),
            ((0, 1), (0, 1)),
            ((2, 0), (0, 0)),
            ((2, 1), (0, 0))
        ];

        vm.segments.segment_used_sizes = Some(vec![0]);

        let pointer = Relocatable::from((2, 2));

        assert_eq!(
            builtin.final_stack(&vm, pointer).unwrap(),
            (Relocatable::from((2, 1)), 0)
        );
    }

    #[test]
    fn final_stack_error_stop_pointer() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::new(10), true).unwrap();

        let mut vm = vm!();

        vm.memory = memory![
            ((0, 0), (0, 0)),
            ((0, 1), (0, 1)),
            ((2, 0), (0, 0)),
            ((2, 1), (0, 0))
        ];

        vm.segments.segment_used_sizes = Some(vec![999]);

        let pointer = Relocatable::from((2, 2));

        assert_eq!(
            builtin.final_stack(&vm, pointer),
            Err(RunnerError::InvalidStopPointer("keccak".to_string()))
        );
    }

    #[test]
    fn final_stack_error_when_not_included() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::new(10), false).unwrap();

        let mut vm = vm!();

        vm.memory = memory![
            ((0, 0), (0, 0)),
            ((0, 1), (0, 1)),
            ((2, 0), (0, 0)),
            ((2, 1), (0, 0))
        ];

        vm.segments.segment_used_sizes = Some(vec![0]);

        let pointer = Relocatable::from((2, 2));

        assert_eq!(
            builtin.final_stack(&vm, pointer).unwrap(),
            (Relocatable::from((2, 2)), 0)
        );
    }

    #[test]
    fn final_stack_error_non_relocatable() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::new(10), true).unwrap();

        let mut vm = vm!();

        vm.memory = memory![
            ((0, 0), (0, 0)),
            ((0, 1), (0, 1)),
            ((2, 0), (0, 0)),
            ((2, 1), 2)
        ];

        vm.segments.segment_used_sizes = Some(vec![0]);

        let pointer = Relocatable::from((2, 2));

        assert_eq!(
            builtin.final_stack(&vm, pointer),
            Err(RunnerError::FinalStack)
        );
    }

    #[test]
    fn get_used_cells_and_allocated_size_test() {
        let builtin: BuiltinRunner = KeccakBuiltinRunner::new(&KeccakInstanceDef::new(10), true)
            .unwrap()
            .into();

        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![0]);
        let program =
            Program::from_file(Path::new("cairo_programs/_keccak.json"), Some("main")).unwrap();

        let mut cairo_runner = cairo_runner!(program, "recursive");

        let hint_processor = BuiltinHintProcessor::new_empty();

        let address = cairo_runner.initialize(&mut vm).unwrap();

        cairo_runner
            .run_until_pc(address, &mut vm, &hint_processor)
            .unwrap();

        assert_eq!(
            builtin.get_used_cells_and_allocated_size(&vm),
            Ok((0, 1072))
        );
    }

    #[test]
    fn get_allocated_memory_units() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::new(10), true).unwrap();

        let mut vm = vm!();

        let program = program!(
            builtins = vec![String::from("keccak")],
            data = vec_data!(
                (4612671182993129469_i64),
                (5189976364521848832_i64),
                (18446744073709551615_i128),
                (5199546496550207487_i64),
                (4612389712311386111_i64),
                (5198983563776393216_i64),
                (2),
                (2345108766317314046_i64),
                (5191102247248822272_i64),
                (5189976364521848832_i64),
                (7),
                (1226245742482522112_i64),
                ((
                    b"3618502788666131213697322783095070105623107215331596699973092056135872020470",
                    10
                )),
                (2345108766317314046_i64)
            ),
            main = Some(8),
        );

        let mut cairo_runner = cairo_runner!(program, "recursive");

        let hint_processor = BuiltinHintProcessor::new_empty();

        let address = cairo_runner.initialize(&mut vm).unwrap();

        cairo_runner
            .run_until_pc(address, &mut vm, &hint_processor)
            .unwrap();

        assert_eq!(builtin.get_allocated_memory_units(&vm), Ok(16));
    }

    #[test]
    fn get_memory_segment_addresses() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();

        assert_eq!(
            builtin.get_memory_segment_addresses(),
            ("keccak", (0, None))
        );
    }

    #[test]
    fn get_memory_accesses_missing_segment_used_sizes() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();
        let vm = vm!();

        assert_eq!(
            builtin.get_memory_accesses(&vm),
            Err(MemoryError::MissingSegmentUsedSizes),
        );
    }

    #[test]
    fn get_memory_accesses_empty() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![0]);
        assert_eq!(builtin.get_memory_accesses(&vm), Ok(vec![]));
    }

    #[test]
    fn get_memory_accesses() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![4]);
        assert_eq!(
            builtin.get_memory_accesses(&vm),
            Ok(vec![
                (builtin.base(), 0).into(),
                (builtin.base(), 1).into(),
                (builtin.base(), 2).into(),
                (builtin.base(), 3).into(),
            ]),
        );
    }

    #[test]
    fn get_used_cells_missing_segment_used_sizes() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();
        let vm = vm!();

        assert_eq!(
            builtin.get_used_cells(&vm),
            Err(MemoryError::MissingSegmentUsedSizes)
        );
    }

    #[test]
    fn get_used_cells_empty() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![0]);
        assert_eq!(builtin.get_used_cells(&vm), Ok(0));
    }

    #[test]
    fn get_used_cells() {
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![4]);
        assert_eq!(builtin.get_used_cells(&vm), Ok(4));
    }

    #[test]
    fn initial_stack_included_test() {
        let keccak_builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();
        assert_eq!(
            keccak_builtin.initial_stack(),
            vec![mayberelocatable!(0, 0)]
        )
    }

    #[test]
    fn initial_stack_not_included_test() {
        let keccak_builtin =
            KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), false).unwrap();
        assert_eq!(keccak_builtin.initial_stack(), Vec::new())
    }

    #[test]
    fn deduce_memory_cell_memory_valid() {
        let memory = memory![
            ((0, 16), 43),
            ((0, 17), 199),
            ((0, 18), 0),
            ((0, 19), 0),
            ((0, 20), 0),
            ((0, 21), 0),
            ((0, 22), 0),
            ((0, 23), 1),
            ((0, 24), 0),
            ((0, 25), 0),
            ((0, 26), 43),
            ((0, 27), 199),
            ((0, 28), 0),
            ((0, 29), 0),
            ((0, 30), 0),
            ((0, 31), 0),
            ((0, 32), 0),
            ((0, 33), 1),
            ((0, 34), 0),
            ((0, 35), 0)
        ];
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();

        let result = builtin.deduce_memory_cell(&Relocatable::from((0, 25)), &memory);
        assert_eq!(
            result,
            Ok(Some(MaybeRelocatable::from(bigint_str!(
                b"3086936446498698982"
            ))))
        );
    }

    #[test]
    fn deduce_memory_cell_non_reloc_address_err() {
        let memory = memory![
            ((0, 4), 32),
            ((0, 5), 72),
            ((0, 6), 0),
            ((0, 7), 120),
            ((0, 8), 52)
        ];
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();
        let result = builtin.deduce_memory_cell(&Relocatable::from((0, 25)), &memory);
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn deduce_memory_cell_none() {
        let memory = memory![
            ((0, 4), 32),
            ((0, 5), 72),
            ((0, 6), 0),
            ((0, 7), 120),
            ((0, 8), 52)
        ];
        let builtin = KeccakBuiltinRunner::new(&KeccakInstanceDef::default(), true).unwrap();
        let result = builtin.deduce_memory_cell(&Relocatable::from((0, 2)), &memory);
        assert_eq!(result, Ok(None));
    }
}
