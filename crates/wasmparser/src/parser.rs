use crate::limits::MAX_WASM_MODULE_SIZE;
use crate::{
    AliasSectionReader, ComponentExportSectionReader, ComponentFunctionSectionReader,
    ComponentImportSectionReader, ComponentStartSectionReader, ComponentTypeSectionReader,
    InstanceSectionReader,
};
use crate::{
    BinaryReader, BinaryReaderError, DataSectionReader, ElementSectionReader, ExportSectionReader,
    FunctionBody, FunctionSectionReader, GlobalSectionReader, ImportSectionReader,
    MemorySectionReader, Range, Result, TableSectionReader, TagSectionReader, TypeSectionReader,
};
use std::convert::TryInto;
use std::fmt;
use std::iter;

pub(crate) const WASM_EXPERIMENTAL_VERSION: u32 = 0xd;
pub(crate) const WASM_MODULE_VERSION: u32 = 0x1;
pub(crate) const WASM_COMPONENT_VERSION: u32 = 0x0001000a;

/// The supported encoding formats for the parser.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Encoding {
    /// The encoding format is a WebAssembly module.
    Module,
    /// The encoding format is a WebAssembly component.
    Component,
}

/// An incremental parser of a binary WebAssembly module or component.
///
/// This type is intended to be used to incrementally parse a WebAssembly module
/// or component as bytes become available for the module. This can also be used
/// to parse modules or components that are already entirely resident within memory.
///
/// This primary function for a parser is the [`Parser::parse`] function which
/// will incrementally consume input. You can also use the [`Parser::parse_all`]
/// function to parse a module or component that is entirely resident in memory.
#[derive(Debug, Clone)]
pub struct Parser {
    state: State,
    offset: u64,
    max_size: u64,
    encoding: Encoding,
}

#[derive(Debug, Clone)]
enum State {
    Header,
    SectionStart,
    FunctionBody { remaining: u32, len: u32 },
}

/// A successful return payload from [`Parser::parse`].
///
/// On success one of two possible values can be returned, either that more data
/// is needed to continue parsing or a chunk of the input was parsed, indicating
/// how much of it was parsed.
#[derive(Debug)]
pub enum Chunk<'a> {
    /// This can be returned at any time and indicates that more data is needed
    /// to proceed with parsing. Zero bytes were consumed from the input to
    /// [`Parser::parse`]. The `usize` value here is a hint as to how many more
    /// bytes are needed to continue parsing.
    NeedMoreData(u64),

    /// A chunk was successfully parsed.
    Parsed {
        /// This many bytes of the `data` input to [`Parser::parse`] were
        /// consumed to produce `payload`.
        consumed: usize,
        /// The value that we actually parsed.
        payload: Payload<'a>,
    },
}

