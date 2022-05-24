use std::{cell::RefCell, collections::HashMap, sync::{Arc, Mutex}};

use petgraph::{graph::NodeIndex};
use rand::prelude::StdRng;
use rand::Rng;
#[cfg(not(target_arch = "wasm32"))]
use rayon::iter::ParallelIterator;

use coeus_macros::iterator;

use self::runtime::{invoke_runtime, StringClass};

use coeus_models::models::{BinaryObject, Class, CodeItem, DexFile, Instruction, MethodData, ValueType, InstructionOffset, InstructionSize, Method};

pub mod runtime;

use runtime::VM_BUILTINS;

const MAX_SIZE : usize = 100_000;

#[derive(Clone)]
pub struct VMState {
    pub pc: InstructionOffset,
    pub last_instruction_size: InstructionSize,
    pub current_instruction_size: InstructionSize,
    pub current_stackframe: Vec<Register>,
    pub return_reg: Register,
    num_params: usize,
    num_registers: usize,
    current_instructions: HashMap<InstructionOffset, (InstructionSize, Instruction)>,
    pub current_dex_file: Arc<DexFile>,
    pub current_method_index: u32,
    pub vm_state: ExecutionState,
    last_break_point_reg: u32,
}

impl std::fmt::Debug for VMState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VMState")
            .field("PC", &self.pc)
            .field("CurrentStackframe", &self.current_stackframe)
            .field("ReturnRegister", &self.return_reg)
            .field("CurrentState", &self.vm_state)
            .field("CurrentMethodIndex", &self.current_method_index)
            .field("CurrentDexFile", &self.current_dex_file.identifier)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ExecutionState {
    Stopped,
    Paused,
    Running,
    RunningStaticInitializer,
    StaticInitializer,
    Error,
    Finished,
}

