use crate::args::Args;
use crate::elf;
use crate::elf::DynamicEntry;
use crate::elf::DynamicTag;
use crate::elf::EhFrameHdr;
use crate::elf::EhFrameHdrEntry;
use crate::elf::FileHeader;
use crate::elf::ProgramHeader;
use crate::elf::RelocationKind;
use crate::elf::RelocationKindInfo;
use crate::elf::SectionHeader;
use crate::elf::SegmentType;
use crate::elf::SymtabEntry;
use crate::elf::PLT_ENTRY_TEMPLATE;
use crate::error::Result;
use crate::input_data::INTERNAL_FILE_ID;
use crate::layout::FileLayout;
use crate::layout::HeaderInfo;
use crate::layout::InternalLayout;
use crate::layout::Layout;
use crate::layout::ObjectLayout;
use crate::layout::Resolution;
use crate::layout::Section;
use crate::layout::SymbolResolution;
use crate::layout::TargetResolutionKind;
use crate::layout::TlsMode;
use crate::output_section_id;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::relaxation::Relaxation;
use crate::resolution::LocalSymbolResolution;
use crate::resolution::SectionSlot;
use crate::slice::slice_take_prefix_mut;
use crate::symbol_db::GlobalSymbolId;
use crate::symbol_db::SymbolDb;
use ahash::AHashMap;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use memmap2::MmapOptions;
use object::Object;
use object::ObjectSection;
use object::ObjectSymbol;
use rayon::prelude::*;
use std::fmt::Display;
use std::ops::Range;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub(crate) struct Output {
    path: Arc<Path>,
    creator: FileCreator,
}

enum FileCreator {
    Background {
        sized_output_sender: Option<Sender<Result<SizedOutput>>>,
        sized_output_recv: Receiver<Result<SizedOutput>>,
    },
    Regular {
        file_size: Option<u64>,
    },
}

struct SizedOutput {
    file: std::fs::File,
    mmap: memmap2::MmapMut,
    path: Arc<Path>,
}

#[derive(Debug)]
struct SectionAllocation {
    id: OutputSectionId,
    offset: usize,
    size: usize,
}

impl Output {
    pub(crate) fn new(args: &Args) -> Output {
        if args.num_threads.get() > 1 {
            let (sized_output_sender, sized_output_recv) = std::sync::mpsc::channel();
            Output {
                path: args.output.clone(),
                creator: FileCreator::Background {
                    sized_output_sender: Some(sized_output_sender),
                    sized_output_recv,
                },
            }
        } else {
            Output {
                path: args.output.clone(),
                creator: FileCreator::Regular { file_size: None },
            }
        }
    }

    pub(crate) fn set_size(&mut self, size: u64) {
        match &mut self.creator {
            FileCreator::Background {
                sized_output_sender,
                sized_output_recv: _,
            } => {
                let sender = sized_output_sender
                    .take()
                    .expect("set_size must only be called once");
                let path = self.path.clone();
                rayon::spawn(move || {
                    let _ = sender.send(SizedOutput::new(path, size));
                });
            }
            FileCreator::Regular { file_size } => *file_size = Some(size),
        }
    }

    #[tracing::instrument(skip_all, name = "Write output file")]
    pub(crate) fn write(&mut self, layout: &Layout) -> Result {
        let mut sized_output = match &self.creator {
            FileCreator::Background {
                sized_output_sender,
                sized_output_recv,
            } => {
                assert!(sized_output_sender.is_none(), "set_size was never called");
                wait_for_sized_output(sized_output_recv)?
            }
            FileCreator::Regular { file_size } => {
                let file_size = file_size.context("set_size was never called")?;
                self.create_file_non_lazily(file_size)?
            }
        };
        sized_output.write(layout)
    }

    #[tracing::instrument(skip_all, name = "Create output file")]
    fn create_file_non_lazily(&mut self, file_size: u64) -> Result<SizedOutput> {
        SizedOutput::new(self.path.clone(), file_size)
    }
}

#[tracing::instrument(skip_all, name = "Wait for output file creation")]
fn wait_for_sized_output(sized_output_recv: &Receiver<Result<SizedOutput>>) -> Result<SizedOutput> {
    sized_output_recv.recv()?
}

impl SizedOutput {
    fn new(path: Arc<Path>, file_size: u64) -> Result<SizedOutput> {
        let _ = std::fs::remove_file(&path);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .with_context(|| format!("Failed to open `{}`", path.display()))?;
        file.set_len(file_size)?;
        let mmap = unsafe { MmapOptions::new().map_mut(&file) }
            .with_context(|| format!("Failed to mmap output file `{}`", path.display()))?;
        Ok(SizedOutput { file, mmap, path })
    }

    pub(crate) fn write(&mut self, layout: &Layout) -> Result {
        self.write_file_contents(layout)?;

        // We consumed the .eh_frame_hdr section in `split_buffers_by_alignment` above, get a fresh copy.
        let mut section_buffers = split_output_into_sections(layout, &mut self.mmap);
        sort_eh_frame_hdr_entries(section_buffers.get_mut(output_section_id::EH_FRAME_HDR));
        crate::fs::make_executable(&self.file)
            .with_context(|| format!("Failed to make `{}` executable", self.path.display()))?;
        Ok(())
    }

    #[tracing::instrument(skip_all, name = "Write data to file")]
    pub(crate) fn write_file_contents(&mut self, layout: &Layout) -> Result {
        let mut section_buffers = split_output_into_sections(layout, &mut self.mmap);

        let mut writable_buckets = split_buffers_by_alignment(&mut section_buffers, layout);
        let files_and_buffers: Vec<_> = layout
            .file_layouts
            .iter()
            .map(|file| {
                if let Some(file_sizes) = file.file_sizes(&layout.output_sections) {
                    (file, writable_buckets.take_mut(&file_sizes))
                } else {
                    (
                        file,
                        OutputSectionPartMap::with_size(layout.output_sections.len()),
                    )
                }
            })
            .collect();
        files_and_buffers
            .into_par_iter()
            .map(|(file, buffer)| {
                file.write(buffer, layout)
                    .with_context(|| format!("Failed copying from {file} to output file"))
            })
            .collect::<Result>()?;
        Ok(())
    }
}

fn split_output_into_sections<'out>(
    layout: &Layout<'_>,
    mmap: &'out mut memmap2::MmapMut,
) -> OutputSectionMap<&'out mut [u8]> {
    let mut section_allocations = Vec::with_capacity(layout.section_layouts.len());
    layout.section_layouts.for_each(|id, s| {
        section_allocations.push(SectionAllocation {
            id,
            offset: s.file_offset,
            size: s.file_size,
        })
    });
    section_allocations.sort_by_key(|s| (s.offset, s.offset + s.size));

    let mut data = mmap.as_mut();
    // OutputSectionMap is ordered by section ID, which is not the same as output order. We
    // split the output file by output order, putting the relevant parts of the buffer into the
    // map.
    let mut section_data = OutputSectionMap::with_size(section_allocations.len());
    let mut offset = 0;
    for a in section_allocations {
        let Some(padding) = a.offset.checked_sub(offset) else {
            panic!(
                "Offsets went backward when splitting output file {offset} to {}",
                a.offset
            );
        };
        slice_take_prefix_mut(&mut data, padding);
        *section_data.get_mut(a.id) = slice_take_prefix_mut(&mut data, a.size);
        offset = a.offset + a.size;
    }
    section_data
}

