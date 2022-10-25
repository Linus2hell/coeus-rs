// Copyright (c) 2022 Ubique Innovation AG <https://www.ubique.ch>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use coeus::{
    coeus_analysis::analysis::{
        self,
        dex::find_cross_reference_array,
        instruction_flow::{Branch, InstructionFlow, LastInstruction, State},
        Context,
    },
    coeus_emulation::vm::{runtime::StringClass, Register, Value, VM},
    coeus_models::models::{
        self, AccessFlags, BinaryObject, DexFile, InstructionOffset, TestFunction,
    },
};
use pyo3::{
    exceptions::PyRuntimeError,
    types::{PyDict, PyTuple},
};
use pyo3::{prelude::*, types::PyList};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use regex::Regex;

use crate::{
    parse::{AnalyzeObject, Runtime},
    vm::{DexVm, VmResult},
};
#[pyclass]
/// Evidences represent found objects in the binaries. They can represent different objects
pub struct Evidence {
    pub(crate) evidence: analysis::Evidence,
}

#[pyclass]
/// Gather last instructions
pub struct Instruction {
    instruction: LastInstruction,
}

#[pyclass]
pub struct Flow {
    instruction_flow: InstructionFlow,
    method: Method,
}
#[pyclass]
pub struct FlowBranch {
    branch: Branch,
    method: Method,
}
#[pyclass]
pub struct FlowState {
    state: State,
    _method: Method,
}
#[pyclass]
pub struct Branching {
    test: TestFunction,
    left: Option<coeus::coeus_analysis::analysis::instruction_flow::Value>,
    right: Option<coeus::coeus_analysis::analysis::instruction_flow::Value>,
    offset_true: InstructionOffset,
    offset_false: InstructionOffset,
    method: Method,
    is_tainted: bool,
}

#[pymethods]
impl FlowState {
    pub fn print_state(&self) -> String {
        format!("{:?}", self.state)
    }
}

#[pymethods]
impl FlowBranch {
    pub fn get_state(&self) -> FlowState {
        FlowState {
            state: self.branch.state.clone(),
            _method: self.method.clone(),
        }
    }
    pub fn get_pc(&self) -> u32 {
        self.branch.pc.0
    }
    pub fn get_current_instruction(&self) -> PyResult<String> {
        let code = self
            .method
            .method_data
            .as_ref()
            .and_then(|m| m.code.as_ref())
            .map(|a| &a.insns)
            .unwrap();
        let i = code.iter().find(|(_, offset, _)| offset.0 == self.get_pc());
        let (_, _, instruction) = if let Some(i) = i {
            i
        } else {
            return Err(PyRuntimeError::new_err("There is no instruction"));
        };
        Ok(instruction.disassembly_from_opcode(
            self.get_pc() as i32,
            &mut HashMap::new(),
            self.method.file.clone(),
        ))
    }
}

#[pymethods]
impl Flow {
    #[new]
    pub fn new(method: &crate::analysis::Method, conservative: bool) -> PyResult<Self> {
        let code = if let Some(md) = method.method_data.as_ref().and_then(|a| a.code.as_ref()) {
            md
        } else {
            return Err(PyRuntimeError::new_err("Could not find method data"));
        };
        let instruction_flow = InstructionFlow::new(code.clone(), method.file.clone(), conservative);
        Ok(Self {
            method: method.clone(),
            instruction_flow,
        })
    }

    pub fn reset(&mut self, start: u32) {
        self.instruction_flow.reset(start);
    }
    pub fn next_instruction(&mut self) -> PyResult<()> {
        if self.instruction_flow.is_done() {
            return Err(PyRuntimeError::new_err("All done"));
        }
        self.instruction_flow.next_instruction();
        Ok(())
    }
    pub fn get_state(&self) -> Vec<FlowBranch> {
        let states = self.instruction_flow.get_all_branches();
        states
            .iter()
            .map(|s| FlowBranch {
                branch: s.clone(),
                method: self.method.clone(),
            })
            .collect()
    }
}