#[derive(Clone, Debug)]
pub enum InformationNode<'a> {
    Source(Value),
    Field(u32),
    Method(u32),
    String(u32),
    ArrayData(&'a [u8]),
}
#[derive(Clone, Debug)]
pub enum Value {
    Array(Vec<u8>),
    Object(ClassInstance),
    Int(i32),
    Short(i16),
    Byte(i8),
}
impl Value {
    pub fn as_string(&self) -> Option<String> {
        if let Value::Object(cl) = self {
            if cl.class.class_name == runtime::StringClass::class_name() {
                return Some(format!("{}", cl));
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
pub enum InternalObject {
    String(String),
    Class(Arc<Class>),
    Vec(Vec<u8>),
    I32(i32),
    U32(u32),
    I64(i64),
}

#[derive(Clone, Debug)]
pub struct ClassInstance {
    internal_state: HashMap<String, InternalObject>,
    pub instances: HashMap<String, u32>,
    pub class: Arc<Class>,
}

impl std::fmt::Display for ClassInstance{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.class.class_name == "Ljava/lang/String;" {
            match self.internal_state.get("tmp_string") {
                Some(InternalObject::String(string)) => f.write_fmt(format_args!("{}", string)),
                _ => {
                    log::debug!("New string instance... {:?}", self.class);
                    f.write_fmt(format_args!("NEW INSTANCE"))
                }
            }
        } else {
            f.debug_struct("ClassInstance")
                .field("class", &self.class)
                .field("instances", &self.instances)
                .finish()
        }
    }
}

impl ClassInstance{
    pub fn new(class: Arc<Class>) -> Self {
        ClassInstance {
            internal_state: HashMap::new(),
            instances: HashMap::new(),
            class,
        }
    }
    pub fn from_class_name(
        class_name: &str,
        internal_state: HashMap<String, InternalObject>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if let Some(class) = VM_BUILTINS.get(class_name) {
            Ok(ClassInstance {
                instances: HashMap::new(),
                internal_state,
                class: class.clone(),
            })
        } else {
            Err("No Builtin found".into())
        }
    }
    pub fn with_internal_state(
        class: Arc<Class>,
        internal_state: HashMap<String, InternalObject>,
    ) -> Self {
        ClassInstance {
            internal_state,
            instances: HashMap::new(),
            class: class,
        }
    }
}
/// Represents a virtual Dex Machine
#[derive(Clone)]
pub struct VM {
    current_state: VMState,
    stack_frames: Vec<VMState>,
    heap: HashMap<u32, Value>,
    instances: HashMap<String, (NodeIndex, u32)>,
    dex_file: Arc<DexFile>,
    runtime: Vec<Arc<DexFile>>,
    resources: Arc<HashMap<String, Arc<BinaryObject>>>,
    builtins: Arc<HashMap<String, Arc<Class>>>,
    rng: Arc<Mutex<RefCell<StdRng>>>,
    break_points: Vec<Breakpoint>,
    stop_on_array_use: bool,
    stop_on_string_use: bool,
    stop_on_array_return: bool,
    stop_on_string_return: bool,
    skip_next_breakpoint: bool,
}

#[derive(Debug)]
pub enum VMException {
    RegisterNotFound(usize),
    StackFrameMissing,
    NoInstructionAtAddress(u32, usize),
    ClassNotFound(u16),
    OutOfMemory,
    InstanceNotFound(u32),
    IndexOutOfBounds,
    WrongNumberOfArguments,
    InvalidRegisterType,
    StackOverflow,
    MethodNotFound(String),
    InvalidMemoryAddress(u32),
    LinkerError,
    StaticDataNotFound(u32),
    Breakpoint(InstructionOffset, u32, BreakpointContext),
}
#[derive(Debug, Copy, Clone)]
pub enum BreakpointContext {
    ResultObjectRegister(u16),
    ArrayReg(u16, u16),
    StringReg(u16, u16),
    FieldSet(u16, u16),
    None,
}
#[derive(Debug, Clone)]
pub enum Breakpoint {
    ArrayUse,
    StringUse,
    StringReturn,
    ArrayReturn,
    RegisterAccess(u8),
    PrototypeResult(u16),
    FunctionResult(u16),
    Instruction(u32),
    FieldSet(u16),
    FieldGet(u16),
    FunctionEntry,
    FunctionExit,
}
use rand::SeedableRng;
/// Implementation for Virtual Machine
/// provides functions to emulate a function
// TODO: we need a way to set breakpoints for certain events e.g. when an array is used
impl VM {
    pub fn get_heap(&self) -> HashMap<u32, Value> {
        self.heap.clone()
    }
     pub fn get_instances(&self) -> HashMap<String, (NodeIndex, u32)> {
        self.instances.clone()
    }
    pub fn new(dex_file: Arc<DexFile>, runtime: Vec<Arc<DexFile>>, resources: Arc<HashMap<String, Arc<BinaryObject>>>) -> VM {
        let rng = rand::rngs::StdRng::seed_from_u64(0xff_ff_ff_ff);

        VM {
            current_state: VMState {
                pc: 0.into(),
                last_instruction_size: 0.into(),
                current_instruction_size: 0.into(),
                current_stackframe: vec![],
                return_reg: Register::Empty,
                num_params: 0,
                num_registers: 0,
                current_instructions: HashMap::new(),
                current_dex_file: dex_file.clone(),
                current_method_index: 0,
                vm_state: ExecutionState::Stopped,

                last_break_point_reg: 0,
            },
            stack_frames: vec![],
            heap: HashMap::new(),
            instances: HashMap::new(),
            dex_file,
            runtime,
            resources,
            builtins: VM_BUILTINS.clone(),
            rng: Arc::new(Mutex::new(RefCell::new(rng))),
            break_points: vec![],
            stop_on_array_use: false,
            stop_on_array_return: false,
            stop_on_string_return: false,
            stop_on_string_use: false,
            skip_next_breakpoint: false,
        }
    }

    pub fn set_breakpoint(&mut self, break_point: Breakpoint) {
        match break_point {
            Breakpoint::ArrayUse => {
                self.stop_on_array_use = true;
            }
            Breakpoint::StringUse => self.stop_on_string_use = true,
            Breakpoint::StringReturn => self.stop_on_string_return = true,
            Breakpoint::ArrayReturn => self.stop_on_array_return = true,
            _ => {}
        }
        self.break_points.push(break_point);
    }
    pub fn clear_breakpoints(&mut self) {
        self.stop_on_array_use = false;
        self.stop_on_string_use = false;
        self.stop_on_string_return = false;
        self.stop_on_array_return = false;
        self.break_points.clear();
    }
    pub fn get_breakpoints_clone(&self) -> Vec<Breakpoint> {
        self.break_points.clone()
    }
    pub fn continue_execution(&mut self, start_address: InstructionOffset) -> Result<(), VMException> {
        self.skip_next_breakpoint = true;
        self.execute(start_address)
    }
    pub fn reset(&mut self) {
        self.current_state.pc = 0.into();
        self.current_state.current_stackframe = vec![];
        self.current_state.return_reg = Register::Empty;
        self.current_state.num_params = 0;
        self.current_state.num_registers = 0;
        self.current_state.current_instructions = HashMap::new();
        self.current_state.current_dex_file = self.dex_file.clone();
        self.current_state.vm_state = ExecutionState::Stopped;
        self.current_state.current_method_index = 0;

        self.stack_frames.clear();
        self.heap.clear();
        self.instances.clear();
        self.skip_next_breakpoint = false;
    }
    pub fn new_instance(&mut self, ty: String, value: Value) -> Result<Register, VMException> {
        if let Some(heap_address) = self.malloc() {
            self.heap.insert(heap_address, value);
             Ok(Register::Reference(ty, heap_address))
        } else {
             Err(VMException::OutOfMemory)
        }
    }
    pub fn get_registers(&self) -> Vec<Register> {
        self.current_state.current_stackframe.clone()
    }
    pub fn get_current_state(&self) -> &VMState {
        &self.current_state
    }
    pub fn get_stack_frames(&self) -> &[VMState] {
        &self.stack_frames
    }
    pub fn get_instance(&self, reg: Register) -> Value {
        match reg {
            Register::Literal(l) => Value::Int(l),
            Register::Reference(_, address) => self.heap[&address].clone(),
            Register::Null => Value::Int(0),
            _ => Value::Int(0),
        }
    }
    pub fn start(
        &mut self,
        method_idx: u32,
        dex_file: &str,
        code_item: &CodeItem,
        arguments: Vec<Register>,
    ) -> Result<(), VMException> {
        self.current_state.pc = 0.into();
        self.current_state.return_reg = Register::Empty;
        self.current_state.current_stackframe = vec![];
        self.current_state.num_params = code_item.ins_size as usize;
        self.current_state.num_registers = code_item.register_size as usize;
        self.current_state.current_method_index = method_idx;
        self.stack_frames.clear();
        self.current_state.current_dex_file = if self.dex_file.identifier == dex_file {
            self.dex_file.clone()
        } else {
            self.runtime.iter().find(|df| df.identifier == dex_file).ok_or(VMException::LinkerError)?.clone()
        };

        if arguments.len() != self.current_state.num_params {
            return Err(VMException::WrongNumberOfArguments);
        }

        let start_params = self.current_state.num_registers - self.current_state.num_params;

        let mut registers = Vec::with_capacity(self.current_state.num_registers);
        for _ in 0..start_params {
            registers.push(Register::Empty);
        }
        for arg in arguments {
            registers.push(arg);
        }
        self.current_state.current_stackframe = registers;

        let code_hash = 
            code_item
                .insns
                .clone()
                .into_iter()
                .map(|ele| (ele.1, (ele.0, ele.2))).collect();
        self.current_state.current_instructions = code_hash;

        match self.execute(InstructionOffset(0)) {
            Ok(_) => {
                log::debug!("Function reached return statement");
                 Ok(())
            }
            Err(exception) => {
                 Err(exception)
            }
        }
    }

    pub fn lookup_method(&self, class_name: &str, method: &Method) -> Result<(Arc<DexFile>, &MethodData), VMException> {
         if let Some(method_data) = iterator!(self.runtime)
            .filter_map(|dex| dex.get_method_by_name_and_prototype(class_name, method.method_name.as_str(), &method.proto_name).map(|d| (dex.clone(),d)))
            .collect::<Vec<(Arc<DexFile>,&MethodData)>>()
            .first()
        {
            return Ok(method_data.clone());
        }

        Err(VMException::LinkerError)
    }

    fn get_method<'a>(
        &'a self,
        dex_file: &'a Arc<DexFile>,
        method_idx: u32,
    ) -> Result<(Arc<DexFile>, &MethodData), VMException> {
        // if let Some(method) = self.method_link_table.get(&method_idx) {
        //     return Ok(method);
        // }
        //first search current dexfile

        if let Some(method_data) = dex_file.get_method_by_idx(method_idx) {
            // self.method_link_table.insert(method_idx, *method_data);
            return Ok((dex_file.clone(), method_data));
        }
        let method =
            dex_file
                .methods
                .get(method_idx as usize)
                .ok_or_else(||VMException::MethodNotFound(format!(
                    "Method Index: {}",
                    method_idx
                )))?;
        let proto_type =  dex_file.protos.get(method.proto_idx as usize).ok_or_else(|| VMException::MethodNotFound(format!(
                    "Method Index: {}, Proto Index: {}",
                    method_idx,
                    method.proto_idx
                )))?.to_string(dex_file);
        let class_name = dex_file
            .get_type_name(method.class_idx)
            .ok_or(VMException::ClassNotFound(method.class_idx))?;

        if let Some(method_data) = iterator!(self.runtime)
            .filter_map(|dex| dex.get_method_by_name_and_prototype(class_name, method.method_name.as_str(), &proto_type).map(|d| (dex.clone(),d)))
            .collect::<Vec<(Arc<DexFile>,&MethodData)>>()
            .first()
        {
            // self.method_link_table.insert(method_idx, **method_data);
            return Ok(method_data.clone());
        }

        Err(VMException::LinkerError)
    }
    fn get_class(&self, dex_file: Arc<DexFile>, type_idx: u32) -> Result<Arc<Class>, VMException> {
        // if let Some(method) = self.class_link_table.get(&type_idx) {
        //     return Ok(method);
        // }
        //first search current dexfile
        if let Some(class) = dex_file.get_class_by_type(type_idx) {
            // self.class_link_table.insert(type_idx, class);
            return Ok(class);
        }

        let class_name = dex_file
            .get_type_name(type_idx as usize)
            .ok_or(VMException::ClassNotFound(type_idx as u16))?;

        if let Some(class) = iterator!(self.runtime)
            .filter_map(|dex| dex.get_class_by_name(class_name))
            .collect::<Vec<Arc<Class>>>()
            .first()
        {
            return Ok(class.clone());
        }
        if let Some(class) = iterator!(self.builtins)
            .filter_map(|(_, class)| {
                if class.class_name == class_name {
                    Some(class.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<Arc<Class>>>()
            .first()
        {
            return Ok(class.clone());
        }

        Err(VMException::ClassNotFound(type_idx as u16))
    }

    fn binary_op<T>(
        &mut self,
        dst: T,
        a: T,
        b: T,
        op: fn(i32, i32) -> Result<i32, VMException>,
    ) -> Result<(), VMException>
    where
        T: Into<usize> + Copy,
    {
        let mut new_register = Register::Empty;
        if let Register::Literal(a) = *self
            .current_state
            .current_stackframe
            .get(a.into())
            .ok_or_else(||VMException::RegisterNotFound(a.into()))?
        {
            if let Register::Literal(b) = *self
                .current_state
                .current_stackframe
                .get(b.into())
                .ok_or_else(||VMException::RegisterNotFound(b.into()))?
            {
                new_register = Register::Literal(op(a, b)?);
            }
        }
        self.update_register(dst.into(), new_register)
    }
    fn binary_op_lit<T, U>(
        &mut self,
        dst: T,
        a: T,
        lit: U,
        op: fn(i32, i32) -> i32,
    ) -> Result<(), VMException>
    where
        T: Into<usize> + Copy,
        U: Into<i32> + Copy,
    {
        let mut new_register = Register::Empty;
        if let Register::Literal(a) = *self
            .current_state
            .current_stackframe
            .get(a.into())
            .ok_or_else(||VMException::RegisterNotFound(a.into()))?
        {
            new_register = Register::Literal(op(a, lit.into()));
        }
        self.update_register(dst.into(), new_register)
    }

    fn execute(&mut self, start_address: InstructionOffset) -> Result<(), VMException> {
        let mut dex_file = self.current_state.current_dex_file.clone();
        let mut code_item = self.current_state.current_instructions.clone();
        let mut current_instruction =
            code_item
                .get(&start_address)
                .ok_or(VMException::NoInstructionAtAddress(
                    self.current_state.current_method_index,
                    start_address.into(),
                ))?;
        let mut method_idx = self.current_state.current_method_index;

        let mut steps = 0;

        self.current_state.vm_state = ExecutionState::Running;
        self.current_state.last_instruction_size = 0.into();


        //  self.current_state.current_method_index = method_idx;
        loop {
            steps += 1;
            if steps > 1000 {
                return Err(VMException::StackOverflow);
            }
            log::debug!("{:?}", self.current_state.current_stackframe);
            log::debug!("Executing: {:?} ", current_instruction.1);
            log::debug!("PC: {}", u32::from(self.current_state.pc));
            self.current_state.current_instruction_size = InstructionSize(current_instruction.0.0 / 2);
            match &current_instruction.1 {
                Instruction::Switch(reg, table_offset) => {
                    let reg_data = if let Some(Register::Literal(reg)) = self.current_state.current_stackframe.get(*reg as usize) {reg} else {
                        return Err(VMException::RegisterNotFound((*reg) as usize));
                    };
                    if let Some((_,Instruction::SwitchData(switch))) = code_item.get(&(self.current_state.pc + *table_offset)) {
                        if let Some(offset) = switch.targets.get(reg_data) {
                            self.current_state.pc += *offset as i32;
                            current_instruction = code_item.get(&self.current_state.pc).ok_or(
                                VMException::NoInstructionAtAddress(
                                    self.current_state.current_method_index,
                                    self.current_state.pc.into(),
                                ),
                            )?;
                            continue;
                        } 
                    }
                }
                Instruction::SwitchData(_) => {}
                Instruction::Throw(_) => {}
                Instruction::Nop => {}
                &Instruction::Move(dst, src) => {
                    let src_reg: u8 = src.into();
                    let dst_reg: u8 = dst.into();
                    let src = self
                        .current_state
                        .current_stackframe
                        .get(src_reg as usize)
                        .ok_or(VMException::RegisterNotFound(src_reg as usize))?
                        .to_owned();

                    self.update_register(dst_reg as usize, src)?;
                }
                &Instruction::MoveFrom16(dst_reg, src_reg) => {
                    let src = self
                        .current_state
                        .current_stackframe
                        .get(src_reg as usize)
                        .ok_or(VMException::RegisterNotFound(src_reg as usize))?
                        .to_owned();
                    self.update_register(dst_reg as usize, src)?;
                }
                &Instruction::Move16(dst_reg, src_reg) => {
                    let src = self
                        .current_state
                        .current_stackframe
                        .get(src_reg as usize)
                        .ok_or(VMException::RegisterNotFound(src_reg as usize))?
                        .to_owned();
                    self.update_register(dst_reg as usize, src)?;
                }
                Instruction::MoveWide(_, _) => {}
                Instruction::MoveWideFrom16(_, _) => {}
                Instruction::MoveWide16(_, _) => {}

                &Instruction::MoveObject(dst_reg, src_reg) => {
                    let src_reg: u8 = src_reg.into();
                    let dst_reg: u8 = dst_reg.into();
                    let src = self
                        .current_state
                        .current_stackframe
                        .get(src_reg as usize)
                        .ok_or(VMException::RegisterNotFound(src_reg as usize))?
                        .to_owned();
                    self.update_register(dst_reg as usize, src)?;
                }
                &Instruction::MoveObjectFrom16(dst_reg, src_reg) => {
                    let src = self
                        .current_state
                        .current_stackframe
                        .get(src_reg as usize)
                        .ok_or(VMException::RegisterNotFound(src_reg as usize))?
                        .to_owned();
                    self.update_register(dst_reg as usize, src)?;
                }
                &Instruction::MoveObject16(dst_reg, src_reg) => {
                    let src = self
                        .current_state
                        .current_stackframe
                        .get(src_reg as usize)
                        .ok_or(VMException::RegisterNotFound(src_reg as usize))?
                        .to_owned();
                    self.update_register(dst_reg as usize, src)?;
                }
                &Instruction::XorInt(dst_a, b) => {
                    let dst: u8 = dst_a.into();
                    let b: u8 = b.into();
                    self.binary_op(dst, dst, b, |a, b| Ok(a ^ b))?;
                }
                &Instruction::XorLong(_, _) => {}
                &Instruction::XorIntDst(dst, a, b) => {
                    self.binary_op(dst, a, b, |a, b| Ok(a ^ b))?;
                }
                &Instruction::XorIntDstLit8(dst, a, lit) => {
                    self.binary_op_lit(dst, a, lit, |a, b| a ^ b)?;
                }
                &Instruction::XorLongDst(_, _, _) => {}

                &Instruction::XorIntDstLit16(dst, a, lit) => {
                    let dst: u8 = dst.into();
                    let a: u8 = a.into();
                    self.binary_op_lit(dst, a, lit, |a, b| a ^ b)?;
                }
                &Instruction::RemIntDst(dst, a, b) => {
                    self.binary_op(dst, a, b, |a, b| {
                        if b == 0 {
                            return Err(VMException::InvalidRegisterType);
                        }
                        Ok(a % b)
                    })?;
                }
                &Instruction::RemLongDst(_, _, _) => {}
                &Instruction::RemInt(dst_a, b) => {
                    let dst_a: u8 = dst_a.into();
                    let b: u8 = b.into();

                    self.binary_op(dst_a, dst_a, b, |a, b| {
                        if b == 0 {
                            return Err(VMException::InvalidRegisterType);
                        }
                        Ok(a % b)
                    })?;
                }
                &Instruction::RemLong(_, _) => {}
                &Instruction::RemIntLit16(dst, a, lit) => {
                    let dst: u8 = dst.into();
                    let a: u8 = a.into();
                    self.binary_op_lit(dst, a, lit, |a, b| a.wrapping_rem(b))?;
                }
                &Instruction::RemIntLit8(dst, a, lit) => {
                    self.binary_op_lit(dst, a, lit, |a, b| a.wrapping_rem(b))?;
                }
                &Instruction::AddInt(dst_a, b) => {
                    let dst_a: u8 = dst_a.into();
                    let b: u8 = b.into();
                    self.binary_op(dst_a, dst_a, b, |a, b| Ok(a.wrapping_add(b)))?;
                }
                &Instruction::AddIntDst(dst, a, b) => {
                    self.binary_op(dst, a, b, |a, b| Ok( a.wrapping_add(b)))?;
                }
                &Instruction::AddIntLit8(dst, a, lit) => {
                    self.binary_op_lit(dst, a, lit, |a, b|  a.wrapping_add(b))?;
                }
                &Instruction::AddIntLit16(dst, a, lit) => {
                    let dst: u16 = dst.into();
                    let a: u16 = a.into();
                    self.binary_op_lit(dst, a, lit, |a, b| a.wrapping_add(b) )?;
                }
                Instruction::AddLong(_, _) => {}
                Instruction::AddLongDst(_, _, _) => {}

                &Instruction::MulInt(dst_a, b) => {
                    let dst_a: u8 = dst_a.into();
                    let b: u8 = b.into();
                    self.binary_op(dst_a, dst_a, b, |a, b| Ok(a.wrapping_mul(b)))?;
                }
                &Instruction::MulIntDst(dst, a, b) => {
                    self.binary_op(dst, a, b, |a, b| Ok(a.wrapping_mul(b)))?;
                }
                &Instruction::MulIntLit8(dst, a, lit) => {
                    self.binary_op_lit(dst, a, lit, |a, b| a.wrapping_mul(b))?;
                }
                &Instruction::MulIntLit16(dst, a, lit) => {
                    let dst: u16 = dst.into();
                    let a: u16 = a.into();
                    self.binary_op_lit(dst, a, lit, |a, b| a.wrapping_mul(b))?;
                }
                Instruction::MulLong(_, _) => {}
                Instruction::MulLongDst(_, _, _) => {}

                &Instruction::SubInt(dst_a, b) => {
                    let dst_a: u8 = dst_a.into();
                    let b: u8 = b.into();
                    self.binary_op(dst_a, dst_a, b, |a, b| Ok(a.wrapping_sub(b)))?;
                }
                &Instruction::SubIntDst(dst, a, b) => {
                    self.binary_op(dst, a, b, |a, b| Ok(a.wrapping_sub(b)))?;
                }
                &Instruction::SubIntLit8(dst, a, lit) => {
                    self.binary_op_lit(dst, a, lit, |a, b| a.wrapping_sub(b))?;
                }
                &Instruction::SubIntLit16(dst, a, lit) => {
                    let dst: u16 = dst.into();
                    let a: u16 = a.into();
                    self.binary_op_lit(dst, a, lit, |a, b| a.wrapping_sub(b))?;
                }
                Instruction::SubLong(_, _) => {}
                Instruction::SubLongDst(_, _, _) => {}

                &Instruction::AndInt(dst_a, b) => {
                    let dst_a: u8 = dst_a.into();
                    let b: u8 = b.into();
                    self.binary_op(dst_a, dst_a, b, |a, b| Ok(a & b))?;
                }
                &Instruction::AndIntDst(dst, a, b) => {
                    self.binary_op(dst, a, b, |a, b| Ok(a & b))?;
                }
                &Instruction::AndIntLit8(dst, a, lit) => {
                    self.binary_op_lit(dst, a, lit, |a, b| a & b)?;
                }
                &Instruction::AndIntLit16(dst, a, lit) => {
                    let dst: u16 = dst.into();
                    let a: u16 = a.into();
                    self.binary_op_lit(dst, a, lit, |a, b| a & b)?;
                }
                Instruction::AndLong(_, _) => {}
                Instruction::AndLongDst(_, _, _) => {}

                &Instruction::OrInt(dst_a, b) => {
                    let dst_a: u8 = dst_a.into();
                    let b: u8 = b.into();
                    self.binary_op(dst_a, dst_a, b, |a, b| Ok(a | b))?;
                }
                &Instruction::OrIntDst(dst, a, b) => {
                    self.binary_op(dst, a, b, |a, b| Ok(a | b))?;
                }
                &Instruction::OrIntLit8(dst, a, lit) => {
                    self.binary_op_lit(dst, a, lit, |a, b| a | b)?;
                }
                &Instruction::OrIntLit16(dst, a, lit) => {
                    let dst: u16 = dst.into();
                    let a: u16 = a.into();
                    self.binary_op_lit(dst, a, lit, |a, b| a | b)?;
                }
                Instruction::OrLong(_, _) => {}
                Instruction::OrLongDst(_, _, _) => {}

                &Instruction::Test(test, a, b, offset) => {
                    let a: u8 = a.into();
                    let b: u8 = b.into();
                    if let (Some(a), Some(b)) = (
                        self.current_state.current_stackframe.get(a as usize),
                        self.current_state.current_stackframe.get(b as usize),
                    ) {
                        if matches!(a, Register::Empty) | matches!(b, Register::Empty) {
                            return Err(VMException::RegisterNotFound(0));
                        }
                        match test {
                            coeus_models::models::TestFunction::Equal => {
                                if *a == *b {
                                    self.current_state.pc += offset as u32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::NotEqual => {
                                if *a != *b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::LessThan => {
                                if *a < *b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::LessEqual => {
                                if *a <= *b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::GreaterThan => {
                                if *a > *b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::GreaterEqual => {
                                if *a >= *b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                        }
                        current_instruction = code_item
                            .get(&self.current_state.pc)
                            .ok_or(VMException::NoInstructionAtAddress(
                                self.current_state.current_method_index,
                                self.current_state.pc.into(),
                            ))?;
                        continue;
                    } else {
                        return Err(VMException::RegisterNotFound(0));
                    }
                }
                &Instruction::TestZero(test, a, offset) => {
                    let a: u8 = a.into();
                    let b = Register::Literal(0);
                    if let Some(a) = self.current_state.current_stackframe.get(a as usize) {
                        match test {
                            coeus_models::models::TestFunction::Equal => {
                                if *a == b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::NotEqual => {
                                if *a != b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::LessThan => {
                                if *a < b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::LessEqual => {
                                if *a <= b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::GreaterThan => {
                                if *a > b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                            coeus_models::models::TestFunction::GreaterEqual => {
                                if *a >= b {
                                    self.current_state.pc += offset as i32;
                                } else {
                                    self.current_state.pc += (current_instruction.0.0) / 2;
                                }
                            }
                        }
                        current_instruction = code_item
                            .get(&self.current_state.pc)
                            .ok_or(VMException::NoInstructionAtAddress(
                                self.current_state.current_method_index,
                                self.current_state.pc.into(),
                            ))?;
                        continue;
                    } else {
                        return Err(VMException::RegisterNotFound(a as usize));
                    }
                }
                &Instruction::Goto8(offset) => {
                    self.current_state.pc += offset as i32;
                    current_instruction = code_item.get(&self.current_state.pc).ok_or(
                        VMException::NoInstructionAtAddress(
                            self.current_state.current_method_index,
                            self.current_state.pc.into(),
                        ),
                    )?;
                    continue;
                }
                &Instruction::Goto16(offset) => {
                    self.current_state.pc += offset as i32;
                    current_instruction = code_item.get(&self.current_state.pc).ok_or(
                        VMException::NoInstructionAtAddress(
                            self.current_state.current_method_index,
                            self.current_state.pc.into(),
                        ),
                    )?;
                    continue;
                }
                &Instruction::Goto32(offset) => {
                    self.current_state.pc += offset as i32;
                    current_instruction = code_item.get(&self.current_state.pc).ok_or(
                        VMException::NoInstructionAtAddress(
                            self.current_state.current_method_index,
                            self.current_state.pc.into(),
                        ),
                    )?;
                    continue;
                }
                &Instruction::ArrayGetByte(dst, array_reference, index)
                | &Instruction::ArrayGetChar(dst, array_reference, index) => {
                    if let Some(Register::Reference(_, array_reference)) = self
                        .current_state
                        .current_stackframe
                        .get(array_reference as usize)
                    {
                        if let Some(Value::Array(data)) = &self.heap.get(array_reference) {
                            if let Some(&Register::Literal(index)) =
                                self.current_state.current_stackframe.get(index as usize)
                            {
                                if let Some(byte) = data.get(index as usize) {
                                    let new_register = Register::Literal(*byte as i32);
                                    self.update_register(dst as usize, new_register)?;
                                } else {
                                    log::debug!("{:?}", current_instruction);
                                    log::debug!("{:?}", self.current_state.current_stackframe);
                                    return Err(VMException::IndexOutOfBounds);
                                }
                            }
                        } else {
                            log::debug!("{:?}", current_instruction);
                            log::debug!("{:?}", self.current_state.current_stackframe);
                            return Err(VMException::InstanceNotFound(*array_reference));
                        }
                    }
                }
                &Instruction::ArrayPutByte(src, array_reference, index)
                | &Instruction::ArrayPutChar(src, array_reference, index) => {
                    if let Some(Register::Reference(_, array_reference)) = self
                        .current_state
                        .current_stackframe
                        .get(array_reference as usize)
                    {
                        if let Some(Register::Literal(index)) =
                            self.current_state.current_stackframe.get(index as usize)
                        {
                            if let Some(Value::Array(data)) =
                                &mut self.heap.get_mut(array_reference)
                            {
                                if let Some(byte) = data.get_mut(*index as usize) {
                                    if let Some(&Register::Literal(val)) =
                                        self.current_state.current_stackframe.get(src as usize)
                                    {
                                        *byte = val as u8;
                                    }
                                } else {
                                    log::debug!("{:?}", current_instruction);
                                    log::debug!("{:?}", self.current_state.current_stackframe);
                                    return Err(VMException::IndexOutOfBounds);
                                }
                            } else {
                                log::debug!("{:?}", current_instruction);
                                log::debug!("{:?}", self.current_state.current_stackframe);
                                return Err(VMException::InstanceNotFound(*array_reference));
                            }
                        }
                    } else {
                        log::debug!("{:?}", current_instruction);
                        log::debug!("{:?}", self.current_state.current_stackframe);
                        return Err(VMException::RegisterNotFound(array_reference as usize));
                    }
                }
                Instruction::Invoke(_) => {}
                Instruction::InvokeType(a) => {
                    log::debug!("Invoke {}", a);
                }
                &Instruction::MoveResult(dst) => {
                    self.update_register(dst as usize, self.current_state.return_reg.clone())?;
                    self.current_state.return_reg = Register::Empty;
                }
                Instruction::MoveResultWide(_) => {}
                &Instruction::MoveResultObject(dst) => {
                    self.update_register(dst as usize, self.current_state.return_reg.clone())?;
                    self.current_state.return_reg = Register::Empty;
                }
                Instruction::ReturnVoid => {
                    if let Some(state) = self.stack_frames.pop() {
                        self.current_state = state;
                        current_instruction = self
                            .current_state
                            .current_instructions
                            .get(&self.current_state.pc)
                            .ok_or_else(|| {
                                log::error!(
                                    "RETURN POINTER ({}) NOT REFERENCING A INSTRUCTION! FATAL!",
                                    u32::from(self.current_state.pc)
                                );
                                log::error!(
                                    "Possible addresses: {:?}",
                                    self.current_state.current_instructions.keys()
                                );
                                log::error!("{:#?}", self.current_state);
                                log::error!("{:#?}", self.stack_frames);
                                VMException::NoInstructionAtAddress(
                                    self.current_state.current_method_index,
                                    self.current_state.pc.into(),
                                )
                            })?;
                        code_item = self.current_state.current_instructions.clone();
                        dex_file = self.current_state.current_dex_file.clone();
                      
                    } else {
                        self.current_state.vm_state = ExecutionState::Finished;
                        return Ok(());
                    }
                }
                &Instruction::Return(reg) => {
                    let register = self.current_state.current_stackframe.get(reg as usize);
                    if !self.skip_next_breakpoint {
                        if (self.stop_on_array_return || self.stop_on_string_return)
                            && matches!(register, Some(Register::Reference(ty,..)) if ty == "[B" || ty == "[C" || ty == "Ljava/lang/String;" )
                        {
                            return Err(VMException::Breakpoint(
                                 self.current_state.pc,
                                self.current_state.current_method_index,
                               
                                BreakpointContext::ResultObjectRegister(reg as u16),
                            ));
                        }
                    } else {
                        self.skip_next_breakpoint = false
                    }
                    if let Some(register) = register {
                        self.current_state.return_reg = (*register).clone();
                    }
                    
                    if let Some(mut state) = self.stack_frames.pop() {
                        state.return_reg = self.current_state.return_reg.clone();
                        self.current_state = state;

                        current_instruction = self
                            .current_state
                            .current_instructions
                            .get(&self.current_state.pc)
                            .unwrap();
                        code_item = self.current_state.current_instructions.clone();
                       dex_file = self.current_state.current_dex_file.clone();
                    } else {
                        self.current_state.vm_state = ExecutionState::Finished;
                        return Ok(());
                    }
                }
                Instruction::Const => {}
                &Instruction::ConstLit4(dst, lit) => {
                    let lit: u8 = lit.into();
                    let dst: u8 = dst.into();
                    let new_register = Register::Literal(lit.into());
                    self.update_register(dst, new_register)?;
                }
                &Instruction::ConstLit16(dst, lit) => {
                    let new_register = Register::Literal(lit.into());
                    self.update_register(dst, new_register)?;
                }
                &Instruction::ConstLit32(dst, lit) => {
                    let new_register = Register::Literal(lit);
                    self.update_register(dst, new_register)?;
                }
                Instruction::ConstWide => {}
                &Instruction::ConstString(dst, reference) => {
                    let const_str =  self.current_state.current_dex_file.get_string(reference).ok_or(VMException::StaticDataNotFound(reference as u32))?.to_string();
                   
                    let new_register = self.new_instance(
                        StringClass::class_name().to_string(),
                        Value::Object(StringClass::new(const_str.to_string())),
                    )?;
                    self.update_register(dst, new_register)?;
                    
                }
                Instruction::ConstStringJumbo(_, _) => {}
                Instruction::ConstClass(_, _) => {}
                &Instruction::IntToByte(dst, src) | &Instruction::IntToChar(dst, src) => {
                    let dst: u8 = dst.into();
                    let src: u8 = src.into();
                    if let Some(&Register::Literal(val)) =
                        self.current_state.current_stackframe.get(src as usize)
                    {
                        let new_val: i8 = val as i8;
                        let new_register = Register::Literal(new_val as i32);
                        self.update_register(dst as usize, new_register)?;
                    }
                }
                &Instruction::ArrayLength(dst, array_ref_reg) => {
                    let dst: u8 = dst.into();
                    let array_ref_reg: u8 = array_ref_reg.into();
                    if let Some(Register::Reference(_, array_reference)) = self
                        .current_state
                        .current_stackframe
                        .get(array_ref_reg as usize)
                    {
                        if let Some(Value::Array(array)) = self.heap.get(array_reference) {
                            let new_register = Register::Literal(array.len() as i32);
                            self.update_register(dst as usize, new_register)?;
                        }
                    } else {
                        return Err(VMException::InvalidRegisterType);
                    }
                }
                Instruction::NewInstance(dst, type_idx) => {
                    let class = self.get_class(dex_file.clone(), (*type_idx) as u32)?;

                    if let Some(heap_address) = self.malloc() {
                        self.heap
                            .insert(heap_address, Value::Object(ClassInstance::new(class)));
                        let new_register = Register::Reference(
                            dex_file
                                .get_type_name((*type_idx) as usize)
                                .unwrap()
                                .to_owned(),
                            heap_address,
                        );
                        let dst = self
                            .current_state
                            .current_stackframe
                            .get_mut((*dst) as usize)
                            .ok_or(VMException::RegisterNotFound((*dst) as usize))?;
                        *dst = new_register;
                    } else {
                        return Err(VMException::OutOfMemory);
                    }
                }
                Instruction::NewInstanceType(_) => {}
                &Instruction::NewArray(dst, size, ty) => {
                    let size: u8 = size.into();
                    let dst: u8 = dst.into();
                    if let Some(Register::Literal(size)) =
                        self.current_state.current_stackframe.get(size as usize)
                    {
                        if (*size as usize) > MAX_SIZE {
                            log::warn!("{} is too large to allocate, abbort execution", *size);
                            return Err(VMException::StackOverflow);
                        }
                        let arr = vec![0; *size as usize];
                        if let Some(heap_address) = self.malloc() {
                            self.heap.insert(heap_address, Value::Array(arr));
                            let type_name = if let Some(type_name) =
                                self.current_state.current_dex_file.get_type_name(ty)
                            {
                                type_name.to_string()
                            } else {
                                "[B".to_string()
                            };
                            let new_register = Register::Reference(type_name, heap_address as u32);
                            let dst = self
                                .current_state
                                .current_stackframe
                                .get_mut(dst as usize)
                                .ok_or(VMException::RegisterNotFound(dst as usize))?;
                            *dst = new_register;
                        }
                    }
                }
                Instruction::FilledNewArray(_, _, data) => {
                    if let Some(heap_address) = self.malloc() {
                        self.heap.insert(heap_address, Value::Array(data.clone()));
                        self.current_state.return_reg =
                            Register::Reference("[B".to_string(), heap_address);
                    } else {
                        return Err(VMException::OutOfMemory);
                    }
                }
                Instruction::FilledNewArrayRange(_, _, _) => {}
                &Instruction::FillArrayData(reference, data) => {
                    if let Some(Register::Reference(_, reference)) = self
                        .current_state
                        .current_stackframe
                        .get(reference as usize)
                    {
                        let val = self
                            .heap
                            .get_mut(reference)
                            .ok_or(VMException::InstanceNotFound(*reference))?;
                        if let Instruction::ArrayData(_, data) = &code_item
                            .get(&((self.current_state.pc + data as i32)))
                            .ok_or(VMException::InstanceNotFound(data as u32))?
                            .1
                        {
                            *val = Value::Array(data.clone());
                        }
                    }
                }
                //TODO: implement ranged opcodes
                Instruction::InvokeSuperRange(..)
                | Instruction::InvokeVirtualRange(..)
                | Instruction::InvokeDirectRange(..) => {}
                Instruction::InvokeStaticRange(..) => {}
                Instruction::InvokeInterfaceRange(..) => {}

                Instruction::InvokeSuper(_, _, _) => {}
                Instruction::InvokeVirtual(_, method_ref, argument_registers)
                | Instruction::InvokeDirect(_, method_ref, argument_registers) => {
                    if self.stack_frames.len() > 20 {
                        return Err(VMException::StackOverflow);
                    }

                    let mut arguments = vec![];
                    
                    for (regs, &arg) in argument_registers.iter().enumerate() {
                        let reg = self
                            .current_state
                            .current_stackframe
                            .get(arg as usize)
                            .ok_or(VMException::RegisterNotFound(arg as usize))?
                            .clone();
                        if (self.stop_on_array_use || self.stop_on_string_use) 
                        // just make sure we don't include breakpoints in non direct execution (e.g clinit from staticget)
                        && matches!(self.current_state.vm_state, ExecutionState::Running)
                        {
                            if regs as u32 >= self.current_state.last_break_point_reg {
                                if !self.skip_next_breakpoint {
                                    if let Register::Reference(_, ref reference) = reg {
                                        match self.heap.get(reference) {
                                            Some(Value::Array(_)) if self.stop_on_array_use => {
                                                self.current_state.vm_state =
                                                    ExecutionState::Paused;
                                                self.current_state.last_break_point_reg = regs as u32;
                                                return Err(VMException::Breakpoint(
                                                     self.current_state.pc,
                                                    self.current_state.current_method_index,
                                                    BreakpointContext::ArrayReg(
                                                        arg as u16,
                                                        *method_ref,
                                                    ),
                                                ));
                                            }
                                            Some(Value::Object(class_instance))
                                                if class_instance.class.class_name
                                                    == StringClass::class_name() =>
                                            {
                                                self.current_state.vm_state =
                                                    ExecutionState::Paused;
                                                self.current_state.last_break_point_reg = regs as u32;
                                                return Err(VMException::Breakpoint(
                                                     self.current_state.pc,
                                                    self.current_state.current_method_index,
                                                    BreakpointContext::StringReg(
                                                        arg as u16,
                                                        *method_ref,
                                                    ),
                                                ));
                                            }
                                            _ => {}
                                        }
                                    }
                                } else {
                                    log::debug!("Skip breakpoint");
                                    self.skip_next_breakpoint = false
                                }
                            }
                        }
                       
                        arguments.push(reg);
                    }
                    self.current_state.last_break_point_reg = 0;

                    //save current execution state
                    self.stack_frames.push(self.current_state.clone());

                    self.current_state.current_method_index = *method_ref as u32;
                    method_idx = self.current_state.current_method_index;

                    if let Ok((file,the_code)) = self.get_method(&dex_file, (*method_ref) as u32) {
                        
                        let the_code = the_code
                            .code
                            .as_ref()
                            .ok_or_else(||VMException::MethodNotFound(the_code.name.clone()))?
                            .to_owned();
                        let the_code_hash =
                            the_code
                                .insns
                                .clone()
                                .into_iter()
                                .map(|ele| (ele.1, (ele.0, ele.2))).collect();
                       
                        //build stackframe

                        self.current_state.current_dex_file = file;

                        self.current_state.pc = 0.into();
                        self.current_state.return_reg = Register::Empty;
                        //self.current_state.current_stackframe = vec![];
                        self.current_state.num_params = the_code.ins_size as usize;
                        self.current_state.num_registers = the_code.register_size as usize;

                        let start_params =
                            self.current_state.num_registers - self.current_state.num_params;

                        let mut registers = Vec::with_capacity(self.current_state.num_registers);
                        for _ in 0..start_params {
                            registers.push(Register::Empty);
                        }
                        for i in 0..self.current_state.num_params {
                            if let Some(arg) = arguments.get(i) {
                                registers.push((*arg).clone());
                            } else {
                                registers.push(Register::Empty);
                            }
                        }
                        self.current_state.current_stackframe = registers;

                        //push new instructions
                        self.current_state.current_instructions = the_code_hash;

                        //self.execute((*method_ref) as u32, 0)?;
                        dex_file = self.current_state.current_dex_file.clone();
                        code_item = self.current_state.current_instructions.clone();
                        current_instruction = code_item
                            .get(&self.current_state.pc)
                            .ok_or(VMException::NoInstructionAtAddress(
                                self.current_state.current_method_index,
                                self.current_state.pc.into(),
                            ))?;

                        continue;
                    } else {
                        self.invoke_runtime(dex_file.clone(), method_idx as u32, arguments)?;
                        if let Some(stack_frame) = self.stack_frames.pop() {
                            method_idx = stack_frame.current_method_index;
                            self.current_state.current_method_index = method_idx;
                        }
                    }
                }
                Instruction::InvokeStatic(arg_count, method_ref, argument_registers) => {
                    if self.stack_frames.len() > 20 {
                        return Err(VMException::StackOverflow);
                    }
                    
                    let mut arguments = vec![];
                    for (regs,&arg) in argument_registers.iter().enumerate() {
                        let reg = self
                            .current_state
                            .current_stackframe
                            .get(arg as usize)
                            .ok_or(VMException::RegisterNotFound(arg as usize))?
                            .clone();
                        if (self.stop_on_array_use || self.stop_on_string_use)
                        && matches!(self.current_state.vm_state, ExecutionState::Running)
                        //     .break_points
                        //     .iter()
                        //     .find(|a| matches!(a, Breakpoint::ArrayUse | Breakpoint::StringUse))
                        {
                            if regs as u32 >= self.current_state.last_break_point_reg {
                                if !self.skip_next_breakpoint {
                                    if let Register::Reference(_, ref reference) = reg {
                                        match self.heap.get(reference) {
                                            Some(Value::Array(_)) if self.stop_on_array_use => {
                                                self.current_state.vm_state =
                                                    ExecutionState::Paused;
                                                self.current_state.last_break_point_reg = regs as u32;
                                                return Err(VMException::Breakpoint(
                                                    self.current_state.pc,
                                                    method_idx,
                                                    
                                                    BreakpointContext::ArrayReg(
                                                        arg as u16,
                                                        *method_ref,
                                                    ),
                                                ));
                                            }
                                            Some(Value::Object(class_instance))
                                                if self.stop_on_string_use
                                                    && class_instance.class.class_name
                                                        == StringClass::class_name() =>
                                            {
                                                self.current_state.vm_state =
                                                    ExecutionState::Paused;
                                                self.current_state.last_break_point_reg = regs as u32;
                                                return Err(VMException::Breakpoint(
                                                    self.current_state.pc,
                                                    method_idx,
                                                    BreakpointContext::StringReg(
                                                        arg as u16,
                                                        *method_ref,
                                                    ),
                                                ));
                                            }
                                            _ => {}
                                        }
                                    }
                                } else {
                                    log::debug!("Skip breakpoint");
                                    self.skip_next_breakpoint = false
                                }
                            }
                        }
                       
                        arguments.push(reg);
                    }
                    self.current_state.last_break_point_reg = 0;
                    //save current execution state
                    self.stack_frames.push(self.current_state.clone());

                    self.current_state.current_method_index = *method_ref as u32;
                    method_idx = self.current_state.current_method_index;

                    if let Ok((file, the_code)) = self.get_method(&dex_file, *method_ref as u32) {
                        let method_name =&the_code.name;
                        let access_flags = &the_code.access_flags;

                        let the_code = the_code
                            .code
                            .as_ref()
                            .ok_or_else(||VMException::MethodNotFound(the_code.name.clone()))?
                            .to_owned();
                        let the_code_hash =
                            the_code
                                .insns
                                .clone()
                                .into_iter()
                                .map(|ele| (ele.1, (ele.0, ele.2))).collect();

                        if the_code.ins_size != u16::from(*arg_count) {
                            log::debug!("Expected: {} [{}] got {} [{} {}]", the_code.ins_size, the_code.register_size, arg_count,access_flags , method_name);
                            return Err(VMException::WrongNumberOfArguments);
                        }

                        self.current_state.current_dex_file = file;

                        self.current_state.pc = 0.into();
                        self.current_state.return_reg = Register::Empty;
                        //self.current_state.current_stackframe = vec![];
                        self.current_state.num_params = the_code.ins_size as usize;
                        self.current_state.num_registers = the_code.register_size as usize;

                        let start_params =
                            self.current_state.num_registers - self.current_state.num_params;

                        let mut registers = Vec::with_capacity(self.current_state.num_registers);
                        for _ in 0..start_params {
                            registers.push(Register::Empty);
                        }
                        for argument in arguments {
                            registers.push(argument);
                        }

                        self.current_state.current_stackframe = registers;

                        log::debug!(
                            "Running: {}",
                            self.get_method(&dex_file, method_idx).unwrap().1.name
                        );
                        //push new instructions
                        self.current_state.current_instructions = the_code_hash;
                        //self.execute((*method_ref) as u32,  0)?;
                        dex_file = self.current_state.current_dex_file.clone();
                        code_item = self.current_state.current_instructions.clone();
                        current_instruction = code_item
                            .get(&self.current_state.pc)
                            .ok_or(VMException::NoInstructionAtAddress(
                                self.current_state.current_method_index,
                                self.current_state.pc.into(),
                            ))?;
                        continue;
                    } else {
                        let result = self.invoke_runtime(dex_file.clone(), *method_ref as u32, arguments);
                        //we ignore it for now
                        match result {
                            Ok(_) => {
                                log::debug!("successfull execution of builtin");
                            },
                            Err(err) => {
                                log::warn!("Builtin failed with {:#?}. skipping", err);
                            }
                        }
                        if let Some(stack_frame) = self.stack_frames.pop() {
                            method_idx = stack_frame.current_method_index;
                            self.current_state.current_method_index = method_idx;
                        }
                    }
                }
                Instruction::InvokeInterface(_, _, _) => {}

                Instruction::NotImpl(_, _) => {}
                Instruction::ArrayData(_, _) => {}

                Instruction::StaticGet(_, _) => {}
                Instruction::StaticGetWide(_, _) => {}
                &Instruction::StaticGetObject(dst, field_idx) => {
                    let field = if let Some(field) = dex_file.fields.get(field_idx as usize) {field} else {
                        return Err(VMException::ClassNotFound(0));
                    };
                    let class_name = if let Some(c) = dex_file.get_type_name(field.class_idx as usize) {
                        c.to_string()
                    } else {
                        return Err(VMException::ClassNotFound(field.class_idx as u16));
                    };
                    let field_name = format!("{}->{}", class_name, field.name);
                    if !self.instances.contains_key(&field_name)
                        && !matches!(
                            self.current_state.vm_state,
                            ExecutionState::RunningStaticInitializer
                        )
                    {
                        if let Some(field) = dex_file.fields.get(field_idx as usize) {
                             if let Some(class) = iterator!(self
                                .dex_file
                                .classes)
                                .find_any(|c| c.class_idx == field.class_idx as u32)
                                {
                                if let Some(data) = class.get_data_for_static_field(field_idx as u32) {
                                    //data.
                                    match &data.value_type {
                                        ValueType::String => {
                                            let str = data.get_string_id();
                                            if let Some(str) = self.dex_file.get_string(str as usize) {
                                                let str = str.to_string(); 
                                                let instance = runtime::StringClass::new(str);
                                                if let Ok(Register::Reference(_, memory_address)) = self.new_instance(StringClass::class_name().to_string(), Value::Object(instance)) {
                                                    self.instances.insert(field_name.clone(), (NodeIndex::new(0), memory_address));
                                                };
                                            }
                                        },
                                        _ => {
                                            
                                        }
                                    }
                                    
                                } 
                            }
                            if let Some(class) = iterator!(self
                                .dex_file
                                .classes)
                                .find_any(|c| c.class_idx == field.class_idx as u32)
                                {
                                
                                if let Some(static_init) =
                                    iterator!(class.codes).find_any(|m| m.name == "<clinit>")
                                {
                                    // set pc one back so we can come bakc here
                                   
                                    // if self.current_state.pc - self.current_state.last_instruction_size
                                    //     >= 0
                                    // {
                                    //     self.current_state.pc -=
                                    //         self.current_state.last_instruction_size;
                                    // }
                                    self.current_state.vm_state =
                                        ExecutionState::RunningStaticInitializer;
                                    self.stack_frames.push(self.current_state.clone());

                                    self.current_state.pc = 0.into();
                                    self.current_state.num_params = 0;
                                    self.current_state.vm_state =
                                        ExecutionState::StaticInitializer;
                                    self.current_state.num_registers =
                                        static_init.code.as_ref().unwrap().register_size as usize;
                                    let mut registers =
                                        Vec::with_capacity(self.current_state.num_registers);
                                    for _ in 0..self.current_state.num_registers {
                                        registers.push(Register::Empty);
                                    }
                                    self.current_state.current_stackframe = registers;
                                    let the_code_hash = 
                                        static_init
                                            .code
                                            .as_ref()
                                            .unwrap()
                                            .insns
                                            .clone()
                                            .into_iter()
                                            .map(|ele| (ele.1, (ele.0, ele.2))).collect();
                                    

                                    self.current_state.last_instruction_size = 0.into();
                                    log::debug!("Field not found, run static initializer");
                                    {
                                        self.current_state.current_method_index =
                                            static_init.method.method_idx as u32;
                                        method_idx = self.current_state.current_method_index;
                                        //push new instructions
                                        self.current_state.current_instructions = the_code_hash;

                                        dex_file = self.current_state.current_dex_file.clone();
                                        code_item = self.current_state.current_instructions.clone();
                                        current_instruction = code_item
                                            .get(&self.current_state.pc)
                                            .ok_or(VMException::NoInstructionAtAddress(
                                                self.current_state.current_method_index,
                                                self.current_state.pc.into(),
                                            ))?;
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                    if !self.instances.contains_key(&field_name) {
                        //so we ran our static class initializer, but still no reference.
                        // Let's check if it is a Context or Application and return a pseudo reference
                        if let Some(field) = dex_file.fields.get(field_idx as usize) {
                            if let Some(type_name) = dex_file.get_type_name(field.type_idx as usize)
                            {
                                if type_name == "Landroid/content/Context;"
                                    || type_name == "Landroid/app/Application;"
                                    || type_name == "Ljava/nio/charset/Charset;"
                                {
                                    let instance =
                                        ClassInstance::new(VM_BUILTINS[type_name].clone());
                                    if let Ok(Register::Reference(_, memory_address)) = self
                                        .new_instance(
                                            type_name.to_string(),
                                            Value::Object(instance),
                                        )
                                    {
                                        self.instances.insert(
                                            field_name.clone(),
                                            (NodeIndex::new(0), memory_address),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    if matches!(self.current_state.vm_state, ExecutionState::RunningStaticInitializer) {
                        self.current_state.vm_state = ExecutionState::Running;
                    }

                    if let Some((_, val)) = self.instances.get(&field_name) {
                        match self.heap.get(val) {
                            Some(Value::Object(class)) => {
                                let new_register =
                                    Register::Reference(class.class.class_name.to_string(), *val);
                                self.update_register(dst, new_register)?;
                            }
                            Some(Value::Array(_)) => {
                                let new_register = Register::Reference("[B".to_string(), *val);
                                self.update_register(dst, new_register)?;
                            }
                            _ => {
                                return Err(VMException::InvalidMemoryAddress(*val));
                            }
                        }
                    } else {
                        return Err(VMException::StaticDataNotFound(field_idx as u32));
                    }
                }
                Instruction::StaticGetBoolean(_, _) => {}
                Instruction::StaticGetByte(_, _) => {}
                Instruction::StaticGetChar(_, _) => {}
                Instruction::StaticGetShort(_, _) => {}
                Instruction::StaticPut(_, _) => {}
                Instruction::StaticPutWide(_, _) => {}
                &Instruction::StaticPutObject(src, field_idx) => {
                     let field = if let Some(field) = dex_file.fields.get(field_idx as usize) {field} else {
                        return Err(VMException::ClassNotFound(0));
                    };
                    let class_name = if let Some(c) = dex_file.get_class_by_type(field.class_idx as u32) {
                        c.class_name.clone()
                    } else {
                        return Err(VMException::ClassNotFound(field.class_idx as u16));
                    };
                    let field_name = format!("{}->{}", class_name, field.name);

                    if let Some(&Register::Reference(_, address)) =
                        self.current_state.current_stackframe.get(src as usize)
                    {
                        let _class_resource = self.heap.get(&address).unwrap();
                        if !self.skip_next_breakpoint {
                            if iterator!(self.break_points).any(|bp| matches!(bp, Breakpoint::FieldSet(idx) if *idx == field_idx)){
                                match _class_resource {
                                    Value::Array(_) => {
                                        return Err(VMException::Breakpoint(
                                             self.current_state.pc,
                                            self.current_state.current_method_index,
                                           BreakpointContext::FieldSet(src as u16, field_idx)
                                        ))
                                    }
                                    Value::Object(val) if val.class.class_name == StringClass::class_name() => {
                                        return Err(VMException::Breakpoint(
                                            self.current_state.pc,
                                            self.current_state.current_method_index,
                                            BreakpointContext::FieldSet(src as u16, field_idx)
                                        ))
                                    }
                                   _ => {}
                                }
                            }
                        } else {
                            self.skip_next_breakpoint = false;
                        }
                        match self.instances.get_mut(&field_name) {
                            Some((_, val)) => {
                                *val = address;
                            }
                            None => {
                                log::debug!("Insert instances");
                                self.instances
                                    .insert(field_name, (NodeIndex::new(0), address));
                            }
                        }
                    } else {
                        return Err(VMException::RegisterNotFound(src as usize));
                    }
                }
                Instruction::StaticPutBoolean(_, _) => {}
                Instruction::StaticPutByte(_, _) => {}
                Instruction::StaticPutChar(_, _) => {}
                Instruction::StaticPutShort(_, _) => {}
                Instruction::InstanceGet(dst, obj, field_id) => {
                    let dst: u8 = (*dst).into();
                    let obj: u8 = (*obj).into();
                    let field_id = *field_id;

                    let field = if let Some(field) = dex_file.fields.get(field_id as usize) {field} else {
                        return Err(VMException::ClassNotFound(0));
                    };
                    let class_name = if let Some(c) = dex_file.get_class_by_type(field.class_idx as u32) {
                        c.class_name.clone()
                    } else {
                        return Err(VMException::ClassNotFound(field.class_idx as u16));
                    };
                    let field_name = format!("{}->{}", class_name, field.name);

                    if let Some(Register::Reference(_, instance)) =
                        self.current_state.current_stackframe.get(obj as usize)
                    {
                        if let Some(Value::Object(class_instance)) = self.heap.get(instance) {
                            if let Some(field_instance) =
                                class_instance.instances.get(&field_name)
                            {
                                let val = self
                                    .heap
                                    .get(field_instance)
                                    .ok_or(VMException::StaticDataNotFound(field_id as u32))?
                                    .clone();
                                if let Value::Int(val) = val {
                                    self.update_register(dst, Register::Literal(val))?;
                                }
                            } else {
                                log::debug!(
                                    "{:?} was not found",
                                    dex_file.get_string(
                                        dex_file.fields[field_id as usize].name_idx as usize
                                    )
                                );
                                self.update_register(dst, Register::Literal(0))?;
                            }
                        }
                    }
                }
                Instruction::InstanceGetWide(_, _, _) => {}
                &Instruction::InstanceGetObject(dst, instance, field_id) => {
                    let dst: u8 = dst.into();
                    let instance: u8 = instance.into();

                    let field = if let Some(field) = dex_file.fields.get(field_id as usize) {field} else {
                        return Err(VMException::ClassNotFound(0));
                    };
                    let class_name = if let Some(c) = dex_file.get_class_by_type(field.class_idx as u32) {
                        c.class_name.clone()
                    } else {
                        return Err(VMException::ClassNotFound(field.class_idx as u16));
                    };
                    let field_name = format!("{}->{}", class_name, field.name);

                    if let Some(Register::Reference(_, instance)) =
                        self.current_state.current_stackframe.get(instance as usize)
                    {
                        if let Some(Value::Object(class_instance)) = self.heap.get(instance) {
                            if let Some(field_instance) =
                                class_instance.instances.get(&field_name)
                            {
                                let new_register = Register::Reference(
                                    class_instance.class.class_name.to_string(),
                                    *field_instance,
                                );
                                self.update_register(dst, new_register)?;
                            }
                        }
                    }
                }
                Instruction::InstanceGetBoolean(_, _, _) => {}
                Instruction::InstanceGetByte(_, _, _) => {}
                Instruction::InstanceGetChar(_, _, _) => {}
                Instruction::InstanceGetShort(_, _, _) => {}
                &Instruction::InstancePut(src, instance, field_id) => {
                    let src: u8 = src.into();
                    let instance: u8 = instance.into();

                    let field = if let Some(field) = dex_file.fields.get(field_id as usize) {field} else {
                        return Err(VMException::ClassNotFound(0));
                    };
                    let class_name = if let Some(c) = dex_file.get_class_by_type(field.class_idx as u32) {
                        c.class_name.clone()
                    } else {
                        return Err(VMException::ClassNotFound(field.class_idx as u16));
                    };
                    let field_name = format!("{}->{}", class_name, field.name);

                    if let (Some(Register::Literal(src)), Some(Register::Reference(_, instance))) = (
                        self.current_state.current_stackframe.get(src as usize),
                        self.current_state.current_stackframe.get(instance as usize),
                    ) {
                        if let Some(Value::Object(class_instance)) = self.heap.get(instance) {
                            let class_instance = class_instance.clone();
                            if let Some(field_instance) =
                                class_instance.instances.get(&field_name)
                            {
                                let field_instance = *field_instance;
                                self.heap
                                    .entry(field_instance)
                                    .and_modify(|e| *e = Value::Int(*src));
                            } else {
                                let address;
                                {
                                    address = self.malloc();
                                    if let Some(address) = address {
                                        self.heap.entry(address).or_insert(Value::Int(*src));
                                    }
                                }
                                if let Some(Value::Object(class_instance)) =
                                    self.heap.get_mut(instance)
                                {
                                    if let Some(address) = address {
                                        class_instance.instances.insert(field_name, address);
                                    }
                                }
                            }
                        }
                    }
                }
                Instruction::InstancePutWide(_, _, _) => {}
                &Instruction::InstancePutObject(src, instance, field_id) => {
                    let src: u8 = src.into();
                    let instance: u8 = instance.into();

                    let field = if let Some(field) = dex_file.fields.get(field_id as usize) {field} else {
                        return Err(VMException::ClassNotFound(0));
                    };
                    let class_name = if let Some(c) = dex_file.get_class_by_type(field.class_idx as u32) {
                        c.class_name.clone()
                    } else {
                        return Err(VMException::ClassNotFound(field.class_idx as u16));
                    };
                    let field_name = format!("{}->{}", class_name, field.name);

                    if let (
                        Some(Register::Reference(_, src)),
                        Some(Register::Reference(_, instance)),
                    ) = (
                        self.current_state.current_stackframe.get(src as usize),
                        self.current_state.current_stackframe.get(instance as usize),
                    ) {
                        if let Some(Value::Object(class_instance)) = self.heap.get_mut(instance) {
                            let field_instance =
                                class_instance.instances.entry(field_name).or_insert(0);
                            *field_instance = *src;
                        }
                    }
                }
                Instruction::InstancePutBoolean(_, _, _) => {}
                Instruction::InstancePutByte(_, _, _) => {}
                Instruction::InstancePutChar(_, _, _) => {}
                Instruction::InstancePutShort(_, _, _) => {}
            }
            if matches!(self.current_state.vm_state, ExecutionState::Finished) {
                return Ok(());
            }
            if !matches!(self.current_state.vm_state, ExecutionState::RunningStaticInitializer) {
                self.current_state.last_instruction_size = InstructionSize((current_instruction.0.0) / 2);
                self.current_state.pc += self.current_state.last_instruction_size;
            }
            current_instruction = code_item.get(&self.current_state.pc).ok_or(
                VMException::NoInstructionAtAddress(
                    self.current_state.current_method_index,
                    self.current_state.pc.into(),
                ),
            )?;
        }
    }

    fn invoke_runtime(
        &mut self,
        dex_file: Arc<DexFile>,
        method_idx: u32,
        arguments: Vec<Register>,
    ) -> Result<(), VMException> {
        invoke_runtime(self, dex_file, method_idx, arguments)?;
        Ok(())
    }

    fn update_register<T>(&mut self, dst: T, new_register: Register) -> Result<(), VMException>
    where
        T: Into<usize> + Copy,
    {
        let dst = self
            .current_state
            .current_stackframe
            .get_mut(dst.into())
            .ok_or_else(||VMException::RegisterNotFound(dst.into()))?;
        *dst = new_register;
        Ok(())
    }

    fn malloc(&self) -> Option<u32> {
        let mut tries = 0;
        if let Ok(rng) = self.rng.lock() {
            loop {
                let heap_address = rng.borrow_mut().gen::<u32>();
                if !self.heap.contains_key(&heap_address) {
                    log::debug!("allocated memory at {}", heap_address);
                    return Some(heap_address);
                }
                tries += 1;
                if tries > 10 {
                    return None;
                }
            }
        } else { 
            None
        }
    }

    pub fn get_return_object(&self) -> Option<Value> {
        match self.current_state.return_reg {
            Register::Literal(l) => Some(Value::Int(l)),
            Register::Reference(_, reference) => if let Some(instance) = self.heap.get(&reference) {
                Some(instance.to_owned())
            } else {None},
            _ => None
        }
    }
}

#[derive(Clone, Debug)]
pub enum Register {
    Literal(i32),
    LiteralWide(i64),
    Paired(u32),
    Reference(String, u32),
    Empty,
    Null,
}

impl Ord for Register {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let left = self.get_int();
        let right = other.get_int();
        left.cmp(&right)
    }
}
impl Eq for Register {}
impl PartialOrd for Register {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        let left = self.get_int();
        let right = other.get_int();
        left.partial_cmp(&right)
    }
}
impl PartialEq for Register {
    fn eq(&self, other: &Self) -> bool {
        let left = self.get_int();
        let right = other.get_int();
        left.eq(&right)
    }
}

impl Register {
    pub fn get_int(&self) -> i64 {
        match self {
            &Register::Literal(a) => a as i64,
            &Register::Reference(_, a) => a as i64,
            Register::Empty => 0,
            Register::Null => 0,
            _ => 0,
        }
    }
}