#[tracing::instrument(skip_all, name = "Sort .eh_frame_hdr")]
fn sort_eh_frame_hdr_entries(eh_frame_hdr: &mut [u8]) {
    let entry_bytes = &mut eh_frame_hdr[core::mem::size_of::<elf::EhFrameHdr>()..];
    let entries: &mut [elf::EhFrameHdrEntry] = bytemuck::cast_slice_mut(entry_bytes);
    entries.sort_by_key(|e| e.frame_ptr);
}

/// Splits the writable buffers for each segment further into separate buffers for each alignment.
fn split_buffers_by_alignment<'out>(
    section_buffers: &'out mut OutputSectionMap<&mut [u8]>,
    layout: &Layout,
) -> OutputSectionPartMap<&'out mut [u8]> {
    layout
        .section_part_layouts
        .output_order_map(&layout.output_sections, |section_id, _, rec| {
            crate::slice::slice_take_prefix_mut(section_buffers.get_mut(section_id), rec.file_size)
        })
}

fn write_program_headers(program_headers_out: &mut ProgramHeaderWriter, layout: &Layout) -> Result {
    for segment_layout in layout.segment_layouts.segments.iter() {
        let segment_sizes = &segment_layout.sizes;
        let segment_id = segment_layout.id;
        let segment_header = program_headers_out.take_header()?;
        let mut alignment = segment_sizes.alignment;
        if segment_id.segment_type() == SegmentType::Load {
            alignment = alignment.max(crate::alignment::PAGE);
        }
        *segment_header = ProgramHeader {
            segment_type: segment_id.segment_type() as u32,
            flags: segment_id.segment_flags(),
            offset: segment_sizes.file_offset as u64,
            virtual_addr: segment_sizes.mem_offset,
            physical_addr: segment_sizes.mem_offset,
            file_size: segment_sizes.file_size as u64,
            mem_size: segment_sizes.mem_size,
            alignment: alignment.value(),
        };
    }
    Ok(())
}

impl FileHeader {
    fn build(layout: &Layout, header_info: &HeaderInfo) -> Result<Self> {
        let args = layout.args();
        let ty = if args.pie {
            elf::FileType::SharedObject
        } else {
            elf::FileType::Executable
        };
        Ok(Self {
            magic: [0x7f, b'E', b'L', b'F'],
            class: 2, // 64 bit
            data: 1,  // Little endian
            ei_version: 1,
            os_abi: 0,
            abi_version: 0,
            padding: [0; 7],
            ty: ty as u16,
            machine: 0x3e, // x86-64
            e_version: 1,
            entry_point: layout.entry_symbol_address()?,

            program_header_offset: elf::PHEADER_OFFSET,
            section_header_offset: u64::from(elf::FILE_HEADER_SIZE)
                + header_info.program_headers_size(),
            flags: 0,
            ehsize: elf::FILE_HEADER_SIZE,
            program_header_entry_size: elf::PROGRAM_HEADER_SIZE,
            program_header_num: header_info.active_segment_ids.len() as u16,
            section_header_entry_size: elf::SECTION_HEADER_SIZE,
            section_header_num: header_info.num_output_sections_with_content,
            section_names_index: layout
                .output_sections
                .output_index_of_section(crate::output_section_id::SHSTRTAB)
                .expect("we always write .shstrtab"),
        })
    }
}

impl<'data> FileLayout<'data> {
    fn write(&self, buffers: OutputSectionPartMap<&mut [u8]>, layout: &Layout) -> Result {
        match self {
            Self::Object(s) => s.write(buffers, layout)?,
            Self::Internal(s) => s.write(buffers, layout)?,
            Self::Dynamic(_) => {}
        }
        Ok(())
    }
}

struct PltGotWriter<'data, 'out> {
    layout: &'data Layout<'data>,
    got: &'out mut [u64],
    plt: &'out mut [u8],
    rela_plt: &'out mut [elf::Rela],
    tls: Range<u64>,
}

impl<'data, 'out> PltGotWriter<'data, 'out> {
    fn new(
        layout: &'data Layout,
        buffers: &mut OutputSectionPartMap<&'out mut [u8]>,
    ) -> PltGotWriter<'data, 'out> {
        PltGotWriter {
            layout,
            got: bytemuck::cast_slice_mut(core::mem::take(&mut buffers.got)),
            plt: core::mem::take(&mut buffers.plt),
            rela_plt: bytemuck::cast_slice_mut(core::mem::take(&mut buffers.rela_plt)),
            tls: layout.tls_start_address()..layout.tls_end_address(),
        }
    }

    fn process_symbol(
        &mut self,
        symbol_id: GlobalSymbolId,
        relocation_writer: &mut RelocationWriter,
    ) -> Result {
        match self.layout.global_symbol_resolution(symbol_id) {
            Some(SymbolResolution::Resolved(res)) => {
                self.process_resolution(res, relocation_writer)?;
            }
            Some(SymbolResolution::Dynamic) => {}
            None => {}
        }
        Ok(())
    }

    fn process_resolution(
        &mut self,
        res: &Resolution,
        relocation_writer: &mut RelocationWriter,
    ) -> Result {
        if let Some(got_address) = res.got_address {
            if self.got.is_empty() {
                bail!("Didn't allocate enough space in GOT");
            }

            let mut needs_relocation = relocation_writer.is_active;
            let address = match res.kind {
                TargetResolutionKind::GotTlsDouble => {
                    let mod_got_entry = slice_take_prefix_mut(&mut self.got, 1);
                    mod_got_entry.copy_from_slice(&[elf::CURRENT_EXE_TLS_MOD]);
                    let offset_entry = slice_take_prefix_mut(&mut self.got, 1);
                    // Convert the address to an offset relative to the TCB which is the end of the TLS
                    // segment.
                    offset_entry[0] = res.address.wrapping_sub(self.tls.end);
                    return Ok(());
                }
                TargetResolutionKind::GotTlsOffset => {
                    needs_relocation = false;
                    // Convert the address to an offset relative to the TCB which is the end of the TLS
                    // segment.
                    if !self.tls.contains(&res.address) {
                        bail!(
                            "GotTlsOffset resolves to address not in TLS segment 0x{:x}",
                            res.address
                        );
                    }
                    res.address.wrapping_sub(self.tls.end)
                }
                TargetResolutionKind::IFunc => {
                    needs_relocation = false;
                    0
                }
                _ => res.address,
            };
            let got_entry = slice_take_prefix_mut(&mut self.got, 1);
            if needs_relocation {
                relocation_writer.write_relocation(got_address.get(), address)?;
            } else {
                got_entry[0] = address;
            }
            if let Some(plt_address) = res.plt_address {
                if self.plt.is_empty() {
                    bail!("Didn't allocate enough space in PLT");
                }
                let plt_entry = slice_take_prefix_mut(&mut self.plt, elf::PLT_ENTRY_SIZE as usize);
                plt_entry.copy_from_slice(PLT_ENTRY_TEMPLATE);
                let offset: i32 = ((got_address.get().wrapping_sub(plt_address.get() + 0xb))
                    as i64)
                    .try_into()
                    .map_err(|_| anyhow!("PLT is more than 2GB away from GOT"))?;
                plt_entry[7..11].copy_from_slice(&offset.to_le_bytes());
            }
        }
        Ok(())
    }

