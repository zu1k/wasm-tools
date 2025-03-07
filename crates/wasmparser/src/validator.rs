/* Copyright 2018 Mozilla Foundation
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::{
    limits::*, BinaryReaderError, Encoding, FunctionBody, Parser, Payload, Range, Result,
    SectionReader, SectionWithLimitedItems, Type, WASM_COMPONENT_VERSION, WASM_MODULE_VERSION,
};
use std::mem;
use std::sync::Arc;

/// Test whether the given buffer contains a valid WebAssembly module or component,
/// analogous to [`WebAssembly.validate`][js] in the JS API.
///
/// This functions requires the bytes to validate are entirely resident in memory.
/// Additionally this validates the given bytes with the default set of WebAssembly
/// features implemented by `wasmparser`.
///
/// For more fine-tuned control over validation it's recommended to review the
/// documentation of [`Validator`].
///
/// Upon success, the type information for the top-level module or component will
/// be returned.
///
/// [js]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/WebAssembly/validate
pub fn validate(bytes: &[u8]) -> Result<Types> {
    Validator::new().validate_all(bytes)
}

#[test]
fn test_validate() {
    assert!(validate(&[0x0, 0x61, 0x73, 0x6d, 0x1, 0x0, 0x0, 0x0]).is_ok());
    assert!(validate(&[0x0, 0x61, 0x73, 0x6d, 0x2, 0x0, 0x0, 0x0]).is_err());
}

mod component;
mod core;
mod func;
mod operators;
pub mod types;

use self::component::*;
pub use self::core::ValidatorResources;
use self::core::*;
use self::types::{TypeList, Types};
pub use func::FuncValidator;

fn check_max(cur_len: usize, amt_added: u32, max: usize, desc: &str, offset: usize) -> Result<()> {
    if max
        .checked_sub(cur_len)
        .and_then(|amt| amt.checked_sub(amt_added as usize))
        .is_none()
    {
        if max == 1 {
            return Err(BinaryReaderError::new(format!("multiple {}", desc), offset));
        }

        return Err(BinaryReaderError::new(
            format!("{} count exceeds limit of {}", desc, max),
            offset,
        ));
    }

    Ok(())
}

/// Validator for a WebAssembly binary module or component.
///
/// This structure encapsulates state necessary to validate a WebAssembly
/// binary. This implements validation as defined by the [core
/// specification][core]. A `Validator` is designed, like
/// [`Parser`], to accept incremental input over time.
/// Additionally a `Validator` is also designed for parallel validation of
/// functions as they are received.
///
/// It's expected that you'll be using a [`Parser`] in tandem with a
/// `Validator`. As each [`Payload`](crate::Payload) is received from a
/// [`Parser`] you'll pass it into a `Validator` to test the validity of the
/// payload. Note that all payloads received from a [`Parser`] are expected to
/// be passed to a [`Validator`]. For example if you receive
/// [`Payload::TypeSection`](crate::Payload) you'll call
/// [`Validator::type_section`] to validate this.
///
/// The design of [`Validator`] is intended that you'll interleave, in your own
/// application's processing, calls to validation. Each variant, after it's
/// received, will be validated and then your application would proceed as
/// usual. At all times, however, you'll have access to the [`Validator`] and
/// the validation context up to that point. This enables applications to check
/// the types of functions and learn how many globals there are, for example.
///
/// [core]: https://webassembly.github.io/spec/core/valid/index.html
#[derive(Default)]
pub struct Validator {
    /// The current state of the validator.
    state: State,

    /// The global type space used by the validator and any sub-validators.
    types: TypeList,

    /// The module state when parsing a WebAssembly module.
    module: Option<ModuleState>,

    /// With the component model enabled, this stores the pushed component states.
    /// The top of the stack is the current component state.
    components: Vec<ComponentState>,

    /// Enabled WebAssembly feature flags, dictating what's valid and what
    /// isn't.
    features: WasmFeatures,
}

enum State {
    /// A header has not yet been parsed.
    ///
    /// The value is the expected encoding for the header.
    Unparsed(Option<Encoding>),
    /// A module header has been parsed.
    ///
    /// The associated module state is available via [`Validator::module`].
    Module,
    /// A component header has been parsed.
    ///
    /// The associated component state exists at the top of the
    /// validator's [`Validator::components`] stack.
    Component,
    /// The parse has completed and no more data is expected.
    End,
}

impl State {
    fn ensure_module_state(&mut self, section: &str, offset: usize) -> Result<()> {
        match self {
            Self::Unparsed(_) => Err(BinaryReaderError::new(
                format!(
                    "unexpected module {} section before header was parsed",
                    section
                ),
                offset,
            )),
            Self::Module => Ok(()),
            Self::Component => Err(BinaryReaderError::new(
                format!(
                    "unexpected module {} section while parsing a component",
                    section
                ),
                offset,
            )),
            Self::End => Err(BinaryReaderError::new(
                format!(
                    "unexpected module {} section after parsing has completed",
                    section
                ),
                offset,
            )),
        }
    }

    fn ensure_component_state(&self, section: &str, offset: usize) -> Result<()> {
        match self {
            Self::Unparsed(_) => Err(BinaryReaderError::new(
                format!(
                    "unexpected component {} section before header was parsed",
                    section
                ),
                offset,
            )),
            Self::Module => Err(BinaryReaderError::new(
                format!(
                    "unexpected component {} section while parsing a module",
                    section
                ),
                offset,
            )),
            Self::Component => Ok(()),
            Self::End => Err(BinaryReaderError::new(
                format!(
                    "unexpected component {} section after parsing has completed",
                    section
                ),
                offset,
            )),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::Unparsed(None)
    }
}

/// Flags for features that are enabled for validation.
#[derive(Hash, Debug, Copy, Clone)]
pub struct WasmFeatures {
    /// The WebAssembly `mutable-global` proposal (enabled by default)
    pub mutable_global: bool,
    /// The WebAssembly `nontrapping-float-to-int-conversions` proposal (enabled by default)
    pub saturating_float_to_int: bool,
    /// The WebAssembly `sign-extension-ops` proposal (enabled by default)
    pub sign_extension: bool,
    /// The WebAssembly reference types proposal (enabled by default)
    pub reference_types: bool,
    /// The WebAssembly multi-value proposal (enabled by default)
    pub multi_value: bool,
    /// The WebAssembly bulk memory operations proposal (enabled by default)
    pub bulk_memory: bool,
    /// The WebAssembly SIMD proposal
    pub simd: bool,
    /// The WebAssembly Relaxed SIMD proposal
    pub relaxed_simd: bool,
    /// The WebAssembly threads proposal
    pub threads: bool,
    /// The WebAssembly tail-call proposal
    pub tail_call: bool,
    /// Whether or not only deterministic instructions are allowed
    pub deterministic_only: bool,
    /// The WebAssembly multi memory proposal
    pub multi_memory: bool,
    /// The WebAssembly exception handling proposal
    pub exceptions: bool,
    /// The WebAssembly memory64 proposal
    pub memory64: bool,
    /// The WebAssembly extended_const proposal
    pub extended_const: bool,
    /// The WebAssembly component model proposal.
    pub component_model: bool,
}

impl WasmFeatures {
    pub(crate) fn check_value_type(&self, ty: Type) -> Result<(), &'static str> {
        match ty {
            Type::I32 | Type::I64 | Type::F32 | Type::F64 => Ok(()),
            Type::FuncRef | Type::ExternRef => {
                if self.reference_types {
                    Ok(())
                } else {
                    Err("reference types support is not enabled")
                }
            }
            Type::V128 => {
                if self.simd {
                    Ok(())
                } else {
                    Err("SIMD support is not enabled")
                }
            }
        }
    }
}

impl Default for WasmFeatures {
    fn default() -> WasmFeatures {
        WasmFeatures {
            // off-by-default features
            relaxed_simd: false,
            threads: false,
            tail_call: false,
            multi_memory: false,
            exceptions: false,
            memory64: false,
            extended_const: false,
            component_model: false,
            deterministic_only: cfg!(feature = "deterministic"),

            // on-by-default features
            mutable_global: true,
            saturating_float_to_int: true,
            sign_extension: true,
            bulk_memory: true,
            multi_value: true,
            reference_types: true,
            simd: true,
        }
    }
}

/// Possible return values from [`Validator::payload`].
#[allow(clippy::large_enum_variant)]
pub enum ValidPayload<'a> {
    /// The payload validated, no further action need be taken.
    Ok,
    /// The payload validated, but it started a nested module or component.
    ///
    /// This result indicates that the specified parser should be used instead
    /// of the currently-used parser until this returned one ends.
    Parser(Parser),
    /// A function was found to be validate.
    Func(FuncValidator<ValidatorResources>, FunctionBody<'a>),
    /// The end payload was validated and the types known to the validator
    /// are provided.
    End(Types),
}

impl Validator {
    /// Creates a new [`Validator`] ready to validate a WebAssembly module
    /// or component.
    ///
    /// The new validator will receive payloads parsed from
    /// [`Parser`], and expects the first payload received to be
    /// the version header from the parser.
    pub fn new() -> Validator {
        Validator::default()
    }

    /// Creates a new [`Validator`] which has the specified set of wasm
    /// features activated for validation.
    ///
    /// This function is the same as [`Validator::new`] except it also allows
    /// you to customize the active wasm features in use for validation. This
    /// can allow enabling experimental proposals or also turning off
    /// on-by-default wasm proposals.
    pub fn new_with_features(features: WasmFeatures) -> Validator {
        let mut ret = Validator::new();
        ret.features = features;
        ret
    }

    /// Returns the wasm features used for this validator.
    pub fn features(&self) -> &WasmFeatures {
        &self.features
    }

    /// Validates an entire in-memory module or component with this validator.
    ///
    /// This function will internally create a [`Parser`] to parse the `bytes`
    /// provided. The entire module or component specified by `bytes` will be
    /// parsed and validated.
    ///
    /// Upon success, the type information for the top-level module or component
    /// will be returned.
    pub fn validate_all(&mut self, bytes: &[u8]) -> Result<Types> {
        let mut functions_to_validate = Vec::new();
        let mut last_types = None;
        for payload in Parser::new(0).parse_all(bytes) {
            match self.payload(&payload?)? {
                ValidPayload::Func(a, b) => {
                    functions_to_validate.push((a, b));
                }
                ValidPayload::End(types) => {
                    // Only the last (top-level) type information will be returned
                    last_types = Some(types);
                }
                _ => {}
            }
        }

        for (mut validator, body) in functions_to_validate {
            validator.validate(&body)?;
        }

        Ok(last_types.unwrap())
    }

    /// Convenience function to validate a single [`Payload`].
    ///
    /// This function is intended to be used as a convenience. It will
    /// internally perform any validation necessary to validate the [`Payload`]
    /// provided. The convenience part is that you're likely already going to
    /// be matching on [`Payload`] in your application, at which point it's more
    /// appropriate to call the individual methods on [`Validator`] per-variant
    /// in [`Payload`], such as [`Validator::type_section`].
    ///
    /// This function returns a [`ValidPayload`] variant on success, indicating
    /// one of a few possible actions that need to be taken after a payload is
    /// validated. For example function contents are not validated here, they're
    /// returned through [`ValidPayload`] for validation by the caller.
    pub fn payload<'a>(&mut self, payload: &Payload<'a>) -> Result<ValidPayload<'a>> {
        use crate::Payload::*;
        match payload {
            Version {
                num,
                encoding,
                range,
            } => self.version(*num, *encoding, range)?,

            // Module sections
            TypeSection(s) => self.type_section(s)?,
            ImportSection(s) => self.import_section(s)?,
            FunctionSection(s) => self.function_section(s)?,
            TableSection(s) => self.table_section(s)?,
            MemorySection(s) => self.memory_section(s)?,
            TagSection(s) => self.tag_section(s)?,
            GlobalSection(s) => self.global_section(s)?,
            ExportSection(s) => self.export_section(s)?,
            StartSection { func, range } => self.start_section(*func, range)?,
            ElementSection(s) => self.element_section(s)?,
            DataCountSection { count, range } => self.data_count_section(*count, range)?,
            CodeSectionStart {
                count,
                range,
                size: _,
            } => self.code_section_start(*count, range)?,
            CodeSectionEntry(body) => {
                let func_validator = self.code_section_entry(body)?;
                return Ok(ValidPayload::Func(func_validator, *body));
            }
            DataSection(s) => self.data_section(s)?,

            // Component sections
            ComponentTypeSection(s) => self.component_type_section(s)?,
            ComponentImportSection(s) => self.component_import_section(s)?,
            ComponentFunctionSection(s) => self.component_function_section(s)?,
            ModuleSection { parser, range, .. } => {
                self.module_section(range)?;
                return Ok(ValidPayload::Parser(parser.clone()));
            }
            ComponentSection { parser, range, .. } => {
                self.component_section(range)?;
                return Ok(ValidPayload::Parser(parser.clone()));
            }
            InstanceSection(s) => self.instance_section(s)?,
            ComponentExportSection(s) => self.component_export_section(s)?,
            ComponentStartSection(s) => self.component_start_section(s)?,
            AliasSection(s) => self.alias_section(s)?,

            End(offset) => return Ok(ValidPayload::End(self.end(*offset)?)),

            CustomSection { .. } => {} // no validation for custom sections
            UnknownSection { id, range, .. } => self.unknown_section(*id, range)?,
        }
        Ok(ValidPayload::Ok)
    }

    /// Validates [`Payload::Version`](crate::Payload).
    pub fn version(&mut self, num: u32, encoding: Encoding, range: &Range) -> Result<()> {
        match &self.state {
            State::Unparsed(expected) => {
                if let Some(expected) = expected {
                    if *expected != encoding {
                        return Err(BinaryReaderError::new(
                            format!(
                                "expected a version header for a {}",
                                match expected {
                                    Encoding::Module => "module",
                                    Encoding::Component => "component",
                                }
                            ),
                            range.start,
                        ));
                    }
                }
            }
            _ => {
                return Err(BinaryReaderError::new(
                    "wasm version header out of order",
                    range.start,
                ))
            }
        }

        self.state = match (encoding, num) {
            (Encoding::Module, WASM_MODULE_VERSION) => {
                assert!(self.module.is_none());
                self.module = Some(ModuleState::default());
                State::Module
            }
            (Encoding::Component, WASM_COMPONENT_VERSION) => {
                if !self.features.component_model {
                    return Err(BinaryReaderError::new(
                        "WebAssembly component model feature not enabled",
                        range.start,
                    ));
                }

                self.components.push(ComponentState::default());
                State::Component
            }
            _ => {
                return Err(BinaryReaderError::new(
                    "unknown binary version",
                    range.start,
                ));
            }
        };

        Ok(())
    }

    /// Validates [`Payload::TypeSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn type_section(&mut self, section: &crate::TypeSectionReader<'_>) -> Result<()> {
        self.ensure_module_section(
            Order::Type,
            section,
            "type",
            |state, _, types, count, offset| {
                check_max(
                    state.module.types.len(),
                    count,
                    MAX_WASM_TYPES,
                    "types",
                    offset,
                )?;
                types.reserve(count as usize);
                state.module.assert_mut().types.reserve(count as usize);
                Ok(())
            },
            |state, features, types, def, offset| {
                state
                    .module
                    .assert_mut()
                    .add_type(def, features, types, offset)
            },
        )
    }

    /// Validates [`Payload::ImportSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn import_section(&mut self, section: &crate::ImportSectionReader<'_>) -> Result<()> {
        self.ensure_module_section(
            Order::Import,
            section,
            "import",
            |_, _, _, _, _| Ok(()), // add_import will check limits
            |state, features, types, import, offset| {
                state
                    .module
                    .assert_mut()
                    .add_import(import, features, types, offset)
            },
        )
    }

    /// Validates [`Payload::FunctionSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn function_section(&mut self, section: &crate::FunctionSectionReader<'_>) -> Result<()> {
        self.ensure_module_section(
            Order::Function,
            section,
            "function",
            |state, _, _, count, offset| {
                check_max(
                    state.module.functions.len(),
                    count,
                    MAX_WASM_FUNCTIONS,
                    "functions",
                    offset,
                )?;
                state.module.assert_mut().functions.reserve(count as usize);
                debug_assert!(state.expected_code_bodies.is_none());
                state.expected_code_bodies = Some(count);
                Ok(())
            },
            |state, _, types, ty, offset| state.module.assert_mut().add_function(ty, types, offset),
        )
    }

    /// Validates [`Payload::TableSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn table_section(&mut self, section: &crate::TableSectionReader<'_>) -> Result<()> {
        let features = self.features;
        self.ensure_module_section(
            Order::Table,
            section,
            "table",
            |state, _, _, count, offset| {
                check_max(
                    state.module.tables.len(),
                    count,
                    state.module.max_tables(&features),
                    "tables",
                    offset,
                )?;
                state.module.assert_mut().tables.reserve(count as usize);
                Ok(())
            },
            |state, features, _, ty, offset| {
                state.module.assert_mut().add_table(ty, features, offset)
            },
        )
    }

    /// Validates [`Payload::MemorySection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn memory_section(&mut self, section: &crate::MemorySectionReader<'_>) -> Result<()> {
        self.ensure_module_section(
            Order::Memory,
            section,
            "memory",
            |state, features, _, count, offset| {
                check_max(
                    state.module.memories.len(),
                    count,
                    state.module.max_memories(features),
                    "memories",
                    offset,
                )?;
                state.module.assert_mut().memories.reserve(count as usize);
                Ok(())
            },
            |state, features, _, ty, offset| {
                state.module.assert_mut().add_memory(ty, features, offset)
            },
        )
    }

    /// Validates [`Payload::TagSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn tag_section(&mut self, section: &crate::TagSectionReader<'_>) -> Result<()> {
        if !self.features.exceptions {
            return Err(BinaryReaderError::new(
                "exceptions proposal not enabled",
                section.range().start,
            ));
        }

        self.ensure_module_section(
            Order::Tag,
            section,
            "tag",
            |state, _, _, count, offset| {
                check_max(
                    state.module.tags.len(),
                    count,
                    MAX_WASM_TAGS,
                    "tags",
                    offset,
                )?;
                state.module.assert_mut().tags.reserve(count as usize);
                Ok(())
            },
            |state, features, types, ty, offset| {
                state
                    .module
                    .assert_mut()
                    .add_tag(ty, features, types, offset)
            },
        )
    }

    /// Validates [`Payload::GlobalSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn global_section(&mut self, section: &crate::GlobalSectionReader<'_>) -> Result<()> {
        self.ensure_module_section(
            Order::Global,
            section,
            "global",
            |state, _, _, count, offset| {
                check_max(
                    state.module.globals.len(),
                    count,
                    MAX_WASM_GLOBALS,
                    "globals",
                    offset,
                )?;
                state.module.assert_mut().globals.reserve(count as usize);
                Ok(())
            },
            |state, features, types, global, offset| {
                state.add_global(global, features, types, offset)
            },
        )
    }

    /// Validates [`Payload::ExportSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn export_section(&mut self, section: &crate::ExportSectionReader<'_>) -> Result<()> {
        self.ensure_module_section(
            Order::Export,
            section,
            "export",
            |state, _, _, count, offset| {
                check_max(
                    state.module.exports.len(),
                    count,
                    MAX_WASM_EXPORTS,
                    "exports",
                    offset,
                )?;
                state.module.assert_mut().exports.reserve(count as usize);
                Ok(())
            },
            |state, features, _, e, offset| {
                let module = state.module.assert_mut();
                let ty = module.export_to_entity_type(&e, offset)?;
                module.add_export(e.name, ty, features, offset)
            },
        )
    }

    /// Validates [`Payload::StartSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn start_section(&mut self, func: u32, range: &Range) -> Result<()> {
        let offset = range.start;
        self.state.ensure_module_state("start", offset)?;
        let state = self.module.as_mut().unwrap();
        state.update_order(Order::Start, offset)?;

        let ty = state.module.get_func_type(func, &self.types, offset)?;
        if !ty.params.is_empty() || !ty.returns.is_empty() {
            return Err(BinaryReaderError::new(
                "invalid start function type",
                offset,
            ));
        }

        Ok(())
    }

    /// Validates [`Payload::ElementSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn element_section(&mut self, section: &crate::ElementSectionReader<'_>) -> Result<()> {
        self.ensure_module_section(
            Order::Element,
            section,
            "element",
            |state, _, _, count, offset| {
                check_max(
                    state.module.element_types.len(),
                    count,
                    MAX_WASM_ELEMENT_SEGMENTS,
                    "element segments",
                    offset,
                )?;
                state
                    .module
                    .assert_mut()
                    .element_types
                    .reserve(count as usize);
                Ok(())
            },
            |state, features, types, e, offset| {
                state.add_element_segment(e, features, types, offset)
            },
        )
    }

    /// Validates [`Payload::DataCountSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn data_count_section(&mut self, count: u32, range: &Range) -> Result<()> {
        let offset = range.start;
        self.state.ensure_module_state("data count", offset)?;
        let state = self.module.as_mut().unwrap();
        state.update_order(Order::DataCount, offset)?;

        if count > MAX_WASM_DATA_SEGMENTS as u32 {
            return Err(BinaryReaderError::new(
                "data count section specifies too many data segments",
                offset,
            ));
        }

        state.module.assert_mut().data_count = Some(count);
        Ok(())
    }

    /// Validates [`Payload::CodeSectionStart`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn code_section_start(&mut self, count: u32, range: &Range) -> Result<()> {
        let offset = range.start;
        self.state.ensure_module_state("code", offset)?;
        let state = self.module.as_mut().unwrap();
        state.update_order(Order::Code, offset)?;

        match state.expected_code_bodies.take() {
            Some(n) if n == count => {}
            Some(_) => {
                return Err(BinaryReaderError::new(
                    "function and code section have inconsistent lengths",
                    offset,
                ));
            }
            // empty code sections are allowed even if the function section is
            // missing
            None if count == 0 => {}
            None => {
                return Err(BinaryReaderError::new(
                    "code section without function section",
                    offset,
                ))
            }
        }

        // Take a snapshot of the types when we start the code section.
        state.module.assert_mut().snapshot = Some(Arc::new(self.types.commit()));

        Ok(())
    }

    /// Validates [`Payload::CodeSectionEntry`](crate::Payload).
    ///
    /// This function will prepare a [`FuncValidator`] which can be used to
    /// validate the function. The function body provided will be parsed only
    /// enough to create the function validation context. After this the
    /// [`OperatorsReader`](crate::readers::OperatorsReader) returned can be used to read the
    /// opcodes of the function as well as feed information into the validator.
    ///
    /// Note that the returned [`FuncValidator`] is "connected" to this
    /// [`Validator`] in that it uses the internal context of this validator for
    /// validating the function. The [`FuncValidator`] can be sent to
    /// another thread, for example, to offload actual processing of functions
    /// elsewhere.
    ///
    /// This method should only be called when parsing a module.
    pub fn code_section_entry(
        &mut self,
        body: &crate::FunctionBody,
    ) -> Result<FuncValidator<ValidatorResources>> {
        let offset = body.range().start;
        self.state.ensure_module_state("code", offset)?;
        let state = self.module.as_mut().unwrap();

        Ok(FuncValidator::new(
            state.next_code_entry_type(offset)?,
            0,
            ValidatorResources(state.module.arc().clone()),
            &self.features,
        )
        .unwrap())
    }

    /// Validates [`Payload::DataSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a module.
    pub fn data_section(&mut self, section: &crate::DataSectionReader<'_>) -> Result<()> {
        let mut section = section.clone();
        section.forbid_bulk_memory(!self.features.bulk_memory);

        self.ensure_module_section(
            Order::Data,
            &section,
            "data",
            |state, _, _, count, offset| {
                state.data_segment_count = count;
                check_max(
                    state.module.data_count.unwrap_or(0) as usize,
                    count,
                    MAX_WASM_DATA_SEGMENTS,
                    "data segments",
                    offset,
                )
            },
            |state, features, types, d, offset| state.add_data_segment(d, features, types, offset),
        )
    }

    /// Validates [`Payload::ComponentTypeSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a component.
    pub fn component_type_section(
        &mut self,
        section: &crate::ComponentTypeSectionReader,
    ) -> Result<()> {
        self.ensure_component_section(
            section,
            "type",
            |components, types, count, offset| {
                let current = components.last_mut().unwrap();
                check_max(current.types.len(), count, MAX_WASM_TYPES, "types", offset)?;
                types.reserve(count as usize);
                current.types.reserve(count as usize);
                Ok(())
            },
            |components, types, features, ty, offset| {
                ComponentState::add_type(components, ty, features, types, offset)
            },
        )
    }

    /// Validates [`Payload::ComponentImportSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a component.
    pub fn component_import_section(
        &mut self,
        section: &crate::ComponentImportSectionReader,
    ) -> Result<()> {
        self.ensure_component_section(
            section,
            "import",
            |_, _, _, _| Ok(()), // add_import will check limits
            |components, types, _, import, offset| {
                components
                    .last_mut()
                    .unwrap()
                    .add_import(import, types, offset)
            },
        )
    }

    /// Validates [`Payload::ComponentFunctionSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a component.
    pub fn component_function_section(
        &mut self,
        section: &crate::ComponentFunctionSectionReader,
    ) -> Result<()> {
        self.ensure_component_section(
            section,
            "function",
            |components, _, count, offset| {
                let current = components.last_mut().unwrap();
                check_max(
                    current.functions.len(),
                    count,
                    MAX_WASM_FUNCTIONS,
                    "functions",
                    offset,
                )?;
                current.functions.reserve(count as usize);
                Ok(())
            },
            |components, types, _, func, offset| {
                let current = components.last_mut().unwrap();
                match func {
                    crate::ComponentFunction::Lift {
                        type_index,
                        func_index,
                        options,
                    } => current.lift_function(
                        type_index,
                        func_index,
                        options.into_vec(),
                        types,
                        offset,
                    ),
                    crate::ComponentFunction::Lower {
                        func_index,
                        options,
                    } => current.lower_function(func_index, options.into_vec(), types, offset),
                }
            },
        )
    }

    /// Validates [`Payload::ModuleSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a component.
    pub fn module_section(&mut self, range: &Range) -> Result<()> {
        self.state.ensure_component_state("module", range.start)?;
        let current = self.components.last_mut().unwrap();
        check_max(
            current.modules.len(),
            1,
            MAX_WASM_MODULES,
            "modules",
            range.start,
        )?;

        match mem::replace(&mut self.state, State::Unparsed(Some(Encoding::Module))) {
            State::Component => {}
            _ => unreachable!(),
        }

        Ok(())
    }

    /// Validates [`Payload::ComponentSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a component.
    pub fn component_section(&mut self, range: &Range) -> Result<()> {
        self.state
            .ensure_component_state("component", range.start)?;
        let current = self.components.last_mut().unwrap();
        check_max(
            current.components.len(),
            1,
            MAX_WASM_COMPONENTS,
            "components",
            range.start,
        )?;

        match mem::replace(&mut self.state, State::Unparsed(Some(Encoding::Component))) {
            State::Component => {}
            _ => unreachable!(),
        }

        Ok(())
    }

    /// Validates [`Payload::InstanceSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a component.
    pub fn instance_section(&mut self, section: &crate::InstanceSectionReader) -> Result<()> {
        self.ensure_component_section(
            section,
            "instance",
            |components, _, count, offset| {
                let current = components.last_mut().unwrap();
                check_max(
                    current.instances.len(),
                    count,
                    MAX_WASM_INSTANCES,
                    "instances",
                    offset,
                )?;
                current.instances.reserve(count as usize);
                Ok(())
            },
            |components, types, _, instance, offset| {
                components
                    .last_mut()
                    .unwrap()
                    .add_instance(instance, types, offset)
            },
        )
    }

    /// Validates [`Payload::ComponentExportSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a component.
    pub fn component_export_section(
        &mut self,
        section: &crate::ComponentExportSectionReader,
    ) -> Result<()> {
        self.ensure_component_section(
            section,
            "export",
            |components, _, count, offset| {
                let current = components.last_mut().unwrap();
                check_max(
                    current.exports.len(),
                    count,
                    MAX_WASM_EXPORTS,
                    "exports",
                    offset,
                )?;
                current.exports.reserve(count as usize);
                Ok(())
            },
            |components, types, _, export, offset| {
                let current = components.last_mut().unwrap();
                let ty = current.export_to_entity_type(&export, types, offset)?;
                current.add_export(export.name, ty, offset)
            },
        )
    }

    /// Validates [`Payload::ComponentStartSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a component.
    pub fn component_start_section(
        &mut self,
        section: &crate::ComponentStartSectionReader,
    ) -> Result<()> {
        let range = section.range();
        self.state.ensure_component_state("start", range.start)?;
        let f = section.clone().read()?;

        self.components.last_mut().unwrap().add_start(
            f.func_index,
            &f.arguments,
            &self.types,
            range.start,
        )
    }

    /// Validates [`Payload::AliasSection`](crate::Payload).
    ///
    /// This method should only be called when parsing a component.
    pub fn alias_section(&mut self, section: &crate::AliasSectionReader) -> Result<()> {
        self.ensure_component_section(
            section,
            "alias",
            |_, _, _, _| Ok(()), // maximums checked via `add_alias`
            |components, types, _, alias, offset| -> Result<(), BinaryReaderError> {
                ComponentState::add_alias(components, alias, types, offset)
            },
        )
    }

    /// Validates [`Payload::UnknownSection`](crate::Payload).
    ///
    /// Currently always returns an error.
    pub fn unknown_section(&mut self, id: u8, range: &Range) -> Result<()> {
        Err(BinaryReaderError::new(
            format!("malformed section id: {}", id),
            range.start,
        ))
    }

    /// Validates [`Payload::End`](crate::Payload).
    ///
    /// Returns the types known to the validator for the module or component.
    pub fn end(&mut self, offset: usize) -> Result<Types> {
        match std::mem::replace(&mut self.state, State::End) {
            State::Unparsed(_) => Err(BinaryReaderError::new(
                "cannot call `end` before a header has been parsed",
                offset,
            )),
            State::End => Err(BinaryReaderError::new(
                "cannot call `end` after parsing has completed",
                offset,
            )),
            State::Module => {
                let mut state = self.module.take().unwrap();
                state.validate_end(offset)?;

                // If there's a parent component, we'll add a module to the parent state
                // and continue to validate the component
                if let Some(parent) = self.components.last_mut() {
                    parent.add_module(&state.module, &mut self.types, offset)?;
                    self.state = State::Component;
                }

                Ok(Types::from_module(
                    self.types.commit(),
                    state.module.arc().clone(),
                ))
            }
            State::Component => {
                let mut component = self.components.pop().unwrap();

                // If there's a parent component, pop the stack, add it to the parent,
                // and continue to validate the component
                if self.components.len() > 1 {
                    let current = self.components.last_mut().unwrap();
                    current.add_component(&mut component, &mut self.types);
                    self.state = State::Component;
                }

                Ok(Types::from_component(self.types.commit(), component))
            }
        }
    }

    fn ensure_module_section<T>(
        &mut self,
        order: Order,
        section: &T,
        name: &str,
        validate_section: impl FnOnce(
            &mut ModuleState,
            &WasmFeatures,
            &mut TypeList,
            u32,
            usize,
        ) -> Result<()>,
        mut validate_item: impl FnMut(
            &mut ModuleState,
            &WasmFeatures,
            &mut TypeList,
            T::Item,
            usize,
        ) -> Result<()>,
    ) -> Result<()>
    where
        T: SectionReader + Clone + SectionWithLimitedItems,
    {
        let offset = section.range().start;
        self.state.ensure_module_state(name, offset)?;

        let state = self.module.as_mut().unwrap();
        state.update_order(order, offset)?;

        validate_section(
            state,
            &self.features,
            &mut self.types,
            section.get_count(),
            offset,
        )?;

        let mut section = section.clone();
        for _ in 0..section.get_count() {
            let offset = section.original_position();
            let item = section.read()?;
            validate_item(state, &self.features, &mut self.types, item, offset)?;
        }

        section.ensure_end()?;

        Ok(())
    }

    fn ensure_component_section<T>(
        &mut self,
        section: &T,
        name: &str,
        validate_section: impl FnOnce(&mut Vec<ComponentState>, &mut TypeList, u32, usize) -> Result<()>,
        mut validate_item: impl FnMut(
            &mut Vec<ComponentState>,
            &mut TypeList,
            &WasmFeatures,
            T::Item,
            usize,
        ) -> Result<()>,
    ) -> Result<()>
    where
        T: SectionReader + Clone + SectionWithLimitedItems,
    {
        let offset = section.range().start;

        if !self.features.component_model {
            return Err(BinaryReaderError::new(
                "component model feature is not enabled",
                offset,
            ));
        }

        self.state.ensure_component_state(name, offset)?;
        validate_section(
            &mut self.components,
            &mut self.types,
            section.get_count(),
            offset,
        )?;

        let mut section = section.clone();
        for _ in 0..section.get_count() {
            let offset = section.original_position();
            let item = section.read()?;
            validate_item(
                &mut self.components,
                &mut self.types,
                &self.features,
                item,
                offset,
            )?;
        }

        section.ensure_end()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{GlobalType, MemoryType, TableType, Type, Validator, WasmFeatures};
    use anyhow::Result;

    #[test]
    fn test_module_type_information() -> Result<()> {
        let bytes = wat::parse_str(
            r#"
            (module
                (type (func (param i32 i64) (result i32)))
                (memory 1 5)
                (table 10 funcref)
                (global (mut i32) (i32.const 0))
                (func (type 0) (i32.const 0))
                (tag (param i64 i32))
                (elem funcref (ref.func 0))
            )
        "#,
        )?;

        let mut validator = Validator::new_with_features(WasmFeatures {
            exceptions: true,
            ..Default::default()
        });

        let types = validator.validate_all(&bytes)?;

        assert_eq!(types.type_count(), 2);
        assert_eq!(types.memory_count(), 1);
        assert_eq!(types.table_count(), 1);
        assert_eq!(types.global_count(), 1);
        assert_eq!(types.function_count(), 1);
        assert_eq!(types.tag_count(), 1);
        assert_eq!(types.element_count(), 1);
        assert_eq!(types.module_count(), 0);
        assert_eq!(types.component_count(), 0);
        assert_eq!(types.instance_count(), 0);
        assert_eq!(types.value_count(), 0);

        match types.func_type_at(0) {
            Some(ty) => {
                assert_eq!(ty.params.as_ref(), [Type::I32, Type::I64]);
                assert_eq!(ty.returns.as_ref(), [Type::I32]);
            }
            _ => unreachable!(),
        }

        match types.func_type_at(1) {
            Some(ty) => {
                assert_eq!(ty.params.as_ref(), [Type::I64, Type::I32]);
                assert_eq!(ty.returns.as_ref(), []);
            }
            _ => unreachable!(),
        }

        assert_eq!(
            types.memory_at(0),
            Some(MemoryType {
                memory64: false,
                shared: false,
                initial: 1,
                maximum: Some(5)
            })
        );

        assert_eq!(
            types.table_at(0),
            Some(TableType {
                initial: 10,
                maximum: None,
                element_type: Type::FuncRef,
            })
        );

        assert_eq!(
            types.global_at(0),
            Some(GlobalType {
                content_type: Type::I32,
                mutable: true
            })
        );

        match types.function_at(0) {
            Some(ty) => {
                assert_eq!(ty.params.as_ref(), [Type::I32, Type::I64]);
                assert_eq!(ty.returns.as_ref(), [Type::I32]);
            }
            _ => unreachable!(),
        }

        match types.tag_at(0) {
            Some(ty) => {
                assert_eq!(ty.params.as_ref(), [Type::I64, Type::I32]);
                assert_eq!(ty.returns.as_ref(), []);
            }
            _ => unreachable!(),
        }

        assert_eq!(types.element_at(0), Some(Type::FuncRef));

        Ok(())
    }
}