#[pymethods]
impl Branching {
    pub fn get_method(&self) -> Method {
        self.method.clone()
    }
    pub fn has_dead_branch(&self) -> bool {
        let result = self.left.as_ref().map(|a| a.is_constant()).unwrap_or(false)
            && self.right.as_ref().map(|a| a.is_constant()).unwrap_or(true);
        result && !self.is_tainted
    }
    pub fn branch_offset(&self) -> Option<u32> {
        if !self.has_dead_branch() {
            return None;
        }
        match (&self.left, &self.right) {
            (Some(left), Some(right)) => {
                let left = left.try_get_number()?;
                let right = right.try_get_number()?;
                match self.test {
                    TestFunction::Equal => {
                        if left == right {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::NotEqual => {
                        if left != right {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::LessThan => {
                        if left < right {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::LessEqual => {
                        if left <= right {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::GreaterThan => {
                        if left > right {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::GreaterEqual => {
                        if left >= right {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                }
            }
            (Some(left), None) => {
                let number = left.try_get_number()?;
                match self.test {
                    TestFunction::Equal => {
                        if number == 0 {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::NotEqual => {
                        if number != 0 {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::LessThan => {
                        if number < 0 {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::LessEqual => {
                        if number <= 0 {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::GreaterThan => {
                        if number > 0 {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                    TestFunction::GreaterEqual => {
                        if number >= 0 {
                            Some(self.offset_true.0)
                        } else {
                            Some(self.offset_false.0)
                        }
                    }
                }
            }
            _ => None,
        }
    }
}

#[pymethods]
impl Instruction {
    pub fn __str__(&self) -> String {
        if let LastInstruction::FunctionCall {
            name,
            signature: _,
            class_name,
            class: _,
            method: _,
            args,
            result,
        } = &self.instruction
        {
            format!(
                "{}->{}({}) : {}",
                class_name,
                name,
                args.iter()
                    .map(|a| format!("{}", a))
                    .collect::<Vec<_>>()
                    .join(","),
                result
                    .as_ref()
                    .map(|a| format!("{}", a))
                    .unwrap_or_else(|| "Void".to_string())
            )
        } else {
            format!("{:?}", self.instruction)
        }
    }
    pub fn execute(&mut self, py: Python, vm: &mut DexVm) -> PyResult<Py<PyAny>> {
        if let Ok(mut vm) = vm.vm.lock() {
            let result = self.instruction.execute(&mut vm).unwrap();
            return match result {
                coeus::coeus_analysis::analysis::instruction_flow::Value::String(s) => {
                    Ok(s.into_py(py))
                }
                coeus::coeus_analysis::analysis::instruction_flow::Value::Number(n) => {
                    Ok(n.into_py(py))
                }
                coeus::coeus_analysis::analysis::instruction_flow::Value::Byte(b) => {
                    Ok(b.into_py(py))
                }
                coeus::coeus_analysis::analysis::instruction_flow::Value::Bytes(bytes) => {
                    Ok(bytes.into_py(py))
                }
                coeus::coeus_analysis::analysis::instruction_flow::Value::Boolean(b) => {
                    Ok(b.into_py(py))
                }
                _ => Err(PyRuntimeError::new_err("Unknown result")),
            };
        }
        Err(PyRuntimeError::new_err("Could not execute"))
    }
    pub fn get_argument_types(&self) -> Vec<String> {
        if let LastInstruction::FunctionCall {
            name: _name,
            signature: _signature,
            class_name: _class_name,
            class: _class,
            method: _method,
            args,
            result: _result,
        } = &self.instruction
        {
            let mut type_names = vec![];
            for arg in args {
                match arg {
                    analysis::instruction_flow::Value::String(_) => {
                        type_names.push("Ljava/lang/String;".to_string())
                    }
                    analysis::instruction_flow::Value::Number(_) => {
                        type_names.push("I".to_string())
                    }
                    analysis::instruction_flow::Value::Boolean(_) => {
                        type_names.push("Z".to_string())
                    }
                    analysis::instruction_flow::Value::Char(_) => type_names.push("C".to_string()),
                    analysis::instruction_flow::Value::Byte(_) => type_names.push("B".to_string()),
                    analysis::instruction_flow::Value::Bytes(_) => {
                        type_names.push("[B".to_string())
                    }
                    analysis::instruction_flow::Value::Variable(l) => type_names.push(format!(
                        "{:?}",
                        Instruction {
                            instruction: (**l).to_owned()
                        }
                        .get_argument_types()
                    )),
                    analysis::instruction_flow::Value::Unknown { ty } => {
                        type_names.push(ty.to_string())
                    }
                    analysis::instruction_flow::Value::Object { ty } => {
                        type_names.push(ty.to_string())
                    }
                    analysis::instruction_flow::Value::Invalid
                    | analysis::instruction_flow::Value::Empty => {}
                }
            }
            type_names
        } else {
            vec![]
        }
    }
    pub fn get_string_arguments(&self) -> Vec<String> {
        if let LastInstruction::FunctionCall {
            name: _name,
            signature: _signature,
            class_name: _class_name,
            class: _class,
            method: _method,
            args,
            result: _result,
        } = &self.instruction
        {
            args.iter()
                .filter_map(|a| {
                    if let analysis::instruction_flow::Value::String(s) = a {
                        Some(s.to_string())
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            vec![]
        }
    }
    pub fn get_arguments_as_value(&self) -> Vec<InstructionValue> {
        if let LastInstruction::FunctionCall { args, .. } = &self.instruction {
            args.iter()
                .map(|value| InstructionValue {
                    value: value.clone(),
                })
                .collect()
        } else {
            vec![]
        }
    }
    pub fn get_function_name(&self) -> PyResult<String> {
        if let LastInstruction::FunctionCall {
            name: _, signature, ..
        } = &self.instruction
        {
            Ok(signature.clone())
        } else {
            Err(PyRuntimeError::new_err("Instruction is no function"))
        }
    }
}
#[pyclass]
#[derive(Clone)]
pub struct InstructionValue {
    value: coeus::coeus_analysis::analysis::instruction_flow::Value,
}
#[pymethods]
impl InstructionValue {
    pub fn get_value(&self, py: Python) -> Py<PyAny> {
        match &self.value {
            coeus::coeus_analysis::analysis::instruction_flow::Value::Bytes(a) => a.to_object(py),
            coeus::coeus_analysis::analysis::instruction_flow::Value::Object { ty } => {
                ty.to_object(py)
            }
            coeus::coeus_analysis::analysis::instruction_flow::Value::Number(i) => i.to_object(py),
            coeus::coeus_analysis::analysis::instruction_flow::Value::Boolean(b) => b.to_object(py),
            coeus::coeus_analysis::analysis::instruction_flow::Value::Byte(b) => b.to_object(py),
            analysis::instruction_flow::Value::String(s) => s.to_object(py),
            analysis::instruction_flow::Value::Char(c) => c.to_object(py),
            analysis::instruction_flow::Value::Variable(instruction) => Instruction {
                instruction: *instruction.clone(),
            }
            .into_py(py),
            analysis::instruction_flow::Value::Unknown { ty } => ty.to_object(py),
            analysis::instruction_flow::Value::Invalid => "invalid".to_object(py),
            analysis::instruction_flow::Value::Empty => "empty".to_object(py),
        }
    }
}
#[pyclass]
#[derive(Clone)]
/// This represents a method, which can also be executed.
pub struct Method {
    pub(crate) method: Arc<models::Method>,
    pub(crate) method_data: Option<models::MethodData>,
    pub(crate) file: Arc<DexFile>,
    pub(crate) class: Arc<models::Class>,
}

#[pyclass]
#[derive(Clone)]
/// This represents a class.
pub struct Class {
    class: Arc<models::Class>,
    file: Arc<DexFile>,
}

#[pyclass]
/// This represents a string
pub struct DexString {
    pub(crate) string: String,
    pub(crate) _place: analysis::Location,
}

#[pyclass]
#[derive(Clone)]
/// Helper class for field access evidences
pub struct FieldAccess {
    field: DexField,
    place: analysis::Location,
    instruction: String,
}

#[pymethods]
impl FieldAccess {
    pub fn get_instruction(&self) -> &str {
        self.instruction.as_str()
    }
    pub fn get_field(&self) -> DexField {
        self.field.clone()
    }
    pub fn get_class(&self) -> PyResult<Class> {
        if let (Some(class), Some(file)) = (self.place.get_class(), self.place.get_dex_file()) {
            Ok(Class { class, file })
        } else {
            Err(PyRuntimeError::new_err("Could not get class"))
        }
    }
    pub fn get_function(&self) -> PyResult<Method> {
        if let analysis::Location::DexMethod(method_idx, f) = &self.place {
            let class = self.get_class()?;
            let method_data = f.get_method_by_idx(*method_idx).cloned();
            let method = if let Some(method) = f
                .methods
                .iter()
                .find(|m| m.method_idx == (*method_idx as u16))
            {
                method.clone()
            } else {
                return Err(PyRuntimeError::new_err("Could not find method"));
            };
            Ok(Method {
                method,
                method_data,
                file: f.clone(),
                class: class.class,
            })
        } else {
            Err(PyRuntimeError::new_err(
                "Something is wrong with this method",
            ))
        }
    }
}

#[pyclass]
#[derive(Clone)]
/// This represents a field.
pub struct DexField {
    pub(crate) field: Arc<models::Field>,
    pub(crate) access_flags: Option<AccessFlags>,
    pub(crate) field_name: Option<String>,
    pub(crate) file: Arc<DexFile>,
    pub(crate) dex_class: Class,
}

#[pyclass]
pub struct NativeSymbol {
    pub(crate) file: Arc<BinaryObject>,
    pub(crate) symbol: String,
    /// TODO: we need to calculate the adress as if we were a linker
    pub(crate) address: u64,
    pub(crate) size: u64,
    pub(crate) is_export: bool,
}

#[pymethods]
impl NativeSymbol {
    pub fn symbol(&self) -> String {
        self.symbol.clone()
    }
    pub fn is_export(&self) -> bool {
        self.is_export
    }
    pub fn address(&self) -> u64 {
        self.address
    }
    pub fn get_function_bytes(&self) -> Vec<u8> {
        if !self.is_export {
            return vec![];
        }
        (&self.file.data()[self.address as usize..(self.address + self.size - 1) as usize]).to_vec()
    }
}

#[pymethods]
impl Evidence {
    pub fn cross_references(&self, ao: &AnalyzeObject) -> Vec<Evidence> {
        if let Some(place) = self.evidence.get_context() {
            return find_cross_reference_array(&[place.to_owned()], &ao.files)
                .iter()
                .map(|evidence| Evidence {
                    evidence: evidence.to_owned(),
                })
                .collect();
        }
        vec![]
    }

    pub fn downcast(&self, py: Python) -> PyResult<Py<PyAny>> {
        if let Ok(fa) = self.as_field_access() {
            return Ok(fa.into_py(py));
        }
        if let Ok(method) = self.as_method() {
            return Ok(method.into_py(py));
        }
        if let Ok(class) = self.as_class() {
            return Ok(Py::new(py, class)?.into_ref(py).into());
        }
        if let Ok(field) = self.as_field() {
            return Ok(Py::new(py, field)?.into_ref(py).into());
        }
        if let Ok(string) = self.as_string() {
            return Ok(Py::new(py, string)?.into_ref(py).into());
        }
        if let Ok(native) = self.as_native_symbol() {
            return Ok(native.into_py(py));
        }

        Err(PyRuntimeError::new_err(
            "Type does not match any defined ones",
        ))
    }

    pub fn as_native_symbol(&self) -> PyResult<NativeSymbol> {
        let context = if matches!(self.evidence, analysis::Evidence::CrossReference(..)) {
            self.evidence.get_place_context()
        } else {
            self.evidence.get_context()
        };
        if let Some(Context::NativeLib(file, name, address, exported, sym)) = context {
            Ok(NativeSymbol {
                file: file.clone(),
                symbol: name.to_string(),
                address: *address,
                size: sym.st_size,
                is_export: *exported,
            })
        } else {
            Err(PyRuntimeError::new_err("Not a native symbol"))
        }
    }
    /// Cast the Evidence as a method by extracting the Context
    #[pyo3(text_signature = "($self)")]
    pub fn as_method(&self) -> PyResult<Method> {
        let context = if matches!(self.evidence, analysis::Evidence::CrossReference(..)) {
            self.evidence.get_place_context()
        } else {
            self.evidence.get_context()
        };
        match context {
            Some(Context::DexMethod(method, file)) => {
                let class = if let Some(class) = file.get_class_by_type(method.class_idx) {
                    class
                } else {
                    let mut default_class = models::Class::default();
                    let type_name = file.get_type_name(method.class_idx).unwrap_or("UNKNOWN");
                    default_class.class_name = type_name.to_string();
                    default_class.class_idx = method.class_idx as u32;
                    Arc::new(default_class)
                };
                let method_data = class
                    .codes
                    .iter()
                    .find(|a| {
                        a.method.method_name == method.method_name
                            && a.method.proto_name == method.proto_name
                    })
                    .cloned();

                Ok(self::Method {
                    method: method.clone(),
                    method_data,
                    file: file.clone(),
                    class,
                })
            }
            _ => Err(PyRuntimeError::new_err("not a method")),
        }
    }
    /// Cast the Evidence as a method by extracting the Context
    #[pyo3(text_signature = "($self)")]
    pub fn as_class(&self) -> PyResult<self::Class> {
        let context = if matches!(self.evidence, analysis::Evidence::CrossReference(..)) {
            self.evidence.get_place_context()
        } else {
            self.evidence.get_context()
        };
        match context {
            Some(Context::DexClass(clazz, file)) => Ok(self::Class {
                class: clazz.clone(),
                file: file.clone(),
            }),
            Some(Context::DexType(t, name, file)) => {
                let class = if let Some(class) = file.get_class_by_type(*t) {
                    class
                } else {
                    Arc::new(models::Class {
                        class_name: name.to_string(),
                        class_idx: *t,
                        dex_identifier: file.get_identifier().to_string(),
                        ..Default::default()
                    })
                };

                Ok(self::Class {
                    class,
                    file: file.clone(),
                })
            }
            _ => Err(PyRuntimeError::new_err("not a class")),
        }
    }
    /// Cast the Evidence as a string by extracting the Context
    #[pyo3(text_signature = "($self)")]
    pub fn as_string(&self) -> PyResult<self::DexString> {
        if let analysis::Evidence::String(s) = &self.evidence {
            Ok(DexString {
                string: s.content.clone(),
                _place: s.place.clone(),
            })
        } else {
            Err(PyRuntimeError::new_err("not a string"))
        }
    }
    #[pyo3(text_signature = "($self)")]
    pub fn as_field_access(&self) -> PyResult<self::FieldAccess> {
        if let analysis::Evidence::Instructions(instructions) = &self.evidence {
            let (field, file) =
                if let analysis::Context::DexField(field, file) = &instructions.context {
                    (field, file)
                } else {
                    return Err(PyRuntimeError::new_err("not a field"));
                };
            let class = if let Some(class) = file.get_class_by_type(field.class_idx) {
                class
            } else {
                let mut default_class = models::Class::default();
                let type_name = file.get_type_name(field.class_idx).unwrap_or("UNKNOWN");
                default_class.class_name = type_name.to_string();
                default_class.class_idx = field.class_idx as u32;
                Arc::new(default_class)
            };
            let mut access_flags = None;
            if let Some(class_data) = &class.class_data {
                for i_f in &class_data.instance_fields {
                    if let Some(instance_field) = file.fields.get(i_f.field_idx as usize) {
                        if instance_field == field {
                            access_flags = Some(i_f.access_flags);
                            break;
                        }
                    }
                }
                for s_f in &class_data.static_fields {
                    if let Some(static_field) = file.fields.get(s_f.field_idx as usize) {
                        if static_field == field {
                            access_flags = Some(s_f.access_flags);
                            break;
                        }
                    }
                }
            }

            let dex_field = DexField {
                field: field.clone(),
                access_flags,
                file: file.clone(),
                field_name: if let analysis::Evidence::String(s) = &self.evidence {
                    Some(s.content.clone())
                } else {
                    None
                },
                dex_class: Class {
                    class,
                    file: file.clone(),
                },
            };
            Ok(FieldAccess {
                field: dex_field,
                place: instructions.place.clone(),
                instruction: instructions
                    .instructions
                    .get(0)
                    .cloned()
                    .unwrap_or_default(),
            })
        } else {
            Err(PyRuntimeError::new_err("not a field"))
        }
    }
    /// Cast the Evidence as a string by extracting the Context
    #[pyo3(text_signature = "($self)")]
    pub fn as_field(&self) -> PyResult<self::DexField> {
        let context = if matches!(self.evidence, analysis::Evidence::CrossReference(..)) {
            self.evidence.get_place_context()
        } else {
            self.evidence.get_context()
        };
        if let Some(Context::DexField(field, file)) = context {
            let class = if let Some(class) = file.get_class_by_type(field.class_idx) {
                class
            } else {
                let mut default_class = models::Class::default();
                let type_name = file.get_type_name(field.class_idx).unwrap_or("UNKNOWN");
                default_class.class_name = type_name.to_string();
                default_class.class_idx = field.class_idx as u32;
                Arc::new(default_class)
            };
            let mut access_flags = None;
            if let Some(class_data) = &class.class_data {
                for i_f in &class_data.instance_fields {
                    if let Some(instance_field) = file.fields.get(i_f.field_idx as usize) {
                        if instance_field == field {
                            access_flags = Some(i_f.access_flags);
                            break;
                        }
                    }
                }
                for s_f in &class_data.static_fields {
                    if let Some(static_field) = file.fields.get(s_f.field_idx as usize) {
                        if static_field == field {
                            access_flags = Some(s_f.access_flags);
                            break;
                        }
                    }
                }
            }

            Ok(DexField {
                field: field.clone(),
                access_flags,
                file: file.clone(),
                field_name: if let analysis::Evidence::String(s) = &self.evidence {
                    Some(s.content.clone())
                } else {
                    None
                },
                dex_class: Class {
                    class,
                    file: file.clone(),
                },
            })
        } else {
            Err(PyRuntimeError::new_err("not a field"))
        }
    }
}

#[pymethods]
impl DexField {
    pub fn try_get_value(&self, dex_vm: &mut DexVm) -> PyResult<VmResult> {
        if !self
            .access_flags
            .map(|a| a.contains(AccessFlags::STATIC))
            .unwrap_or(false)
        {
            return Err(PyRuntimeError::new_err("field is not static"));
        }
        if let Result::Ok(method) = self.dex_class.get_method("<clinit>") {
            let mut vm = if let Ok(vm) = dex_vm.vm.lock() {
                vm
            } else {
                return Err(PyRuntimeError::new_err("Could not acquire lock on vm"));
            };
            if let Some(method_data) = method.method_data {
                if vm
                    .start(
                        method_data.method_idx,
                        &method.file.identifier,
                        &method_data.code.unwrap(),
                        vec![],
                    )
                    .is_err()
                {
                    return Err(PyRuntimeError::new_err("VM failed"));
                }
                let fqdn = self.fqdn();
                drop(vm);
                dex_vm.get_static_field(&fqdn)
            } else {
                Err(PyRuntimeError::new_err("No method data"))
            }
        } else {
            Err(PyRuntimeError::new_err("No clinit found"))
        }
    }
    pub fn field_name(&self) -> String {
        self.field_name.clone().unwrap_or_else(|| String::from(""))
    }
    pub fn fqdn(&self) -> String {
        format!("{}->{}", self.dex_class.class.class_name, self.field_name())
    }
    #[getter(dex_class)]
    pub fn get_class(&self) -> Class {
        self.dex_class.clone()
    }
    pub fn get_field_access(&self, ao: &AnalyzeObject) -> Vec<FieldAccess> {
        let field_context = analysis::Context::DexField(self.field.clone(), self.file.clone());
        find_cross_reference_array(&[field_context], &ao.files)
            .iter()
            .filter_map(|evidence| {
                if let analysis::Evidence::Instructions(instructions) = evidence {
                    Some(FieldAccess {
                        field: self.clone(),
                        place: instructions.place.clone(),
                        instruction: instructions
                            .instructions
                            .get(0)
                            .cloned()
                            .unwrap_or_default(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}

#[pymethods]
impl DexString {
    pub fn content(&self) -> String {
        self.string.clone()
    }
}

#[pymethods]
impl Method {
    pub fn __hash__(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.signature().hash(&mut hasher);
        hasher.finish()
    }
    pub fn __richcmp__(&self, other: &Method, _op: pyo3::basic::CompareOp) -> bool {
        self.signature() == other.signature()
    }
    pub fn __repr__(&self) -> String {
        self.signature()
    }

    pub fn get_argument_types_string(&self) -> Vec<String> {
        let proto = &self.file.protos[self.method.proto_idx as usize];
        let mut arguments = vec![];
        for arg in &proto.arguments {
            let type_name =
                self.file.strings[self.file.types[*arg as usize] as usize].to_str_lossy();
            arguments.push(friendly_name(&type_name));
        }
        arguments
    }
    pub fn get_return_type(&self) -> String {
        let proto = &self.file.protos[self.method.proto_idx as usize];
        proto.get_return_type(&self.file)
    }
    #[staticmethod]
    pub fn find_all_branch_decisions_array(methods: &PyList, vm: &mut DexVm, conservative: bool) -> Vec<Branching> {
        let methods: Vec<Method> = methods
            .into_iter()
            .flat_map(|a| a.extract::<Method>().ok())
            .collect::<Vec<_>>();

        let m = if let Ok(vm) = vm.vm.lock() {
            vm.clone()
        } else {
            return vec![];
        };
        let branchings = methods
            .par_iter()
            .flat_map(|a| {
                let m = m.clone();
                let mut dex_vm = DexVm {
                    vm: Arc::new(Mutex::new(m)),
                };
                a.find_all_branch_decisions(&mut dex_vm, conservative)
            })
            .collect::<Vec<_>>();
        branchings
    }

    pub fn find_all_branch_decisions(&self, vm: &mut DexVm, conservative: bool) -> Vec<Branching> {
        let mut branchings = vec![];
        if let Some(code) = &self.method_data {
            if let Some(code) = &code.code {
                let mut instruction_flow = InstructionFlow::new(code.clone(), self.file.clone(), conservative);
                let branches = instruction_flow.get_all_branch_decisions();
                for mut b in branches {
                    if b.state.tainted || b.state.loop_count.iter().any(|(_, value)| *value > 1) {
                        continue;
                    }
                    let (instruction_size, instruction) =
                        if let Some(a) = instruction_flow.get_instruction(&b.pc) {
                            a
                        } else {
                            continue;
                        };
                    let mut vm_lock = if let Ok(vm) = vm.vm.lock() {
                        vm
                    } else {
                        return branchings;
                    };
                    if let coeus::coeus_models::models::Instruction::Test(
                        test,
                        left,
                        right,
                        offset,
                    ) = instruction
                    {
                        let left = if let Ok(val) =
                            b.state.registers[u8::from(left) as usize].try_get_value(&mut vm_lock)
                        {
                            val
                        } else {
                            b.state.registers[u8::from(left) as usize].clone()
                        };
                        let right = if let Ok(val) =
                            b.state.registers[u8::from(right) as usize].try_get_value(&mut vm_lock)
                        {
                            val
                        } else {
                            b.state.registers[u8::from(right) as usize].clone()
                        };

                        branchings.push(Branching {
                            test,
                            left: Some(left),
                            right: Some(right),
                            offset_true: b.pc + offset as i32,
                            offset_false: b.pc + (instruction_size.0 / 2),
                            method: self.clone(),
                            is_tainted: b.state.tainted
                                || b.state.loop_count.iter().any(|(_, value)| *value > 1),
                        });
                    } else if let coeus::coeus_models::models::Instruction::TestZero(
                        test,
                        left,
                        offset,
                    ) = instruction
                    {
                        let left = if let Ok(val) =
                            b.state.registers[left as usize].try_get_value(&mut vm_lock)
                        {
                            val
                        } else {
                            b.state.registers[left as usize].clone()
                        };

                        branchings.push(Branching {
                            test,
                            left: Some(left),
                            right: None,
                            offset_false: b.pc + (instruction_size.0 / 2),
                            offset_true: b.pc + offset as i32,
                            method: self.clone(),
                            is_tainted: b.state.tainted
                                || b.state.loop_count.iter().any(|(_, value)| *value > 1),
                        })
                    }
                }
            }
        }
        branchings
    }
    pub fn find_method_call(&self, signature: &str) -> Vec<Instruction> {
        let mut f_calls = vec![];
        let regex = Regex::new(signature).unwrap();
        if let Some(code) = &self.method_data {
            if let Some(code) = &code.code {
                let mut instruction_flow = InstructionFlow::new(code.clone(), self.file.clone(), true);
                let branches = instruction_flow
                    .find_all_calls_regex(&regex)
                    .iter()
                    .filter_map(|a| a.state.last_instruction.clone())
                    .map(|last_instruction| Instruction {
                        instruction: last_instruction,
                    })
                    .collect::<Vec<Instruction>>();
                f_calls.extend(branches);
            }
        }
        f_calls
    }
    pub fn cross_references(&self, ao: &AnalyzeObject) -> Vec<Evidence> {
        let place = Context::DexMethod(self.method.clone(), self.file.clone());
        return find_cross_reference_array(&[place], &ao.files)
            .iter()
            .map(|evidence| Evidence {
                evidence: evidence.to_owned(),
            })
            .collect();
    }

    /// Return the name of the function
    pub fn name(&self) -> &str {
        &self.method.method_name
    }
    pub fn proto_type(&self) -> &str {
        &self.method.proto_name
    }
    pub fn code(&self) -> String {
        if let Some(code) = &self.method_data {
            code.get_disassembly(&self.file)
        } else {
            String::from("")
        }
    }
    pub fn signature(&self) -> String {
        format!(
            "{}->{}{}",
            self.get_class().name(),
            self.name(),
            self.proto_type()
        )
    }
    pub fn get_class(&self) -> Class {
        Class {
            class: self.class.clone(),
            file: self.file.clone(),
        }
    }
    #[args(args = "*", kwargs = "**")]
    pub fn __call__(
        &self,
        py: Python,
        args: &PyTuple,
        kwargs: Option<&PyDict>,
    ) -> PyResult<VmResult> {
        if let Some(method_data) = &self.method_data {
            let proto_idx = method_data.method.proto_idx;
            let proto = self.file.protos[proto_idx as usize].clone();
            let mut vm_arguments: Vec<Register> = vec![];
            let runtime = if let Some(vm_runtime) = kwargs.and_then(|a| a.get_item("runtime")) {
                let runtime: Runtime = vm_runtime.extract()?;
                runtime.runtime
            } else {
                vec![]
            };
            let py_vm = if let Some(vm) = kwargs.and_then(|a| a.get_item("vm")) {
                vm.extract()?
            } else {
                let vm = VM::new(self.file.clone(), runtime, Arc::new(HashMap::new()));
                Py::new(
                    py,
                    DexVm {
                        vm: Arc::new(Mutex::new(vm)),
                    },
                )?
            };
            let py_cell: &PyCell<DexVm> = py_vm.into_ref(py);
            let mut vm_mut = py_cell.borrow_mut();
            let vm = &mut vm_mut.vm;
            let mut vm = if let Ok(vm) = vm.lock() {
                vm
            } else {
                return Err(PyRuntimeError::new_err("Could not lock vm"));
            };
            for (actual_arg, python_arg) in proto.arguments.iter().zip(args) {
                if let Some(type_name) = self.file.get_type_name(*actual_arg) {
                    match type_name {
                        "Z" => {
                            let boolean_arg: bool = python_arg.extract()?;
                            vm_arguments.push(Register::Literal(if boolean_arg { 1 } else { 0 }));
                        }
                        "B" => {
                            let byte_arg: i8 = python_arg.extract()?;
                            vm_arguments.push(Register::Literal(byte_arg as i32));
                        }
                        "S" => {
                            let short_arg: i16 = python_arg.extract()?;
                            vm_arguments.push(Register::Literal(short_arg as i32));
                        }
                        "C" => {
                            let char_arg: char = python_arg.extract()?;
                            vm_arguments.push(Register::Literal(char_arg as i32));
                        }
                        "I" => {
                            let int_arg: i32 = python_arg.extract()?;
                            vm_arguments.push(Register::Literal(int_arg as i32));
                        }
                        "J" => {
                            let long_arg: i64 = python_arg.extract()?;
                            vm_arguments.push(Register::LiteralWide(long_arg));
                        }

                        "[B" => {
                            let byte_array_arg: &[u8] = python_arg.extract()?;
                            if let Ok(instance) = vm.new_instance(
                                "[B".to_string(),
                                Value::Array(byte_array_arg.to_vec()),
                            ) {
                                vm_arguments.push(instance);
                            } else {
                                return Err(PyRuntimeError::new_err(
                                    "Could not create instance of byte array",
                                ));
                            }
                        }
                        "Ljava/lang/String;" => {
                            let string_arg: &str = python_arg.extract()?;
                            if let Ok(instance) = vm.new_instance(
                                StringClass::class_name().to_string(),
                                Value::Object(StringClass::new(string_arg.to_string())),
                            ) {
                                vm_arguments.push(instance);
                            } else {
                                return Err(PyRuntimeError::new_err(
                                    "Could not create instance of byte array",
                                ));
                            }
                        }
                        ty => {
                            return Err(PyRuntimeError::new_err(format!(
                                "{} Type not supported",
                                ty
                            )))
                        }
                    }
                } else {
                    return Err(PyRuntimeError::new_err("Type not found in dexfile..."));
                }
            }
            if !method_data.access_flags.contains(AccessFlags::STATIC) {
                vm_arguments.insert(0, Register::Reference("".to_string(), 0));
            }

            if let Some(code) = &method_data.code {
                match vm.start(
                    method_data.method.method_idx as u32,
                    &self.file.identifier,
                    code,
                    vm_arguments,
                ) {
                    Ok(_) => {}
                    Err(e) => {
                        return Err(PyRuntimeError::new_err(format!(
                            "VM failed: {:?}\n [{:#?}]",
                            e,
                            vm.get_current_state()
                        )))
                    }
                }
            } else {
                return Err(PyRuntimeError::new_err("No method definition found"));
            }
            let output_register = vm.get_current_state().return_reg.clone();
            let output = vm.get_instance(output_register);
            drop(vm);
            Ok(VmResult {
                data: output,
                vm: vm_mut.clone(),
            })
        } else {
            Err(PyRuntimeError::new_err("No method definition found"))
        }
    }
}

pub(crate) fn friendly_name(name: &str) -> String {
    let without_prefix = name.strip_prefix('L').unwrap_or_default();
    let with_dots = without_prefix.replace('/', ".");
    with_dots.strip_suffix(';').unwrap_or_default().to_string()
}

#[pymethods]
impl Class {
    pub fn __hash__(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.name().hash(&mut hasher);
        hasher.finish()
    }
    pub fn __str__(&self) -> String {
        self.name().to_string()
    }
    pub fn __repr__(&self) -> String {
        self.name().to_string()
    }
    pub fn __richcmp__(&self, other: &Class, _op: pyo3::basic::CompareOp) -> bool {
        self.name() == other.name()
    }
    pub fn find_implementations(&self, ao: &AnalyzeObject) -> Vec<Class> {
        if let Some((md, _)) = ao
            .files
            .get_multi_dex_from_dex_identifier(&self.file.identifier)
        {
            let impls = md.get_implementations_for(&self.class);
            return impls
                .iter()
                .map(|(f, c)| Class {
                    class: c.clone(),
                    file: f.clone(),
                })
                .collect();
        }
        vec![]
    }
    /// Return the name of the class
    pub fn name(&self) -> &str {
        &self.class.class_name
    }
    pub fn friendly_name(&self) -> String {
        let without_prefix = self.name().strip_prefix('L').unwrap_or_default();
        let with_dots = without_prefix.replace('/', ".");
        with_dots
            .strip_suffix(';')
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    pub fn code(&self, obj: &AnalyzeObject) -> String {
        if let Some((multi_dex_file, _)) = &obj
            .files
            .get_multi_dex_from_dex_identifier(self.file.get_identifier())
        {
            self.class.get_disassembly(multi_dex_file)
        } else {
            "COUlD NOT FIND DEX_FILE".to_string()
        }
    }
    pub fn find_method_call(&self, signature: &str) -> Vec<Instruction> {
        let methods = self.get_methods();
        let regex = Regex::new(signature).unwrap();
        let function_calls = methods
            .par_iter()
            .flat_map(|m| {
                let mut f_calls = vec![];
                if let Some(code) = &m.method_data {
                    if let Some(code) = &code.code {
                        let mut instruction_flow =
                            InstructionFlow::new(code.clone(), self.file.clone(), true);
                        let branches = instruction_flow
                            .find_all_calls_regex(&regex)
                            .iter()
                            .filter_map(|a| a.state.last_instruction.clone())
                            .map(|last_instruction| Instruction {
                                instruction: last_instruction,
                            })
                            .collect::<Vec<Instruction>>();
                        f_calls.extend(branches);
                    }
                }
                f_calls
            })
            .collect();
        function_calls
    }
    pub fn get_methods(&self) -> Vec<Method> {
        let mut methods = vec![];

        for method in &self.class.codes {
            methods.push(Method {
                method: method.method.clone(),
                method_data: Some(method.clone()),
                file: self.file.clone(),
                class: self.class.clone(),
            });
        }
        methods
    }
    pub fn get_method(&self, name: &str) -> PyResult<Method> {
        self.class
            .codes
            .iter()
            .find(|a| a.method.method_name == name)
            .map(|method| Method {
                method: method.method.clone(),
                method_data: Some(method.clone()),
                file: self.file.clone(),
                class: self.class.clone(),
            })
            .ok_or_else(|| PyRuntimeError::new_err("method not founud"))
    }

    pub fn get_field(&self, name: &str) -> PyResult<DexField> {
        let class_data = if let Some(c_d) = self.class.class_data.as_ref() {
            c_d
        } else {
            return Err(PyRuntimeError::new_err("field not found"));
        };
        class_data
            .instance_fields
            .iter()
            .find(|a| {
                let f = &self.file.fields[a.field_idx as usize];
                f.name == name
            })
            .map(|a| DexField {
                field: self.file.fields[a.field_idx as usize].clone(),
                access_flags: Some(a.access_flags),
                field_name: Some(self.file.fields[a.field_idx as usize].name.clone()),
                file: self.file.clone(),
                dex_class: self.clone(),
            })
            .ok_or_else(|| PyRuntimeError::new_err("field not found"))
    }
    pub fn get_static_field(&self, name: &str) -> PyResult<DexField> {
        let class_data = if let Some(c_d) = self.class.class_data.as_ref() {
            c_d
        } else {
            return Err(PyRuntimeError::new_err("field not found"));
        };
        class_data
            .static_fields
            .iter()
            .find(|a| {
                let f = &self.file.fields[a.field_idx as usize];
                f.name == name
            })
            .map(|a| DexField {
                field: self.file.fields[a.field_idx as usize].clone(),
                access_flags: Some(a.access_flags),
                field_name: Some(self.file.fields[a.field_idx as usize].name.clone()),
                file: self.file.clone(),
                dex_class: self.clone(),
            })
            .ok_or_else(|| PyRuntimeError::new_err("field not found"))
    }

    pub fn __getitem__(&self, name: &str) -> PyResult<Method> {
        self.class
            .codes
            .iter()
            .find(|a| a.method.method_name == name)
            .map(|method| Method {
                method: method.method.clone(),
                method_data: Some(method.clone()),
                file: self.file.clone(),
                class: self.class.clone(),
            })
            .ok_or_else(|| PyRuntimeError::new_err("method not founud"))
    }
}

pub(crate) fn register(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<Evidence>()?;
    m.add_class::<Method>()?;
    m.add_class::<Class>()?;
    m.add_class::<Instruction>()?;
    m.add_class::<Branching>()?;
    m.add_class::<NativeSymbol>()?;
    m.add_class::<FieldAccess>()?;
    m.add_class::<Flow>()?;
    m.add_class::<FlowState>()?;
    m.add_class::<FlowBranch>()?;
    Ok(())
}