/// Values that can be parsed from a WebAssembly module or component.
///
/// This enumeration is all possible chunks of pieces that can be parsed by a
/// [`Parser`] from a binary WebAssembly module or component. Note that for many
/// sections the entire section is parsed all at once, whereas other functions,
/// like the code section, are parsed incrementally. This is a distinction where some
/// sections, like the type section, are required to be fully resident in memory
/// (fully downloaded) before proceeding. Other sections, like the code section,
/// can be processed in a streaming fashion where each function is extracted
/// individually so it can possibly be shipped to another thread while you wait
/// for more functions to get downloaded.
///
/// Note that payloads, when returned, do not indicate that the module or component
/// is valid. For example when you receive a `Payload::TypeSection` the type
/// section itself has not yet actually been parsed. The reader returned will be
/// able to parse it, but you'll have to actually iterate the reader to do the
/// full parse. Each payload returned is intended to be a *window* into the
/// original `data` passed to [`Parser::parse`] which can be further processed
/// if necessary.
pub enum Payload<'a> {
    /// Indicates the header of a WebAssembly module or component.
    Version {
        /// The version number found in the header.
        num: u32,
        /// The encoding format being parsed.
        encoding: Encoding,
        /// The range of bytes that were parsed to consume the header of the
        /// module or component. Note that this range is relative to the start
        /// of the byte stream.
        range: Range,
    },

    /// A module type section was received and the provided reader can be
    /// used to parse the contents of the type section.
    TypeSection(crate::TypeSectionReader<'a>),
    /// A module import section was received and the provided reader can be
    /// used to parse the contents of the import section.
    ImportSection(crate::ImportSectionReader<'a>),
    /// A module function section was received and the provided reader can be
    /// used to parse the contents of the function section.
    FunctionSection(crate::FunctionSectionReader<'a>),
    /// A module table section was received and the provided reader can be
    /// used to parse the contents of the table section.
    TableSection(crate::TableSectionReader<'a>),
    /// A module memory section was received and the provided reader can be
    /// used to parse the contents of the memory section.
    MemorySection(crate::MemorySectionReader<'a>),
    /// A module tag section was received, and the provided reader can be
    /// used to parse the contents of the tag section.
    TagSection(crate::TagSectionReader<'a>),
    /// A module global section was received and the provided reader can be
    /// used to parse the contents of the global section.
    GlobalSection(crate::GlobalSectionReader<'a>),
    /// A module export section was received, and the provided reader can be
    /// used to parse the contents of the export section.
    ExportSection(crate::ExportSectionReader<'a>),
    /// A module start section was received.
    StartSection {
        /// The start function index
        func: u32,
        /// The range of bytes that specify the `func` field, specified in
        /// offsets relative to the start of the byte stream.
        range: Range,
    },
    /// A module element section was received and the provided reader can be
    /// used to parse the contents of the element section.
    ElementSection(crate::ElementSectionReader<'a>),
    /// A module data count section was received.
    DataCountSection {
        /// The number of data segments.
        count: u32,
        /// The range of bytes that specify the `count` field, specified in
        /// offsets relative to the start of the byte stream.
        range: Range,
    },
    /// A module data section was received and the provided reader can be
    /// used to parse the contents of the data section.
    DataSection(crate::DataSectionReader<'a>),

    /// A component type section was received and the provided reader can be
    /// used to parse the contents of the type section.
    ComponentTypeSection(crate::ComponentTypeSectionReader<'a>),
    /// A component import section was received and the provided reader can be
    /// used to parse the contents of the import section.
    ComponentImportSection(crate::ComponentImportSectionReader<'a>),
    /// A component function section was received and the provided reader can be
    /// used to parse the contents of the function section.
    ComponentFunctionSection(crate::ComponentFunctionSectionReader<'a>),
    /// A component module section was received and the provided parser can be
    /// used to parse the nested module.
    ///
    /// This variant is special in that it returns a sub-`Parser`. Upon
    /// receiving a `ModuleSection` it is expected that the returned
    /// `Parser` will be used instead of the parent `Parser` until the parse has
    /// finished. You'll need to feed data into the `Parser` returned until it
    /// returns `Payload::End`. After that you'll switch back to the parent
    /// parser to resume parsing the rest of the current component.
    ///
    /// Note that binaries will not be parsed correctly if you feed the data for
    /// a nested module into the parent [`Parser`].
    ModuleSection {
        /// The parser for the nested module.
        parser: Parser,
        /// The range of bytes that represent the nested module in the
        /// original byte stream.
        range: Range,
    },
    /// A component section from a WebAssembly component was received and the
    /// provided parser can be used to parse the nested component.
    ///
    /// This variant is special in that it returns a sub-`Parser`. Upon
    /// receiving a `ComponentSection` it is expected that the returned
    /// `Parser` will be used instead of the parent `Parser` until the parse has
    /// finished. You'll need to feed data into the `Parser` returned until it
    /// returns `Payload::End`. After that you'll switch back to the parent
    /// parser to resume parsing the rest of the current component.
    ///
    /// Note that binaries will not be parsed correctly if you feed the data for
    /// a nested component into the parent [`Parser`].
    ComponentSection {
        /// The parser for the nested component.
        parser: Parser,
        /// The range of bytes that represent the nested component in the
        /// original byte stream.
        range: Range,
    },
    /// A component instance section was received and the provided reader can be
    /// used to parse the contents of the instance section.
    InstanceSection(crate::InstanceSectionReader<'a>),
    /// A component export section was received, and the provided reader can be
    /// used to parse the contents of the export section.
    ComponentExportSection(crate::ComponentExportSectionReader<'a>),
    /// A component start section was received, and the provided reader can be
    /// used to parse the contents of the start section.
    ComponentStartSection(crate::ComponentStartSectionReader<'a>),
    /// A component alias section was received and the provided reader can be
    /// used to parse the contents of the alias section.
    AliasSection(crate::AliasSectionReader<'a>),

    /// Indicator of the start of the code section of a WebAssembly module.
    ///
    /// This entry is returned whenever the code section starts. The `count`
    /// field indicates how many entries are in this code section. After
    /// receiving this start marker you're guaranteed that the next `count`
    /// items will be either `CodeSectionEntry` or an error will be returned.
    ///
    /// This, unlike other sections, is intended to be used for streaming the
    /// contents of the code section. The code section is not required to be
    /// fully resident in memory when we parse it. Instead a [`Parser`] is
    /// capable of parsing piece-by-piece of a code section.
    CodeSectionStart {
        /// The number of functions in this section.
        count: u32,
        /// The range of bytes that represent this section, specified in
        /// offsets relative to the start of the byte stream.
        range: Range,
        /// The size, in bytes, of the remaining contents of this section.
        ///
        /// This can be used in combination with [`Parser::skip_section`]
        /// where the caller will know how many bytes to skip before feeding
        /// bytes into `Parser` again.
        size: u32,
    },
    /// An entry of the code section, a function, was parsed from a WebAssembly
    /// module.
    ///
    /// This entry indicates that a function was successfully received from the
    /// code section, and the payload here is the window into the original input
    /// where the function resides. Note that the function itself has not been
    /// parsed, it's only been outlined. You'll need to process the
    /// `FunctionBody` provided to test whether it parses and/or is valid.
    CodeSectionEntry(crate::FunctionBody<'a>),

    /// A module or component custom section was received.
    CustomSection {
        /// The name of the custom section.
        name: &'a str,
        /// The offset, relative to the start of the original module or component,
        /// that the `data` payload for this custom section starts at.
        data_offset: usize,
        /// The actual contents of the custom section.
        data: &'a [u8],
        /// The range of bytes that specify this whole custom section (including
        /// both the name of this custom section and its data) specified in
        /// offsets relative to the start of the byte stream.
        range: Range,
    },

    /// An unknown section was found.
    ///
    /// This variant is returned for all unknown sections encountered. This
    /// likely wants to be interpreted as an error by consumers of the parser,
    /// but this can also be used to parse sections currently unsupported by
    /// the parser.
    UnknownSection {
        /// The 8-bit identifier for this section.
        id: u8,
        /// The contents of this section.
        contents: &'a [u8],
        /// The range of bytes, relative to the start of the original data
        /// stream, that the contents of this section reside in.
        range: Range,
    },

    /// The end of the WebAssembly module or component was reached.
    ///
    /// The value is the offset in the input byte stream where the end
    /// was reached.
    End(usize),
}

impl Parser {
    /// Creates a new parser.
    ///
    /// Reports errors and ranges relative to `offset` provided, where `offset`
    /// is some logical offset within the input stream that we're parsing.
    pub fn new(offset: u64) -> Parser {
        Parser {
            state: State::Header,
            offset,
            max_size: u64::max_value(),
            // Assume the encoding is a module until we know otherwise
            encoding: Encoding::Module,
        }
    }

    /// Attempts to parse a chunk of data.
    ///
    /// This method will attempt to parse the next incremental portion of a
    /// WebAssembly binary. Data available for the module or component is
    /// provided as `data`, and the data can be incomplete if more data has yet
    /// to arrive. The `eof` flag indicates whether more data will ever be received.
    ///
    /// There are two ways parsing can succeed with this method:
    ///
    /// * `Chunk::NeedMoreData` - this indicates that there is not enough bytes
    ///   in `data` to parse a payload. The caller needs to wait for more data to
    ///   be available in this situation before calling this method again. It is
    ///   guaranteed that this is only returned if `eof` is `false`.
    ///
    /// * `Chunk::Parsed` - this indicates that a chunk of the input was
    ///   successfully parsed. The payload is available in this variant of what
    ///   was parsed, and this also indicates how many bytes of `data` was
    ///   consumed. It's expected that the caller will not provide these bytes
    ///   back to the [`Parser`] again.
    ///
    /// Note that all `Chunk` return values are connected, with a lifetime, to
    /// the input buffer. Each parsed chunk borrows the input buffer and is a
    /// view into it for successfully parsed chunks.
    ///
    /// It is expected that you'll call this method until `Payload::End` is
    /// reached, at which point you're guaranteed that the parse has completed.
    /// Note that complete parsing, for the top-level module or component,
    /// implies that `data` is empty and `eof` is `true`.
    ///
    /// # Errors
    ///
    /// Parse errors are returned as an `Err`. Errors can happen when the
    /// structure of the data is unexpected or if sections are too large for
    /// example. Note that errors are not returned for malformed *contents* of
    /// sections here. Sections are generally not individually parsed and each
    /// returned [`Payload`] needs to be iterated over further to detect all
    /// errors.
    ///
    /// # Examples
    ///
    /// An example of reading a wasm file from a stream (`std::io::Read`) and
    /// incrementally parsing it.
    ///
    /// ```
    /// use std::io::Read;
    /// use anyhow::Result;
    /// use wasmparser::{Parser, Chunk, Payload::*};
    ///
    /// fn parse(mut reader: impl Read) -> Result<()> {
    ///     let mut buf = Vec::new();
    ///     let mut parser = Parser::new(0);
    ///     let mut eof = false;
    ///     let mut stack = Vec::new();
    ///
    ///     loop {
    ///         let (payload, consumed) = match parser.parse(&buf, eof)? {
    ///             Chunk::NeedMoreData(hint) => {
    ///                 assert!(!eof); // otherwise an error would be returned
    ///
    ///                 // Use the hint to preallocate more space, then read
    ///                 // some more data into our buffer.
    ///                 //
    ///                 // Note that the buffer management here is not ideal,
    ///                 // but it's compact enough to fit in an example!
    ///                 let len = buf.len();
    ///                 buf.extend((0..hint).map(|_| 0u8));
    ///                 let n = reader.read(&mut buf[len..])?;
    ///                 buf.truncate(len + n);
    ///                 eof = n == 0;
    ///                 continue;
    ///             }
    ///
    ///             Chunk::Parsed { consumed, payload } => (payload, consumed),
    ///         };
    ///
    ///         match payload {
    ///             // Sections for WebAssembly modules
    ///             Version { .. } => { /* ... */ }
    ///             TypeSection(_) => { /* ... */ }
    ///             ImportSection(_) => { /* ... */ }
    ///             FunctionSection(_) => { /* ... */ }
    ///             TableSection(_) => { /* ... */ }
    ///             MemorySection(_) => { /* ... */ }
    ///             TagSection(_) => { /* ... */ }
    ///             GlobalSection(_) => { /* ... */ }
    ///             ExportSection(_) => { /* ... */ }
    ///             StartSection { .. } => { /* ... */ }
    ///             ElementSection(_) => { /* ... */ }
    ///             DataCountSection { .. } => { /* ... */ }
    ///             DataSection(_) => { /* ... */ }
    ///
    ///             // Here we know how many functions we'll be receiving as
    ///             // `CodeSectionEntry`, so we can prepare for that, and
    ///             // afterwards we can parse and handle each function
    ///             // individually.
    ///             CodeSectionStart { .. } => { /* ... */ }
    ///             CodeSectionEntry(body) => {
    ///                 // here we can iterate over `body` to parse the function
    ///                 // and its locals
    ///             }
    ///
    ///             // Sections for WebAssembly components
    ///             ComponentTypeSection(_) => { /* ... */ }
    ///             ComponentImportSection(_) => { /* ... */ }
    ///             ComponentFunctionSection(_) => { /* ... */ }
    ///             ModuleSection { .. } => { /* ... */ }
    ///             ComponentSection { .. } => { /* ... */ }
    ///             InstanceSection(_) => { /* ... */ }
    ///             ComponentExportSection(_) => { /* ... */ }
    ///             ComponentStartSection { .. } => { /* ... */ }
    ///             AliasSection(_) => { /* ... */ }
    ///
    ///             CustomSection { name, .. } => { /* ... */ }
    ///
    ///             // most likely you'd return an error here
    ///             UnknownSection { id, .. } => { /* ... */ }
    ///
    ///             // Once we've reached the end of a parser we either resume
    ///             // at the parent parser or we break out of the loop because
    ///             // we're done.
    ///             End(_) => {
    ///                 if let Some(parent_parser) = stack.pop() {
    ///                     parser = parent_parser;
    ///                 } else {
    ///                     break;
    ///                 }
    ///             }
    ///         }
    ///
    ///         // once we're done processing the payload we can forget the
    ///         // original.
    ///         buf.drain(..consumed);
    ///     }
    ///
    ///     Ok(())
    /// }
    ///
    /// # parse(&b"\0asm\x01\0\0\0"[..]).unwrap();
    /// ```
    pub fn parse<'a>(&mut self, data: &'a [u8], eof: bool) -> Result<Chunk<'a>> {
        let (data, eof) = if usize_to_u64(data.len()) > self.max_size {
            (&data[..(self.max_size as usize)], true)
        } else {
            (data, eof)
        };
        // TODO: thread through `offset: u64` to `BinaryReader`, remove
        // the cast here.
        let mut reader = BinaryReader::new_with_offset(data, self.offset as usize);
        match self.parse_reader(&mut reader, eof) {
            Ok(payload) => {
                // Be sure to update our offset with how far we got in the
                // reader
                self.offset += usize_to_u64(reader.position);
                self.max_size -= usize_to_u64(reader.position);
                Ok(Chunk::Parsed {
                    consumed: reader.position,
                    payload,
                })
            }
            Err(e) => {
                // If we're at EOF then there's no way we can recover from any
                // error, so continue to propagate it.
                if eof {
                    return Err(e);
                }

                // If our error doesn't look like it can be resolved with more
                // data being pulled down, then propagate it, otherwise switch
                // the error to "feed me please"
                match e.inner.needed_hint {
                    Some(hint) => Ok(Chunk::NeedMoreData(usize_to_u64(hint))),
                    None => Err(e),
                }
            }
        }
    }

    fn parse_reader<'a>(
        &mut self,
        reader: &mut BinaryReader<'a>,
        eof: bool,
    ) -> Result<Payload<'a>> {
        use Payload::*;

        match self.state {
            State::Header => {
                let start = reader.original_position();
                let num = reader.read_header_version()?;
                self.encoding = match num {
                    WASM_EXPERIMENTAL_VERSION | WASM_MODULE_VERSION => Encoding::Module,
                    WASM_COMPONENT_VERSION => Encoding::Component,
                    _ => {
                        return Err(BinaryReaderError::new(
                            "unknown binary version",
                            reader.original_position() - 4,
                        ))
                    }
                };
                self.state = State::SectionStart;
                Ok(Version {
                    num,
                    encoding: self.encoding,
                    range: Range {
                        start,
                        end: reader.original_position(),
                    },
                })
            }
            State::SectionStart => {
                // If we're at eof and there are no bytes in our buffer, then
                // that means we reached the end of the data since it's
                // just a bunch of sections concatenated after the header.
                if eof && reader.bytes_remaining() == 0 {
                    return Ok(Payload::End(reader.original_position()));
                }

                let id_pos = reader.position;
                let id = reader.read_u8()?;
                if id & 0x80 != 0 {
                    return Err(BinaryReaderError::new("malformed section id", id_pos));
                }
                let len_pos = reader.position;
                let mut len = reader.read_var_u32()?;

                // Test to make sure that this section actually fits within
                // `Parser::max_size`. This doesn't matter for top-level modules
                // but it is required for nested modules/components to correctly ensure
                // that all sections live entirely within their section of the
                // file.
                let section_overflow = self
                    .max_size
                    .checked_sub(usize_to_u64(reader.position))
                    .and_then(|s| s.checked_sub(len.into()))
                    .is_none();
                if section_overflow {
                    return Err(BinaryReaderError::new("section too large", len_pos));
                }

                // Check for custom sections (supported by all encodings)
                if id == 0 {
                    let start = reader.original_position();
                    let range = Range {
                        start,
                        end: reader.original_position() + len as usize,
                    };
                    let mut content = subreader(reader, len)?;
                    // Note that if this fails we can't read any more bytes,
                    // so clear the "we'd succeed if we got this many more
                    // bytes" because we can't recover from "eof" at this point.
                    let name = content.read_string().map_err(clear_hint)?;
                    return Ok(Payload::CustomSection {
                        name,
                        data_offset: content.original_position(),
                        data: content.remaining_buffer(),
                        range,
                    });
                }

                match (self.encoding, id) {
                    // Module sections
                    (Encoding::Module, 1) => {
                        section(reader, len, TypeSectionReader::new, TypeSection)
                    }
                    (Encoding::Module, 2) => {
                        section(reader, len, ImportSectionReader::new, ImportSection)
                    }
                    (Encoding::Module, 3) => {
                        section(reader, len, FunctionSectionReader::new, FunctionSection)
                    }
                    (Encoding::Module, 4) => {
                        section(reader, len, TableSectionReader::new, TableSection)
                    }
                    (Encoding::Module, 5) => {
                        section(reader, len, MemorySectionReader::new, MemorySection)
                    }
                    (Encoding::Module, 6) => {
                        section(reader, len, GlobalSectionReader::new, GlobalSection)
                    }
                    (Encoding::Module, 7) => {
                        section(reader, len, ExportSectionReader::new, ExportSection)
                    }
                    (Encoding::Module, 8) => {
                        let (func, range) = single_u32(reader, len, "start")?;
                        Ok(StartSection { func, range })
                    }
                    (Encoding::Module, 9) => {
                        section(reader, len, ElementSectionReader::new, ElementSection)
                    }
                    (Encoding::Module, 10) => {
                        let start = reader.original_position();
                        let count = delimited(reader, &mut len, |r| r.read_var_u32())?;
                        let range = Range {
                            start,
                            end: reader.original_position() + len as usize,
                        };
                        self.state = State::FunctionBody {
                            remaining: count,
                            len,
                        };
                        Ok(CodeSectionStart {
                            count,
                            range,
                            size: len,
                        })
                    }
                    (Encoding::Module, 11) => {
                        section(reader, len, DataSectionReader::new, DataSection)
                    }
                    (Encoding::Module, 12) => {
                        let (count, range) = single_u32(reader, len, "data count")?;
                        Ok(DataCountSection { count, range })
                    }
                    (Encoding::Module, 13) => {
                        section(reader, len, TagSectionReader::new, TagSection)
                    }

                    // Component sections
                    (Encoding::Component, 1) => section(
                        reader,
                        len,
                        ComponentTypeSectionReader::new,
                        ComponentTypeSection,
                    ),
                    (Encoding::Component, 2) => section(
                        reader,
                        len,
                        ComponentImportSectionReader::new,
                        ComponentImportSection,
                    ),
                    (Encoding::Component, 3) => section(
                        reader,
                        len,
                        ComponentFunctionSectionReader::new,
                        ComponentFunctionSection,
                    ),
                    (Encoding::Component, 4) | (Encoding::Component, 5) => {
                        if len as usize > MAX_WASM_MODULE_SIZE {
                            return Err(BinaryReaderError::new(
                                format!(
                                    "{} section is too large",
                                    if id == 4 { "module" } else { "component " }
                                ),
                                len_pos,
                            ));
                        }

                        let range = Range {
                            start: reader.original_position(),
                            end: reader.original_position() + len as usize,
                        };
                        self.max_size -= u64::from(len);
                        self.offset += u64::from(len);
                        let mut parser = Parser::new(usize_to_u64(reader.original_position()));
                        parser.max_size = len.into();

                        Ok(match id {
                            4 => ModuleSection { parser, range },
                            5 => ComponentSection { parser, range },
                            _ => unreachable!(),
                        })
                    }
                    (Encoding::Component, 6) => {
                        section(reader, len, InstanceSectionReader::new, InstanceSection)
                    }
                    (Encoding::Component, 7) => section(
                        reader,
                        len,
                        ComponentExportSectionReader::new,
                        ComponentExportSection,
                    ),
                    (Encoding::Component, 8) => section(
                        reader,
                        len,
                        ComponentStartSectionReader::new,
                        ComponentStartSection,
                    ),
                    (Encoding::Component, 9) => {
                        section(reader, len, AliasSectionReader::new, AliasSection)
                    }
                    (_, id) => {
                        let offset = reader.original_position();
                        let contents = reader.read_bytes(len as usize)?;
                        let range = Range {
                            start: offset,
                            end: offset + len as usize,
                        };
                        Ok(UnknownSection {
                            id,
                            contents,
                            range,
                        })
                    }
                }
            }

            // Once we hit 0 remaining incrementally parsed items, with 0
            // remaining bytes in each section, we're done and can switch back
            // to parsing sections.
            State::FunctionBody {
                remaining: 0,
                len: 0,
            } => {
                self.state = State::SectionStart;
                self.parse_reader(reader, eof)
            }

            // ... otherwise trailing bytes with no remaining entries in these
            // sections indicates an error.
            State::FunctionBody { remaining: 0, len } => {
                debug_assert!(len > 0);
                let offset = reader.original_position();
                Err(BinaryReaderError::new(
                    "trailing bytes at end of section",
                    offset,
                ))
            }

            // Functions are relatively easy to parse when we know there's at
            // least one remaining and at least one byte available to read
            // things.
            //
            // We use the remaining length try to read a u32 size of the
            // function, and using that size we require the entire function be
            // resident in memory. This means that we're reading whole chunks of
            // functions at a time.
            //
            // Limiting via `Parser::max_size` (nested parsing) happens above in
            // `fn parse`, and limiting by our section size happens via
            // `delimited`. Actual parsing of the function body is delegated to
            // the caller to iterate over the `FunctionBody` structure.
            State::FunctionBody { remaining, mut len } => {
                let body = delimited(reader, &mut len, |r| {
                    let size = r.read_var_u32()?;
                    let offset = r.original_position();
                    Ok(FunctionBody::new(offset, r.read_bytes(size as usize)?))
                })?;
                self.state = State::FunctionBody {
                    remaining: remaining - 1,
                    len,
                };
                Ok(CodeSectionEntry(body))
            }
        }
    }

    /// Convenience function that can be used to parse a module or component
    /// that is entirely resident in memory.
    ///
    /// This function will parse the `data` provided as a WebAssembly module
    /// or component.
    ///
    /// Note that when this function yields sections that provide parsers,
    /// no further action is required for those sections as payloads from
    /// those parsers will be automatically returned.
    pub fn parse_all(self, mut data: &[u8]) -> impl Iterator<Item = Result<Payload>> {
        let mut stack = Vec::new();
        let mut cur = self;
        let mut done = false;
        iter::from_fn(move || {
            if done {
                return None;
            }
            let payload = match cur.parse(data, true) {
                // Propagate all errors
                Err(e) => {
                    done = true;
                    return Some(Err(e));
                }

                // This isn't possible because `eof` is always true.
                Ok(Chunk::NeedMoreData(_)) => unreachable!(),

                Ok(Chunk::Parsed { payload, consumed }) => {
                    data = &data[consumed..];
                    payload
                }
            };

            match &payload {
                Payload::ModuleSection { parser, .. }
                | Payload::ComponentSection { parser, .. } => {
                    stack.push(cur.clone());
                    cur = parser.clone();
                }
                Payload::End(_) => match stack.pop() {
                    Some(p) => cur = p,
                    None => done = true,
                },

                _ => {}
            }

            Some(Ok(payload))
        })
    }

    /// Skip parsing the code section entirely.
    ///
    /// This function can be used to indicate, after receiving
    /// `CodeSectionStart`, that the section will not be parsed.
    ///
    /// The caller will be responsible for skipping `size` bytes (found in the
    /// `CodeSectionStart` payload). Bytes should only be fed into `parse`
    /// after the `size` bytes have been skipped.
    ///
    /// # Panics
    ///
    /// This function will panic if the parser is not in a state where it's
    /// parsing the code section.
    ///
    /// # Examples
    ///
    /// ```
    /// use wasmparser::{Result, Parser, Chunk, Range, SectionReader, Payload::*};
    ///
    /// fn objdump_headers(mut wasm: &[u8]) -> Result<()> {
    ///     let mut parser = Parser::new(0);
    ///     loop {
    ///         let payload = match parser.parse(wasm, true)? {
    ///             Chunk::Parsed { consumed, payload } => {
    ///                 wasm = &wasm[consumed..];
    ///                 payload
    ///             }
    ///             // this state isn't possible with `eof = true`
    ///             Chunk::NeedMoreData(_) => unreachable!(),
    ///         };
    ///         match payload {
    ///             TypeSection(s) => print_range("type section", &s.range()),
    ///             ImportSection(s) => print_range("import section", &s.range()),
    ///             // .. other sections
    ///
    ///             // Print the range of the code section we see, but don't
    ///             // actually iterate over each individual function.
    ///             CodeSectionStart { range, size, .. } => {
    ///                 print_range("code section", &range);
    ///                 parser.skip_section();
    ///                 wasm = &wasm[size as usize..];
    ///             }
    ///             End(_) => break,
    ///             _ => {}
    ///         }
    ///     }
    ///     Ok(())
    /// }
    ///
    /// fn print_range(section: &str, range: &Range) {
    ///     println!("{:>40}: {:#010x} - {:#010x}", section, range.start, range.end);
    /// }
    /// ```
    pub fn skip_section(&mut self) {
        let skip = match self.state {
            State::FunctionBody { remaining: _, len } => len,
            _ => panic!("wrong state to call `skip_section`"),
        };
        self.offset += u64::from(skip);
        self.max_size -= u64::from(skip);
        self.state = State::SectionStart;
    }
}

fn usize_to_u64(a: usize) -> u64 {
    a.try_into().unwrap()
}

/// Parses an entire section resident in memory into a `Payload`.
///
/// Requires that `len` bytes are resident in `reader` and uses `ctor`/`variant`
/// to construct the section to return.
fn section<'a, T>(
    reader: &mut BinaryReader<'a>,
    len: u32,
    ctor: fn(&'a [u8], usize) -> Result<T>,
    variant: fn(T) -> Payload<'a>,
) -> Result<Payload<'a>> {
    let offset = reader.original_position();
    let payload = reader.read_bytes(len as usize)?;
    // clear the hint for "need this many more bytes" here because we already
    // read all the bytes, so it's not possible to read more bytes if this
    // fails.
    let reader = ctor(payload, offset).map_err(clear_hint)?;
    Ok(variant(reader))
}

/// Creates a new `BinaryReader` from the given `reader` which will be reading
/// the first `len` bytes.
///
/// This means that `len` bytes must be resident in memory at the time of this
/// reading.
fn subreader<'a>(reader: &mut BinaryReader<'a>, len: u32) -> Result<BinaryReader<'a>> {
    let offset = reader.original_position();
    let payload = reader.read_bytes(len as usize)?;
    Ok(BinaryReader::new_with_offset(payload, offset))
}

/// Reads a section that is represented by a single uleb-encoded `u32`.
fn single_u32<'a>(reader: &mut BinaryReader<'a>, len: u32, desc: &str) -> Result<(u32, Range)> {
    let range = Range {
        start: reader.original_position(),
        end: reader.original_position() + len as usize,
    };
    let mut content = subreader(reader, len)?;
    // We can't recover from "unexpected eof" here because our entire section is
    // already resident in memory, so clear the hint for how many more bytes are
    // expected.
    let index = content.read_var_u32().map_err(clear_hint)?;
    if !content.eof() {
        return Err(BinaryReaderError::new(
            format!("unexpected content in the {} section", desc),
            content.original_position(),
        ));
    }
    Ok((index, range))
}

/// Attempts to parse using `f`.
///
/// This will update `*len` with the number of bytes consumed, and it will cause
/// a failure to be returned instead of the number of bytes consumed exceeds
/// what `*len` currently is.
fn delimited<'a, T>(
    reader: &mut BinaryReader<'a>,
    len: &mut u32,
    f: impl FnOnce(&mut BinaryReader<'a>) -> Result<T>,
) -> Result<T> {
    let start = reader.position;
    let ret = f(reader)?;
    *len = match (reader.position - start)
        .try_into()
        .ok()
        .and_then(|i| len.checked_sub(i))
    {
        Some(i) => i,
        None => return Err(BinaryReaderError::new("unexpected end-of-file", start)),
    };
    Ok(ret)
}

impl Default for Parser {
    fn default() -> Parser {
        Parser::new(0)
    }
}

impl fmt::Debug for Payload<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use Payload::*;
        match self {
            Version {
                num,
                encoding,
                range,
            } => f
                .debug_struct("Version")
                .field("num", num)
                .field("encoding", encoding)
                .field("range", range)
                .finish(),

            // Module sections
            TypeSection(_) => f.debug_tuple("TypeSection").field(&"...").finish(),
            ImportSection(_) => f.debug_tuple("ImportSection").field(&"...").finish(),
            FunctionSection(_) => f.debug_tuple("FunctionSection").field(&"...").finish(),
            TableSection(_) => f.debug_tuple("TableSection").field(&"...").finish(),
            MemorySection(_) => f.debug_tuple("MemorySection").field(&"...").finish(),
            TagSection(_) => f.debug_tuple("TagSection").field(&"...").finish(),
            GlobalSection(_) => f.debug_tuple("GlobalSection").field(&"...").finish(),
            ExportSection(_) => f.debug_tuple("ExportSection").field(&"...").finish(),
            ElementSection(_) => f.debug_tuple("ElementSection").field(&"...").finish(),
            DataSection(_) => f.debug_tuple("DataSection").field(&"...").finish(),
            StartSection { func, range } => f
                .debug_struct("StartSection")
                .field("func", func)
                .field("range", range)
                .finish(),
            DataCountSection { count, range } => f
                .debug_struct("DataCountSection")
                .field("count", count)
                .field("range", range)
                .finish(),
            CodeSectionStart { count, range, size } => f
                .debug_struct("CodeSectionStart")
                .field("count", count)
                .field("range", range)
                .field("size", size)
                .finish(),
            CodeSectionEntry(_) => f.debug_tuple("CodeSectionEntry").field(&"...").finish(),

            // Component sections
            ComponentTypeSection(_) => f.debug_tuple("ComponentTypeSection").field(&"...").finish(),
            ComponentImportSection(_) => f
                .debug_tuple("ComponentImportSection")
                .field(&"...")
                .finish(),
            ComponentFunctionSection(_) => f
                .debug_tuple("ComponentFunctionSection")
                .field(&"...")
                .finish(),
            ModuleSection { parser: _, range } => f
                .debug_struct("ModuleSection")
                .field("range", range)
                .finish(),
            ComponentSection { parser: _, range } => f
                .debug_struct("ComponentSection")
                .field("range", range)
                .finish(),
            InstanceSection(_) => f.debug_tuple("InstanceSection").field(&"...").finish(),
            ComponentExportSection(_) => f
                .debug_tuple("ComponentExportSection")
                .field(&"...")
                .finish(),
            ComponentStartSection(_) => f
                .debug_tuple("ComponentStartSection")
                .field(&"...")
                .finish(),
            AliasSection(_) => f.debug_tuple("AliasSection").field(&"...").finish(),

            CustomSection {
                name,
                data_offset,
                data: _,
                range,
            } => f
                .debug_struct("CustomSection")
                .field("name", name)
                .field("data_offset", data_offset)
                .field("range", range)
                .field("data", &"...")
                .finish(),

            UnknownSection { id, range, .. } => f
                .debug_struct("UnknownSection")
                .field("id", id)
                .field("range", range)
                .finish(),

            End(offset) => f.debug_tuple("End").field(offset).finish(),
        }
    }
}

fn clear_hint(mut err: BinaryReaderError) -> BinaryReaderError {
    err.inner.needed_hint = None;
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! assert_matches {
        ($a:expr, $b:pat $(,)?) => {
            match $a {
                $b => {}
                a => panic!("`{:?}` doesn't match `{}`", a, stringify!($b)),
            }
        };
    }

    #[test]
    fn header() {
        assert!(Parser::default().parse(&[], true).is_err());
        assert_matches!(
            Parser::default().parse(&[], false),
            Ok(Chunk::NeedMoreData(4)),
        );
        assert_matches!(
            Parser::default().parse(b"\0", false),
            Ok(Chunk::NeedMoreData(3)),
        );
        assert_matches!(
            Parser::default().parse(b"\0asm", false),
            Ok(Chunk::NeedMoreData(4)),
        );
        assert_matches!(
            Parser::default().parse(b"\0asm\x01\0\0\0", false),
            Ok(Chunk::Parsed {
                consumed: 8,
                payload: Payload::Version { num: 1, .. },
            }),
        );
    }

    #[test]
    fn header_iter() {
        for _ in Parser::default().parse_all(&[]) {}
        for _ in Parser::default().parse_all(b"\0") {}
        for _ in Parser::default().parse_all(b"\0asm") {}
        for _ in Parser::default().parse_all(b"\0asm\x01\x01\x01\x01") {}
    }

    fn parser_after_header() -> Parser {
        let mut p = Parser::default();
        assert_matches!(
            p.parse(b"\0asm\x01\0\0\0", false),
            Ok(Chunk::Parsed {
                consumed: 8,
                payload: Payload::Version {
                    num: WASM_MODULE_VERSION,
                    encoding: Encoding::Module,
                    ..
                },
            }),
        );
        p
    }

    fn parser_after_component_header() -> Parser {
        let mut p = Parser::default();
        assert_matches!(
            p.parse(b"\0asm\x0a\0\x01\0", false),
            Ok(Chunk::Parsed {
                consumed: 8,
                payload: Payload::Version {
                    num: WASM_COMPONENT_VERSION,
                    encoding: Encoding::Component,
                    ..
                },
            }),
        );
        p
    }

    #[test]
    fn start_section() {
        assert_matches!(
            parser_after_header().parse(&[], false),
            Ok(Chunk::NeedMoreData(1)),
        );
        assert!(parser_after_header().parse(&[8], true).is_err());
        assert!(parser_after_header().parse(&[8, 1], true).is_err());
        assert!(parser_after_header().parse(&[8, 2], true).is_err());
        assert_matches!(
            parser_after_header().parse(&[8], false),
            Ok(Chunk::NeedMoreData(1)),
        );
        assert_matches!(
            parser_after_header().parse(&[8, 1], false),
            Ok(Chunk::NeedMoreData(1)),
        );
        assert_matches!(
            parser_after_header().parse(&[8, 2], false),
            Ok(Chunk::NeedMoreData(2)),
        );
        assert_matches!(
            parser_after_header().parse(&[8, 1, 1], false),
            Ok(Chunk::Parsed {
                consumed: 3,
                payload: Payload::StartSection { func: 1, .. },
            }),
        );
        assert!(parser_after_header().parse(&[8, 2, 1, 1], false).is_err());
        assert!(parser_after_header().parse(&[8, 0], false).is_err());
    }

    #[test]
    fn end_works() {
        assert_matches!(
            parser_after_header().parse(&[], true),
            Ok(Chunk::Parsed {
                consumed: 0,
                payload: Payload::End(8),
            }),
        );
    }

    #[test]
    fn type_section() {
        assert!(parser_after_header().parse(&[1], true).is_err());
        assert!(parser_after_header().parse(&[1, 0], false).is_err());
        assert!(parser_after_header().parse(&[8, 2], true).is_err());
        assert_matches!(
            parser_after_header().parse(&[1], false),
            Ok(Chunk::NeedMoreData(1)),
        );
        assert_matches!(
            parser_after_header().parse(&[1, 1], false),
            Ok(Chunk::NeedMoreData(1)),
        );
        assert_matches!(
            parser_after_header().parse(&[1, 1, 1], false),
            Ok(Chunk::Parsed {
                consumed: 3,
                payload: Payload::TypeSection(_),
            }),
        );
        assert_matches!(
            parser_after_header().parse(&[1, 1, 1, 2, 3, 4], false),
            Ok(Chunk::Parsed {
                consumed: 3,
                payload: Payload::TypeSection(_),
            }),
        );
    }

    #[test]
    fn custom_section() {
        assert!(parser_after_header().parse(&[0], true).is_err());
        assert!(parser_after_header().parse(&[0, 0], false).is_err());
        assert!(parser_after_header().parse(&[0, 1, 1], false).is_err());
        assert_matches!(
            parser_after_header().parse(&[0, 2, 1], false),
            Ok(Chunk::NeedMoreData(1)),
        );
        assert_matches!(
            parser_after_header().parse(&[0, 1, 0], false),
            Ok(Chunk::Parsed {
                consumed: 3,
                payload: Payload::CustomSection {
                    name: "",
                    data_offset: 11,
                    data: b"",
                    range: Range { start: 10, end: 11 },
                },
            }),
        );
        assert_matches!(
            parser_after_header().parse(&[0, 2, 1, b'a'], false),
            Ok(Chunk::Parsed {
                consumed: 4,
                payload: Payload::CustomSection {
                    name: "a",
                    data_offset: 12,
                    data: b"",
                    range: Range { start: 10, end: 12 },
                },
            }),
        );
        assert_matches!(
            parser_after_header().parse(&[0, 2, 0, b'a'], false),
            Ok(Chunk::Parsed {
                consumed: 4,
                payload: Payload::CustomSection {
                    name: "",
                    data_offset: 11,
                    data: b"a",
                    range: Range { start: 10, end: 12 },
                },
            }),
        );
    }

    #[test]
    fn function_section() {
        assert!(parser_after_header().parse(&[10], true).is_err());
        assert!(parser_after_header().parse(&[10, 0], true).is_err());
        assert!(parser_after_header().parse(&[10, 1], true).is_err());
        assert_matches!(
            parser_after_header().parse(&[10], false),
            Ok(Chunk::NeedMoreData(1))
        );
        assert_matches!(
            parser_after_header().parse(&[10, 1], false),
            Ok(Chunk::NeedMoreData(1))
        );
        let mut p = parser_after_header();
        assert_matches!(
            p.parse(&[10, 1, 0], false),
            Ok(Chunk::Parsed {
                consumed: 3,
                payload: Payload::CodeSectionStart { count: 0, .. },
            }),
        );
        assert_matches!(
            p.parse(&[], true),
            Ok(Chunk::Parsed {
                consumed: 0,
                payload: Payload::End(11),
            }),
        );
        let mut p = parser_after_header();
        assert_matches!(
            p.parse(&[10, 2, 1, 0], false),
            Ok(Chunk::Parsed {
                consumed: 3,
                payload: Payload::CodeSectionStart { count: 1, .. },
            }),
        );
        assert_matches!(
            p.parse(&[0], false),
            Ok(Chunk::Parsed {
                consumed: 1,
                payload: Payload::CodeSectionEntry(_),
            }),
        );
        assert_matches!(
            p.parse(&[], true),
            Ok(Chunk::Parsed {
                consumed: 0,
                payload: Payload::End(12),
            }),
        );

        // 1 byte section with 1 function can't read the function body because
        // the section is too small
        let mut p = parser_after_header();
        assert_matches!(
            p.parse(&[10, 1, 1], false),
            Ok(Chunk::Parsed {
                consumed: 3,
                payload: Payload::CodeSectionStart { count: 1, .. },
            }),
        );
        assert_eq!(
            p.parse(&[0], false).unwrap_err().message(),
            "unexpected end-of-file"
        );

        // section with 2 functions but section is cut off
        let mut p = parser_after_header();
        assert_matches!(
            p.parse(&[10, 2, 2], false),
            Ok(Chunk::Parsed {
                consumed: 3,
                payload: Payload::CodeSectionStart { count: 2, .. },
            }),
        );
        assert_matches!(
            p.parse(&[0], false),
            Ok(Chunk::Parsed {
                consumed: 1,
                payload: Payload::CodeSectionEntry(_),
            }),
        );
        assert_matches!(p.parse(&[], false), Ok(Chunk::NeedMoreData(1)));
        assert_eq!(
            p.parse(&[0], false).unwrap_err().message(),
            "unexpected end-of-file",
        );

        // trailing data is bad
        let mut p = parser_after_header();
        assert_matches!(
            p.parse(&[10, 3, 1], false),
            Ok(Chunk::Parsed {
                consumed: 3,
                payload: Payload::CodeSectionStart { count: 1, .. },
            }),
        );
        assert_matches!(
            p.parse(&[0], false),
            Ok(Chunk::Parsed {
                consumed: 1,
                payload: Payload::CodeSectionEntry(_),
            }),
        );
        assert_eq!(
            p.parse(&[0], false).unwrap_err().message(),
            "trailing bytes at end of section",
        );
    }

    #[test]
    fn single_module() {
        let mut p = parser_after_component_header();
        assert_matches!(p.parse(&[4], false), Ok(Chunk::NeedMoreData(1)));

        // A module that's 8 bytes in length
        let mut sub = match p.parse(&[4, 8], false) {
            Ok(Chunk::Parsed {
                consumed: 2,
                payload: Payload::ModuleSection { parser, .. },
            }) => parser,
            other => panic!("bad parse {:?}", other),
        };

        // Parse the header of the submodule with the sub-parser.
        assert_matches!(sub.parse(&[], false), Ok(Chunk::NeedMoreData(4)));
        assert_matches!(sub.parse(b"\0asm", false), Ok(Chunk::NeedMoreData(4)));
        assert_matches!(
            sub.parse(b"\0asm\x01\0\0\0", false),
            Ok(Chunk::Parsed {
                consumed: 8,
                payload: Payload::Version {
                    num: 1,
                    encoding: Encoding::Module,
                    ..
                },
            }),
        );

        // The sub-parser should be byte-limited so the next byte shouldn't get
        // consumed, it's intended for the parent parser.
        assert_matches!(
            sub.parse(&[10], false),
            Ok(Chunk::Parsed {
                consumed: 0,
                payload: Payload::End(18),
            }),
        );

        // The parent parser should now be back to resuming, and we simulate it
        // being done with bytes to ensure that it's safely at the end,
        // completing the module code section.
        assert_matches!(p.parse(&[], false), Ok(Chunk::NeedMoreData(1)));
        assert_matches!(
            p.parse(&[], true),
            Ok(Chunk::Parsed {
                consumed: 0,
                payload: Payload::End(18),
            }),
        );
    }

    #[test]
    fn nested_section_too_big() {
        let mut p = parser_after_component_header();

        // A module that's 10 bytes in length
        let mut sub = match p.parse(&[4, 10], false) {
            Ok(Chunk::Parsed {
                consumed: 2,
                payload: Payload::ModuleSection { parser, .. },
            }) => parser,
            other => panic!("bad parse {:?}", other),
        };

        // use 8 bytes to parse the header, leaving 2 remaining bytes in our
        // module.
        assert_matches!(
            sub.parse(b"\0asm\x01\0\0\0", false),
            Ok(Chunk::Parsed {
                consumed: 8,
                payload: Payload::Version { num: 1, .. },
            }),
        );

        // We can't parse a section which declares its bigger than the outer
        // module. This is a custom section, one byte big, with one content byte. The
        // content byte, however, lives outside of the parent's module code
        // section.
        assert_eq!(
            sub.parse(&[0, 1, 0], false).unwrap_err().message(),
            "section too large",
        );
    }
}