    /// Checks that we used all of the GOT/PLT entries that we requested during layout.
    fn validate_empty(&self) -> Result {
        if !self.got.is_empty() || !self.plt.is_empty() {
            bail!(
                "Unused PLT/GOT entries remain: GOT={}, PLT={}",
                self.got.len() as u64 / elf::GOT_ENTRY_SIZE,
                self.plt.len() as u64 / elf::PLT_ENTRY_SIZE
            );
        }
        Ok(())
    }

    fn apply_relocation(&mut self, rel: &crate::layout::PltRelocation) -> Result {
        let out = slice_take_prefix_mut(&mut self.rela_plt, 1);
        let out = &mut out[0];
        out.addend = rel.resolver;
        out.address = rel.got_address;
        out.info = elf::RelocationType::IRelative as u32 as u64;
        Ok(())
    }
}

struct SymbolTableWriter<'data, 'out> {
    string_offset: u32,
    local_entries: &'out mut [SymtabEntry],
    global_entries: &'out mut [SymtabEntry],
    strings: &'out mut [u8],
    output_sections: &'data OutputSections<'data>,
}

impl<'data, 'out> SymbolTableWriter<'data, 'out> {
    fn new(
        start_string_offset: u32,
        buffers: &mut OutputSectionPartMap<&'out mut [u8]>,
        sizes: &OutputSectionPartMap<u64>,
        output_sections: &'data OutputSections<'data>,
    ) -> Self {
        let local_entries = bytemuck::cast_slice_mut(slice_take_prefix_mut(
            &mut buffers.symtab_locals,
            sizes.symtab_locals as usize,
        ));
        let global_entries = bytemuck::cast_slice_mut(slice_take_prefix_mut(
            &mut buffers.symtab_globals,
            sizes.symtab_globals as usize,
        ));
        let strings = bytemuck::cast_slice_mut(slice_take_prefix_mut(
            &mut buffers.symtab_strings,
            sizes.symtab_strings as usize,
        ));
        Self {
            string_offset: start_string_offset,
            local_entries,
            global_entries,
            strings,
            output_sections,
        }
    }

    fn copy_symbol(
        &mut self,
        sym: &crate::elf::Symbol,
        output_section_id: OutputSectionId,
        section_address: u64,
    ) -> Result {
        let name = sym.name_bytes()?;
        if !crate::layout::should_copy_symbol(name) {
            return Ok(());
        }
        let is_local = sym.is_local();
        let object::SymbolFlags::Elf { st_info, st_other } = sym.flags() else {
            unreachable!()
        };
        let shndx = self
            .output_sections
            .output_index_of_section(output_section_id)
            .context(
                "internal error: tried to copy symbol that in a section that's not being output",
            )?;
        let value = section_address + sym.address();
        let size = sym.size();
        let entry = self.define_symbol(is_local, shndx, value, size, name)?;
        entry.info = st_info;
        entry.other = st_other;
        Ok(())
    }

    fn define_symbol(
        &mut self,
        is_local: bool,
        shndx: u16,
        value: u64,
        size: u64,
        name: &[u8],
    ) -> Result<&mut SymtabEntry> {
        let entry = if is_local {
            slice_take_prefix_mut(&mut self.local_entries, 1)
        } else {
            slice_take_prefix_mut(&mut self.global_entries, 1)
        };
        entry[0] = SymtabEntry {
            name: self.string_offset,
            info: 0,
            other: 0,
            shndx,
            value,
            size,
        };
        let len = name.len();
        let str_out = slice_take_prefix_mut(&mut self.strings, len + 1);
        str_out[..len].copy_from_slice(name);
        str_out[len] = 0;
        self.string_offset += len as u32 + 1;
        Ok(&mut entry[0])
    }

    /// Verifies that we've used up all the space allocated to this writer. i.e. checks that we
    /// didn't allocate too much or missed writing something that we were supposed to write.
    fn check_exhausted(&self) -> Result {
        if !self.local_entries.is_empty()
            || !self.global_entries.is_empty()
            || !self.strings.is_empty()
        {
            bail!(
                "Didn't use up all allocated symtab/strtab space. local={} global={} strings={}",
                self.local_entries.len(),
                self.global_entries.len(),
                self.strings.len()
            );
        }
        Ok(())
    }
}

impl<'data> ObjectLayout<'data> {
    fn write(&self, mut buffers: OutputSectionPartMap<&mut [u8]>, layout: &Layout) -> Result {
        let start_str_offset = self.strings_offset_start;
        let mut plt_got_writer = PltGotWriter::new(layout, &mut buffers);
        let mut relocation_writer =
            RelocationWriter::new(layout.args().is_relocatable(), &mut buffers);
        for sec in &self.sections {
            match sec {
                SectionSlot::Loaded(sec) => self.write_section(
                    layout,
                    sec,
                    &mut buffers,
                    &mut plt_got_writer,
                    &mut relocation_writer,
                )?,
                SectionSlot::EhFrameData(section_index) => {
                    self.write_eh_frame_data(
                        *section_index,
                        &mut buffers,
                        layout,
                        &mut relocation_writer,
                    )?;
                }
                _ => (),
            }
        }
        for rel in &self.plt_relocations {
            plt_got_writer.apply_relocation(rel)?;
        }
        for symbol_id in &self.loaded_symbols {
            plt_got_writer
                .process_symbol(*symbol_id, &mut relocation_writer)
                .with_context(|| {
                    format!(
                        "Failed to process symbol `{}`",
                        layout.symbol_db.symbol_name(*symbol_id)
                    )
                })?;
        }
        if !layout.args().strip_all {
            self.write_symbols(start_str_offset, buffers, &layout.output_sections, layout)?;
        }
        plt_got_writer.validate_empty()?;
        relocation_writer.validate_empty()?;
        Ok(())
    }

    fn write_section(
        &self,
        layout: &Layout<'_>,
        sec: &Section<'_>,
        buffers: &mut OutputSectionPartMap<&mut [u8]>,
        plt_got_writer: &mut PltGotWriter<'_, '_>,
        relocation_writer: &mut RelocationWriter,
    ) -> Result<(), anyhow::Error> {
        if layout
            .output_sections
            .has_data_in_file(sec.output_section_id.unwrap())
        {
            let section_buffer = buffers.regular_mut(sec.output_section_id.unwrap(), sec.alignment);
            let allocation_size = sec.capacity() as usize;
            if section_buffer.len() < allocation_size {
                bail!(
                    "Insufficient space allocated to section {}. Tried to take {} bytes, but only {} remain",
                    self.display_section_name(sec.index),
                    allocation_size, section_buffer.len()
                );
            }
            let out = slice_take_prefix_mut(section_buffer, allocation_size);
            // Cut off any padding so that our output buffer is the size of our input buffer.
            let out = &mut out[..sec.data.len()];
            out.copy_from_slice(sec.data);
            self.apply_relocations(out, sec, layout, relocation_writer)
                .with_context(|| {
                    format!(
                        "Failed to apply relocations in section {} of {}",
                        self.display_section_name(sec.index),
                        self.input
                    )
                })?;
        }
        if sec.resolution_kind.needs_got_entry() {
            let res = self.section_resolutions[sec.index.0]
                .as_ref()
                .ok_or_else(|| anyhow!("Section requires GOT, but hasn't been resolved"))?;
            plt_got_writer.process_resolution(res, relocation_writer)?;
        };
        Ok(())
    }

