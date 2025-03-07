use crate::{encoders, Section, SectionId};

/// An encoder for the function section of WebAssembly modules.
///
/// # Example
///
/// ```
/// use wasm_encoder::{Module, FunctionSection, ValType};
///
/// let mut functions = FunctionSection::new();
/// let type_index = 0;
/// functions.function(type_index);
///
/// let mut module = Module::new();
/// module.section(&functions);
///
/// // Note: this will generate an invalid module because we didn't generate a
/// // code section containing the function body. See the documentation for
/// // `CodeSection` for details.
///
/// let bytes = module.finish();
/// ```
#[derive(Clone, Debug, Default)]
pub struct FunctionSection {
    bytes: Vec<u8>,
    num_added: u32,
}

impl FunctionSection {
    /// Construct a new module function section encoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of functions in the section.
    pub fn len(&self) -> u32 {
        self.num_added
    }

    /// Determines if the section is empty.
    pub fn is_empty(&self) -> bool {
        self.num_added == 0
    }

    /// Define a function in a module's function section.
    pub fn function(&mut self, type_index: u32) -> &mut Self {
        self.bytes.extend(encoders::u32(type_index));
        self.num_added += 1;
        self
    }
}

impl Section for FunctionSection {
    fn id(&self) -> u8 {
        SectionId::Function.into()
    }

    fn encode<S>(&self, sink: &mut S)
    where
        S: Extend<u8>,
    {
        let num_added = encoders::u32(self.num_added);
        let n = num_added.len();
        sink.extend(
            encoders::u32(u32::try_from(n + self.bytes.len()).unwrap())
                .chain(num_added)
                .chain(self.bytes.iter().copied()),
        );
    }
}