    fn write_symbols(
        &self,
        start_str_offset: u32,
        mut buffers: OutputSectionPartMap<&mut [u8]>,
        sections: &OutputSections,
        layout: &Layout,
    ) -> Result {
        let mut symbol_writer =
            SymbolTableWriter::new(start_str_offset, &mut buffers, &self.mem_sizes, sections);
        for sym in self.object.symbols() {
            match object::ObjectSymbol::section(&sym) {
                object::SymbolSection::Section(section_index) => {
                    if let SectionSlot::Loaded(section) = &self.sections[section_index.0] {
                        let output_section_id = section.output_section_id.unwrap();
                        symbol_writer.copy_symbol(
                            &sym,
                            output_section_id,
                            self.section_resolutions[section_index.0]
                                .as_ref()
                                .unwrap()
                                .address,
                        )?;
                    }
                }
                object::SymbolSection::Common => {
                    if let Some(symbol_id) = self.global_id_for_symbol(&sym) {
                        let symbol = layout.symbol_db.symbol(symbol_id);
                        if symbol.file_id == self.file_id {
                            if let Some(SymbolResolution::Resolved(res)) =
                                layout.global_symbol_resolution(symbol_id)
                            {
                                symbol_writer.copy_symbol(
                                    &sym,
                                    output_section_id::BSS,
                                    res.address,
                                )?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        symbol_writer.check_exhausted()?;
        Ok(())
    }

    fn apply_relocations(
        &self,
        out: &mut [u8],
        section: &Section,
        layout: &Layout,
        relocation_writer: &mut RelocationWriter,
    ) -> Result {
        let section_address = self.section_resolutions[section.index.0]
            .as_ref()
            .unwrap()
            .address;
        let elf_section = &self.object.section_by_index(section.index)?;
        let mut modifier = RelocationModifier::Normal;
        for (offset_in_section, rel) in elf_section.relocations() {
            if modifier == RelocationModifier::SkipNextRelocation {
                modifier = RelocationModifier::Normal;
                continue;
            }
            if let Some(resolution) = self.get_resolution(&rel, layout)? {
                modifier = apply_relocation(
                    &resolution,
                    offset_in_section,
                    &rel,
                    section_address,
                    layout,
                    out,
                    relocation_writer,
                )
                .with_context(|| {
                    format!("Failed to apply {}", self.display_relocation(&rel, layout))
                })?;
            }
        }
        Ok(())
    }

    fn write_eh_frame_data(
        &self,
        eh_frame_section_index: object::SectionIndex,
        buffers: &mut OutputSectionPartMap<&mut [u8]>,
        layout: &Layout,
        relocation_writer: &mut RelocationWriter,
    ) -> Result {
        let output_data = &mut buffers.eh_frame[..];
        let headers_out: &mut [EhFrameHdrEntry] =
            bytemuck::cast_slice_mut(&mut buffers.eh_frame_hdr[..]);
        let mut header_offset = 0;
        let eh_frame_section = self.object.section_by_index(eh_frame_section_index)?;
        let data = eh_frame_section.data()?;
        const PREFIX_LEN: usize = core::mem::size_of::<elf::EhFrameEntryPrefix>();
        let mut relocations = eh_frame_section.relocations().peekable();
        let mut input_pos = 0;
        let mut output_pos = 0;
        let frame_info_ptr_base = self.eh_frame_start_address;
        let eh_frame_hdr_address = layout.mem_address_of_built_in(output_section_id::EH_FRAME_HDR);

        // Map from input offset to output offset of each CIE.
        let mut cies_offset_conversion: AHashMap<u32, u32> = AHashMap::new();

        while input_pos + PREFIX_LEN <= data.len() {
            let prefix: elf::EhFrameEntryPrefix =
                bytemuck::pod_read_unaligned(&data[input_pos..input_pos + PREFIX_LEN]);
            let size = core::mem::size_of_val(&prefix.length) + prefix.length as usize;
            let next_input_pos = input_pos + size;
            let next_output_pos = output_pos + size;
            if next_input_pos > data.len() {
                bail!("Invalid .eh_frame data");
            }
            let mut should_keep = false;
            let mut output_cie_offset = None;
            if prefix.cie_id == 0 {
                // This is a CIE
                cies_offset_conversion.insert(input_pos as u32, output_pos as u32);
                should_keep = true;
            } else {
                // This is an FDE
                if let Some((rel_offset, rel)) = relocations.peek() {
                    if *rel_offset < next_input_pos as u64 {
                        let is_pc_begin =
                            (*rel_offset as usize - input_pos) == elf::FDE_PC_BEGIN_OFFSET;

                        if is_pc_begin {
                            let section_index;
                            let offset_in_section;
                            match rel.target() {
                                object::RelocationTarget::Symbol(index) => {
                                    let elf_symbol = &self.object.symbol_by_index(index)?;
                                    if let Some(index) = elf_symbol.section_index() {
                                        section_index = index;
                                        offset_in_section = elf_symbol.address();
                                    } else {
                                        bail!(".eh_frame pc-begin refers to symbol that's not defined in file");
                                    }
                                }
                                object::RelocationTarget::Section(index) => {
                                    section_index = index;
                                    offset_in_section = 0;
                                }
                                _ => bail!("Unexpected relocation type in .eh_frame pc-begin"),
                            };
                            if let Some(section_resolution) =
                                &self.section_resolutions[section_index.0]
                            {
                                should_keep = true;
                                let cie_pointer_pos = input_pos as u32 + 4;
                                let input_cie_pos = cie_pointer_pos
                                    .checked_sub(prefix.cie_id)
                                    .with_context(|| {
                                        format!(
                                            "CIE pointer is {}, but we're at offset {}",
                                            prefix.cie_id, cie_pointer_pos
                                        )
                                    })?;
                                let frame_ptr = (section_resolution.address + offset_in_section)
                                    as i64
                                    - eh_frame_hdr_address as i64;
                                headers_out[header_offset] = EhFrameHdrEntry {
                                    frame_ptr: i32::try_from(frame_ptr)
                                        .context("32 bit overflow in frame_ptr")?,
                                    frame_info_ptr: i32::try_from(
                                        frame_info_ptr_base + output_pos as u64,
                                    )
                                    .context("32 bit overflow when computing frame_info_ptr")?,
                                };
                                header_offset += 1;
                                // TODO: Experiment with skipping this lookup if the `input_cie_pos`
                                // is the same as the previous entry.
                                let output_cie_pos = cies_offset_conversion.get(&input_cie_pos).with_context(|| format!("FDE referenced CIE at {input_cie_pos}, but no CIE at that position"))?;
                                output_cie_offset = Some(output_pos as u32 + 4 - *output_cie_pos);
                            }
                        }
                    }
                }
            }
            if should_keep {
                if next_output_pos > output_data.len() {
                    bail!("Insufficient allocation to .eh_frame section. Allocated 0x{:x}, but tried to write up to 0x{:x}",
                        self.mem_sizes.eh_frame, next_output_pos);
                }
                let entry_out = &mut output_data[output_pos..next_output_pos];
                entry_out.copy_from_slice(&data[input_pos..next_input_pos]);
                if let Some(output_cie_offset) = output_cie_offset {
                    entry_out[4..8].copy_from_slice(&output_cie_offset.to_le_bytes());
                }
                while let Some((rel_offset, rel)) = relocations.peek() {
                    if *rel_offset >= next_input_pos as u64 {
                        // This relocation belongs to the next entry.
                        break;
                    }
                    if let Some(resolution) = self.get_resolution(rel, layout)? {
                        apply_relocation(
                            &resolution,
                            rel_offset - input_pos as u64,
                            rel,
                            output_pos as u64 + self.eh_frame_start_address,
                            layout,
                            entry_out,
                            relocation_writer,
                        )
                        .with_context(|| {
                            format!("Failed to apply {}", self.display_relocation(rel, layout))
                        })?;
                    }
                    relocations.next();
                }
                output_pos = next_output_pos;
            } else {
                // We're ignoring this entry, skip any relocations for it.
                while let Some((rel_offset, _rel)) = relocations.peek() {
                    if *rel_offset < next_input_pos as u64 {
                        relocations.next();
                    } else {
                        break;
                    }
                }
            }
            input_pos = next_input_pos;
        }

        // Copy any remaining bytes in .eh_frame that aren't large enough to constitute an actual
        // entry. crtend.o has a single u32 equal to 0 as an end marker.
        let remaining = data.len() - input_pos;
        if remaining > 0 {
            output_data[output_pos..output_pos + remaining]
                .copy_from_slice(&data[input_pos..input_pos + remaining]);
        }

        Ok(())
    }

    fn display_relocation<'a>(
        &'a self,
        rel: &'a object::Relocation,
        layout: &'a Layout,
    ) -> DisplayRelocation<'a> {
        DisplayRelocation {
            rel,
            symbol_db: layout.symbol_db,
            object: self,
        }
    }

    fn get_resolution<'a>(
        &'a self,
        rel: &object::Relocation,
        layout: &'a Layout,
    ) -> Result<Option<Resolution>> {
        let resolution = match rel.target() {
            object::RelocationTarget::Symbol(local_symbol_id) => {
                match self.local_symbol_resolutions[local_symbol_id.0] {
                    LocalSymbolResolution::Global(symbol_id) => {
                        match layout.global_symbol_resolution(symbol_id) {
                            Some(SymbolResolution::Resolved(resolution)) => *resolution,
                            Some(SymbolResolution::Dynamic) => todo!(),
                            None => {
                                bail!(
                                    "Missing resolution for non-weak symbol {}",
                                    layout.symbol_db.symbol_name(symbol_id)
                                )
                            }
                        }
                    }
                    LocalSymbolResolution::WeakRefToGlobal(symbol_id) => {
                        match layout.global_symbol_resolution(symbol_id) {
                            Some(SymbolResolution::Resolved(resolution)) => *resolution,
                            Some(SymbolResolution::Dynamic) => todo!(),
                            None => layout.internal().undefined_symbol_resolution,
                        }
                    }
                    LocalSymbolResolution::LocalSection(local_index) => {
                        let mut r = self.section_resolutions[local_index.0].with_context(|| {
                            format!(
                                "Reference to section that hasn't been resolved {}",
                                self.display_section_name(local_index)
                            )
                        })?;
                        let local_sym = self.object.symbol_by_index(local_symbol_id)?;
                        r.address += local_sym.address();
                        r
                    }
                    LocalSymbolResolution::UnresolvedWeak => {
                        layout.internal().undefined_symbol_resolution
                    }
                    LocalSymbolResolution::TlsGetAddr => return Ok(None),
                    LocalSymbolResolution::UndefinedSymbol => {
                        let name = self.object.symbol_by_index(local_symbol_id)?.name_bytes()?;
                        bail!(
                            "Reference to undefined symbol `{}`",
                            String::from_utf8_lossy(name),
                        );
                    }
                    LocalSymbolResolution::Null => bail!("Reference to null symbol"),
                    LocalSymbolResolution::MergedString(res) => {
                        if let Some(symbol_id) = res.symbol_id {
                            match layout.global_symbol_resolution(symbol_id) {
                                Some(SymbolResolution::Resolved(resolution)) => *resolution,
                                Some(SymbolResolution::Dynamic) => todo!(),
                                None => {
                                    bail!(
                                        "Missing resolution for global string-merge symbol {}",
                                        layout.symbol_db.symbol_name(symbol_id)
                                    )
                                }
                            }
                        } else {
                            Resolution {
                                address: layout.merged_string_start_addresses.resolve(res),
                                got_address: None,
                                plt_address: None,
                                kind: TargetResolutionKind::Address,
                            }
                        }
                    }
                }
            }
            object::RelocationTarget::Section(local_index) => {
                self.section_resolutions[local_index.0].unwrap()
            }
            other => bail!("Unsupported relocation {other:?}"),
        };
        Ok(Some(resolution))
    }

    fn display_section_name(&self, section_index: object::SectionIndex) -> String {
        if let Ok(section) = self.object.section_by_index(section_index) {
            if let Ok(name) = section.name() {
                return format!("`{name}`");
            }
        }
        "(failed to get section name)".to_owned()
    }
}

struct DisplayRelocation<'a> {
    rel: &'a object::Relocation,
    symbol_db: &'a SymbolDb<'a>,
    object: &'a ObjectLayout<'a>,
}

impl<'a> Display for DisplayRelocation<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "relocation of type ")?;
        match self.rel.kind() {
            object::RelocationKind::Unknown => write!(f, "{:?}", self.rel.flags())?,
            kind => write!(f, "{kind:?}")?,
        }
        write!(f, " to ")?;
        match self.rel.target() {
            object::RelocationTarget::Symbol(local_symbol_id) => {
                match &self.object.local_symbol_resolutions[local_symbol_id.0] {
                    LocalSymbolResolution::Global(symbol_id) => {
                        write!(f, "global `{}`", self.symbol_db.symbol_name(*symbol_id))?;
                    }
                    LocalSymbolResolution::UnresolvedWeak => write!(
                        f,
                        "unresolved weak symbol `{}`",
                        self.object
                            .object
                            .symbol_by_index(local_symbol_id)
                            .and_then(|s| s.name())
                            .unwrap_or("??")
                    )?,
                    LocalSymbolResolution::TlsGetAddr => write!(f, "TlsGetAddr")?,
                    LocalSymbolResolution::WeakRefToGlobal(symbol_id) => {
                        write!(
                            f,
                            "weak ref to global `{}`",
                            self.symbol_db.symbol_name(*symbol_id)
                        )?;
                    }
                    LocalSymbolResolution::LocalSection(section_index) => {
                        write!(
                            f,
                            "section `{}`",
                            self.object
                                .object
                                .section_by_index(*section_index)
                                .and_then(|sec| sec.name())
                                .unwrap_or("??")
                        )?;
                    }
                    LocalSymbolResolution::UndefinedSymbol => writeln!(f, "undefined section")?,
                    LocalSymbolResolution::Null => writeln!(f, "null symbol")?,
                    LocalSymbolResolution::MergedString(res) => write!(
                        f,
                        "Merged string in section {} at offset {}",
                        res.output_section_id, res.offset
                    )?,
                }
            }
            object::RelocationTarget::Section(section_index) => write!(
                f,
                "section `{}`",
                self.object
                    .object
                    .section_by_index(section_index)
                    .and_then(|s| s.name())
                    .unwrap_or("??")
            )?,
            object::RelocationTarget::Absolute => write!(f, "absolute")?,
            _ => write!(f, "unknown")?,
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RelocationModifier {
    Normal,
    SkipNextRelocation,
}

struct RelocationWriter<'out> {
    /// Whether we're writing relocations. This will be false if we're writing a non-relocatable
    /// output file.
    is_active: bool,
    rela_dyn: &'out mut [crate::elf::Rela],
}

impl<'out> RelocationWriter<'out> {
    fn new(is_active: bool, buffers: &mut OutputSectionPartMap<&'out mut [u8]>) -> Self {
        Self {
            is_active,
            rela_dyn: bytemuck::cast_slice_mut(core::mem::take(&mut buffers.rela_dyn)),
        }
    }

    fn write_relocation(&mut self, place: u64, address: u64) -> Result {
        if !self.is_active {
            return Ok(());
        }
        let rela = crate::slice::take_first_mut(&mut self.rela_dyn)
            .context("insufficient allocation to .rela.dyn")?;
        rela.address = place;
        rela.addend = address;
        rela.info = elf::rel::R_X86_64_RELATIVE.into();
        Ok(())
    }

    fn disabled() -> Self {
        Self {
            is_active: false,
            rela_dyn: Default::default(),
        }
    }

    fn validate_empty(&self) -> Result {
        if self.rela_dyn.is_empty() {
            return Ok(());
        }
        bail!(
            "Allocated too much space in .rela.dyn. {} unused entries remain.",
            self.rela_dyn.len()
        );
    }
}

/// Applies the relocation `rel` at `offset_in_section`, where the section bytes are `out`. See "ELF
/// Handling For Thread-Local Storage" for details about some of the TLS-related relocations and
/// transformations that are applied.
fn apply_relocation(
    resolution: &Resolution,
    offset_in_section: u64,
    rel: &object::Relocation,
    section_address: u64,
    layout: &Layout,
    out: &mut [u8],
    relocation_writer: &mut RelocationWriter,
) -> Result<RelocationModifier> {
    let address = resolution.address;
    let mut offset = offset_in_section as usize;
    let place = section_address + offset_in_section;
    let mut addend = rel.addend() as u64;
    let mut next_modifier = RelocationModifier::Normal;
    let object::RelocationFlags::Elf { mut r_type } = rel.flags() else {
        unreachable!();
    };
    if let Some(relaxation) = Relaxation::new(r_type, out, offset_in_section as usize) {
        let value_is_relocatable = address != 0 && layout.args().is_relocatable();
        r_type = relaxation.new_relocation_kind(value_is_relocatable);
        relaxation.apply(out, offset_in_section as usize, value_is_relocatable);
        if !value_is_relocatable {
            addend = 0;
        }
    }
    let rel_info = RelocationKindInfo::from_raw(r_type)?;
    debug_assert!(rel.size() == 0 || rel.size() as usize / 8 == rel_info.byte_size);
    let value = match rel_info.kind {
        RelocationKind::Absolute => {
            if relocation_writer.is_active && address != 0 {
                relocation_writer.write_relocation(place, address)?;
                0
            } else {
                address.wrapping_add(addend)
            }
        }
        RelocationKind::Relative => address.wrapping_add(addend).wrapping_sub(place),
        RelocationKind::GotRelative => resolution
            .got_address()?
            .wrapping_add(addend)
            .wrapping_sub(place),
        RelocationKind::PltRelative => {
            if layout.args().link_static {
                resolution.address.wrapping_add(addend).wrapping_sub(place)
            } else {
                resolution
                    .plt_address()?
                    .wrapping_add(addend)
                    .wrapping_sub(place)
            }
        }
        RelocationKind::TlsGd => {
            // TODO: Move this logic, or something equivalent into the relaxation module.
            match layout.args().tls_mode() {
                TlsMode::LocalExec => {
                    // Transform GD (general dynamic) into LE (local exec). We can make this
                    // transformation because we're producing a statically linked executable.
                    expect_bytes_before_offset(out, offset, &[0x66, 0x48, 0x8d, 0x3d])?;
                    // Transforms to:
                    // mov %fs:0x0,%rax // the same as a TLSLD relocation
                    // lea {var offset}(%rax),%rax
                    out[offset - 4..offset + 8].copy_from_slice(&[
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0x48, 0x8d, 0x80,
                    ]);
                    offset += 8;
                    next_modifier = RelocationModifier::SkipNextRelocation;
                    address.wrapping_sub(layout.tls_end_address())
                }
                TlsMode::Preserve => resolution
                    .got_address()?
                    .wrapping_add(addend)
                    .wrapping_sub(place),
            }
        }
        RelocationKind::TlsLd => {
            match layout.args().tls_mode() {
                TlsMode::LocalExec => {
                    // Transform LD (local dynamic) into LE (local exec). We can make this
                    // transformation because we're producing a statically linked executable.
                    expect_bytes_before_offset(out, offset, &[0x48, 0x8d, 0x3d])?;
                    // Transforms to: mov %fs:0x0,%rax
                    out[offset - 3..offset + 5]
                        .copy_from_slice(&[0x66, 0x66, 0x66, 0x64, 0x48, 0x8b, 0x04, 0x25]);
                    offset += 5;
                    next_modifier = RelocationModifier::SkipNextRelocation;
                    0
                }
                TlsMode::Preserve => layout
                    .internal()
                    .tlsld_got_entry
                    .unwrap()
                    .get()
                    .wrapping_add(addend)
                    .wrapping_sub(place),
            }
        }
        RelocationKind::DtpOff => {
            if layout.args().link_static {
                address
                    .wrapping_sub(layout.tls_end_address())
                    .wrapping_add(addend)
            } else {
                todo!()
            }
        }
        RelocationKind::GotTpOff => resolution
            .got_address()?
            .wrapping_add(addend)
            .wrapping_sub(place),
        RelocationKind::TpOff => address.wrapping_sub(layout.tls_end_address()),
        other => bail!("Unsupported relocation kind {other:?}"),
    };
    let value_bytes = value.to_le_bytes();
    let end = offset + rel_info.byte_size;
    if out.len() < end {
        bail!("Relocation outside of bounds of section");
    }
    out[offset..end].copy_from_slice(&value_bytes[..rel_info.byte_size]);
    Ok(next_modifier)
}

/// Verifies that the bytes leading up to `offset` are equal to `expected`. Return an error if not.
fn expect_bytes_before_offset(bytes: &[u8], offset: usize, expected: &[u8]) -> Result {
    if offset < expected.len() {
        bail!("Expected bytes {expected:x?}, but only had {offset} bytes available");
    }
    let actual = &bytes[offset - expected.len()..offset];
    if actual != expected {
        bail!("Expected bytes {expected:x?}, got {actual:x?}");
    }
    Ok(())
}

impl<'data> InternalLayout<'data> {
    fn write(&self, mut buffers: OutputSectionPartMap<&mut [u8]>, layout: &Layout) -> Result {
        let (file_header_bytes, rest) = buffers
            .file_headers
            .split_at_mut(usize::from(elf::FILE_HEADER_SIZE));
        let header: &mut FileHeader = bytemuck::from_bytes_mut(file_header_bytes);
        *header = FileHeader::build(layout, &self.header_info)?;

        let (program_headers_bytes, rest) =
            rest.split_at_mut(self.header_info.program_headers_size() as usize);
        let mut program_headers = ProgramHeaderWriter::new(program_headers_bytes);
        write_program_headers(&mut program_headers, layout)?;

        let (section_headers_bytes, _rest) =
            rest.split_at_mut(self.header_info.section_headers_size() as usize);
        write_section_headers(section_headers_bytes, layout);

        write_section_header_strings(buffers.shstrtab, &layout.output_sections);

        let mut relocation_writer =
            RelocationWriter::new(layout.args().is_relocatable(), &mut buffers);

        self.write_plt_got_entries(&mut buffers, layout, &mut relocation_writer)?;

        if !layout.args().strip_all {
            self.write_symbol_table_entries(&mut buffers, layout)?;
        }

        write_eh_frame_hdr(&mut buffers, layout)?;

        self.write_merged_strings(&mut buffers);

        if layout.args().pie {
            self.write_dynamic_entries(buffers.dynamic, layout)?;
        }

        relocation_writer.validate_empty()?;

        Ok(())
    }

    fn write_merged_strings(&self, buffers: &mut OutputSectionPartMap<&mut [u8]>) {
        self.merged_strings.for_each(|section_id, merged| {
            if merged.len > 0 {
                let buffer = buffers.regular_mut(section_id, crate::alignment::MIN);
                for string in &merged.strings {
                    let dest = crate::slice::slice_take_prefix_mut(buffer, string.len());
                    dest.copy_from_slice(string)
                }
            }
        });

        // Write linker identity into .comment section.
        let comment_buffer = buffers.regular_mut(output_section_id::COMMENT, crate::alignment::MIN);
        crate::slice::slice_take_prefix_mut(comment_buffer, self.identity.len())
            .copy_from_slice(self.identity.as_bytes());
    }

    fn write_plt_got_entries(
        &self,
        buffers: &mut OutputSectionPartMap<&mut [u8]>,
        layout: &Layout,
        relocation_writer: &mut RelocationWriter,
    ) -> Result {
        let mut plt_got_writer = PltGotWriter::new(layout, buffers);

        // Our PLT entry for an undefined symbol doesn't really exist, so don't try to write an
        // actual PLT entry for it.
        let undefined_symbol_resolution = Resolution {
            plt_address: None,
            ..self.undefined_symbol_resolution
        };
        plt_got_writer
            .process_resolution(
                &undefined_symbol_resolution,
                &mut RelocationWriter::disabled(),
            )
            .context("undefined symbol resolution")?;
        if let Some(got_address) = self.tlsld_got_entry {
            plt_got_writer.process_resolution(
                &Resolution {
                    address: 1,
                    got_address: Some(got_address),
                    plt_address: None,
                    kind: TargetResolutionKind::Got,
                },
                &mut RelocationWriter::disabled(),
            )?;
            plt_got_writer.process_resolution(
                &Resolution {
                    address: 0,
                    got_address: Some(got_address.saturating_add(elf::GOT_ENTRY_SIZE)),
                    plt_address: None,
                    kind: TargetResolutionKind::Got,
                },
                &mut RelocationWriter::disabled(),
            )?;
        }

        for &symbol_id in &self.defined {
            plt_got_writer
                .process_symbol(symbol_id, relocation_writer)
                .with_context(|| {
                    format!(
                        "Failed to process symbol `{}`",
                        layout.symbol_db.symbol_name(symbol_id)
                    )
                })?;
        }
        plt_got_writer.validate_empty()?;
        Ok(())
    }

    fn write_symbol_table_entries(
        &self,
        buffers: &mut OutputSectionPartMap<&mut [u8]>,
        layout: &Layout,
    ) -> Result {
        let mut symbol_writer = SymbolTableWriter::new(
            self.strings_offset_start,
            buffers,
            &self.mem_sizes,
            &layout.output_sections,
        );

        // Define symbol 0. This needs to be a null placeholder.
        symbol_writer.define_symbol(true, 0, 0, 0, &[])?;

        for &symbol_id in &self.defined {
            let Some(resolution) = layout.global_symbol_resolution(symbol_id) else {
                continue;
            };
            let symbol = layout.symbol_db.symbol(symbol_id);
            let local_index = symbol.local_index_for_file(INTERNAL_FILE_ID)?;
            let def_info = &self.symbol_definitions[local_index.0];
            let section_id = def_info.section_id();

            // We don't emit a section header for our headers section, so don't emit symbols that
            // are in that section, otherwise they'll show up as undefined.
            if section_id == output_section_id::HEADERS {
                continue;
            }

            let shndx = layout
                .output_sections
                .output_index_of_section(section_id)
                .with_context(|| {
                    format!(
                        "symbol `{}` in section `{}` that we're not going to output",
                        layout.symbol_db.symbol_name(symbol_id),
                        String::from_utf8_lossy(layout.output_sections.details(section_id).name)
                    )
                })?;
            let address = match resolution {
                SymbolResolution::Resolved(res) => res.address,
                SymbolResolution::Dynamic => unreachable!(),
            };
            let symbol_name = layout.symbol_db.symbol_name(symbol_id);
            let entry =
                symbol_writer.define_symbol(false, shndx, address, 0, symbol_name.bytes())?;
            entry.info = (elf::Binding::Global as u8) << 4;
        }
        symbol_writer.check_exhausted()?;
        Ok(())
    }

    fn write_dynamic_entries(&self, out: &mut [u8], layout: &Layout) -> Result {
        let mut entries: &mut [DynamicEntry] = bytemuck::cast_slice_mut(out);
        assert_eq!(entries.len(), NUM_DYNAMIC_ENTRIES);
        // When adding/removing entries, don't forget to update NUM_DYNAMIC_ENTRIES
        write_dynamic_entry(
            &mut entries,
            DynamicTag::Init,
            layout.offset_of_section(output_section_id::INIT),
        )?;
        write_dynamic_entry(
            &mut entries,
            DynamicTag::Fini,
            layout.offset_of_section(output_section_id::FINI),
        )?;

        write_dynamic_entry(
            &mut entries,
            DynamicTag::InitArray,
            layout.offset_of_section(output_section_id::INIT_ARRAY),
        )?;
        write_dynamic_entry(
            &mut entries,
            DynamicTag::InitArraySize,
            layout.size_of_section(output_section_id::INIT_ARRAY),
        )?;

        write_dynamic_entry(
            &mut entries,
            DynamicTag::FiniArray,
            layout.offset_of_section(output_section_id::FINI_ARRAY),
        )?;
        write_dynamic_entry(
            &mut entries,
            DynamicTag::FiniArraySize,
            layout.size_of_section(output_section_id::FINI_ARRAY),
        )?;

        write_dynamic_entry(
            &mut entries,
            DynamicTag::StrTab,
            layout.offset_of_section(output_section_id::DYNSTR),
        )?;
        write_dynamic_entry(
            &mut entries,
            DynamicTag::StrSize,
            layout.size_of_section(output_section_id::DYNSTR),
        )?;

        write_dynamic_entry(
            &mut entries,
            DynamicTag::SymTab,
            layout.offset_of_section(output_section_id::DYNSYM),
        )?;
        write_dynamic_entry(
            &mut entries,
            DynamicTag::SymEnt,
            core::mem::size_of::<elf::SymtabEntry>() as u64,
        )?;

        write_dynamic_entry(&mut entries, DynamicTag::Debug, 0)?;

        write_dynamic_entry(
            &mut entries,
            DynamicTag::Rela,
            layout.offset_of_section(output_section_id::RELA_DYN),
        )?;
        write_dynamic_entry(
            &mut entries,
            DynamicTag::RelaSize,
            layout.size_of_section(output_section_id::RELA_DYN),
        )?;
        write_dynamic_entry(&mut entries, DynamicTag::RelaEnt, elf::RELA_ENTRY_SIZE)?;
        write_dynamic_entry(
            &mut entries,
            DynamicTag::RelaCount,
            layout.size_of_section(output_section_id::RELA_DYN)
                / core::mem::size_of::<elf::Rela>() as u64,
        )?;

        write_dynamic_entry(&mut entries, DynamicTag::Flags, elf::flags::BIND_NOW)?;
        write_dynamic_entry(
            &mut entries,
            DynamicTag::Flags1,
            elf::flags_1::PIE | elf::flags_1::NOW,
        )?;

        //write_dynamic_entry(&mut entries, DynamicTag::Hash, todo)?;
        //write_dynamic_entry(&mut entries, DynamicTag::StrTab, todo)?;
        // write_dynamic_entry(&mut entries, DynamicTag::Rela, todo)?;
        // write_dynamic_entry(&mut entries, DynamicTag::RelaSize, todo)?;
        // write_dynamic_entry(&mut entries, DynamicTag::RelEnt, todo)?;
        // write_dynamic_entry(&mut entries, DynamicTag::StrSize, todo)?;
        // write_dynamic_entry(&mut entries, DynamicTag::Rel, todo)?;
        // write_dynamic_entry(&mut entries, DynamicTag::RelSize, todo)?;
        write_dynamic_entry(&mut entries, DynamicTag::Null, 0)?;
        Ok(())
    }
}

fn write_eh_frame_hdr(
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &Layout<'_>,
) -> Result {
    let header: &mut EhFrameHdr = bytemuck::from_bytes_mut(buffers.eh_frame_hdr);
    header.version = 1;

    header.table_encoding = elf::ExceptionHeaderFormat::I32 as u8
        | elf::ExceptionHeaderApplication::EhFrameHdrRelative as u8;

    header.frame_pointer_encoding =
        elf::ExceptionHeaderFormat::I32 as u8 | elf::ExceptionHeaderApplication::Relative as u8;
    header.frame_pointer = eh_frame_ptr(layout)?;

    header.count_encoding =
        elf::ExceptionHeaderFormat::U32 as u8 | elf::ExceptionHeaderApplication::Absolute as u8;
    header.entry_count = eh_frame_hdr_entry_count(layout)?;

    Ok(())
}

fn eh_frame_hdr_entry_count(layout: &Layout<'_>) -> Result<u32> {
    let hdr_sec = layout
        .section_layouts
        .built_in(output_section_id::EH_FRAME_HDR);
    u32::try_from(
        (hdr_sec.mem_size - core::mem::size_of::<elf::EhFrameHdr>() as u64)
            / core::mem::size_of::<elf::EhFrameHdrEntry>() as u64,
    )
    .context(".eh_frame_hdr entries overflowed 32 bits")
}

/// Returns the address of .eh_frame relative to the location in .eh_frame_hdr where the frame
/// pointer is stored.
fn eh_frame_ptr(layout: &Layout<'_>) -> Result<i32> {
    let eh_frame_address = layout.mem_address_of_built_in(output_section_id::EH_FRAME);
    let eh_frame_hdr_address = layout.mem_address_of_built_in(output_section_id::EH_FRAME_HDR);
    i32::try_from(
        eh_frame_address - (eh_frame_hdr_address + elf::FRAME_POINTER_FIELD_OFFSET as u64),
    )
    .context(".eh_frame more than 2GB away from .eh_frame_hdr")
}

// TODO: Compute this at runtime by making the that writes the dynamic entries generic over its
// output, then instantiating it with an output that just counts.
pub(crate) const NUM_DYNAMIC_ENTRIES: usize = 18;

fn write_dynamic_entry(out: &mut &mut [DynamicEntry], tag: DynamicTag, value: u64) -> Result {
    let entry = crate::slice::take_first_mut(out)
        .ok_or_else(|| anyhow!("Insufficient dynamic table entries"))?;
    entry.tag = tag as u64;
    entry.value = value;
    Ok(())
}

fn write_section_headers(out: &mut [u8], layout: &Layout) {
    let entries: &mut [SectionHeader] = bytemuck::cast_slice_mut(out);
    let output_sections = &layout.output_sections;
    let mut entries = entries.iter_mut();
    let mut name_offset = 0;
    output_sections.sections_do(|section_id, section_details| {
        let section_layout = layout.section_layouts.get(section_id);
        if output_sections
            .output_index_of_section(section_id)
            .is_none()
        {
            return;
        }
        let entsize = section_details.element_size;
        let size;
        let alignment;
        if section_details.ty == elf::Sht::Null {
            size = 0;
            alignment = 0;
        } else {
            size = section_layout.mem_size;
            alignment = section_layout.alignment.value();
        };
        let mut link = 0;
        if let Some(link_id) = layout.output_sections.link_id(section_id) {
            link = output_sections
                .output_index_of_section(link_id)
                .unwrap_or(0);
        }
        *entries.next().unwrap() = SectionHeader {
            name: name_offset,
            ty: section_details.ty as u32,
            flags: section_details.section_flags,
            address: section_layout.mem_offset,
            offset: section_layout.file_offset as u64,
            size,
            link: link.into(),
            info: section_id.info(layout),
            alignment,
            entsize,
        };
        name_offset += layout.output_sections.name(section_id).len() as u32 + 1;
    });
    assert!(
        entries.next().is_none(),
        "Allocated section entries that weren't used"
    );
}

fn write_section_header_strings(mut out: &mut [u8], sections: &OutputSections) {
    sections.sections_do(|id, _details| {
        if sections.output_index_of_section(id).is_some() {
            let name = sections.name(id);
            let name_out = crate::slice::slice_take_prefix_mut(&mut out, name.len() + 1);
            name_out[..name.len()].copy_from_slice(name);
            name_out[name.len()] = 0;
        }
    });
}

struct ProgramHeaderWriter<'out> {
    headers: &'out mut [ProgramHeader],
}

impl<'out> ProgramHeaderWriter<'out> {
    fn new(bytes: &'out mut [u8]) -> Self {
        Self {
            headers: bytemuck::cast_slice_mut(bytes),
        }
    }

    fn take_header(&mut self) -> Result<&mut ProgramHeader> {
        crate::slice::take_first_mut(&mut self.headers)
            .ok_or_else(|| anyhow!("Insufficient header slots"))
    }
}
