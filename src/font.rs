//! Font utility and subsetting.

use std::fmt;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::collections::HashMap;
use byteorder::{BE, ReadBytesExt, WriteBytesExt};
use opentype::{OpenTypeReader, Outlines, TableRecord, Tag};
use opentype::tables::{Header, CharMap, MaximumProfile, HorizontalMetrics};

/// An font wrapper which allows to subset a font.
pub struct Font {
    program: Vec<u8>,
}

impl Font {
    /// Create a new font from a font program.
    pub fn new(program: Vec<u8>) -> Font {
        Font { program }
    }

    /// Generate a subsetted version of this font including only the chars listed in
    /// `chars`.
    ///
    /// The resulting pair contains the new font data and the new glyph mapping.
    ///
    /// All needed tables will be included (returning an error if a table was not present
    /// in the  source font) and optional tables will be included if there were present
    /// in the source font.
    pub fn subsetted<C, I1, S1, I2, S2>(
        &self,
        chars: C,
        needed_tables: I1,
        optional_tables: I2
    ) -> Result<(Vec<u8>, HashMap<char, u16>), SubsettingError>
    where
        C: IntoIterator<Item=char>,
        I1: IntoIterator<Item=S1>, S1: AsRef<str>,
        I2: IntoIterator<Item=S2>, S2: AsRef<str>
    {
        let mut chars: Vec<char> = chars.into_iter().collect();
        chars.sort();
        let mut cursor = Cursor::new(&self.program);
        let mut reader = OpenTypeReader::new(&mut cursor);
        let outlines = reader.outlines()?;
        let mut tables = reader.tables()?.to_vec();
        tables.sort_by_key(|r| r.tag);

        Subsetter {
            program: &self.program,
            reader,
            outlines,
            tables,
            cmap: None,
            hmtx: None,
            loca: None,
            glyphs: Vec::with_capacity(chars.len()),
            chars,
            records: Vec::new(),
            body: Vec::new(),
        }.subset(needed_tables, optional_tables)
    }
}

struct Subsetter<'p> {
    // Original font
    program: &'p [u8],
    reader: OpenTypeReader<'p, Cursor<&'p Vec<u8>>>,
    outlines: Outlines,
    tables: Vec<TableRecord>,
    cmap: Option<CharMap>,
    hmtx: Option<HorizontalMetrics>,
    loca: Option<Vec<u32>>,
    glyphs: Vec<u16>,

    // Subsetted font
    chars: Vec<char>,
    records: Vec<TableRecord>,
    body: Vec<u8>,
}

impl<'p> Subsetter<'p> {
    fn subset<I1, S1, I2, S2>(mut self, needed_tables: I1, optional_tables: I2)
    -> SubsetResult<(Vec<u8>, HashMap<char, u16>)>
    where
        I1: IntoIterator<Item=S1>, S1: AsRef<str>,
        I2: IntoIterator<Item=S2>, S2: AsRef<str>
    {
        // Find out which glyphs to include based on which characters we want
        // and which glyphs are used by composition.
        self.build_glyphs()?;

        // Iterate through the needed tables first
        for table in needed_tables.into_iter() {
            let table = table.as_ref();
            let tag: Tag = table.parse()
                .map_err(|_| SubsettingError::UnsupportedTable(table.to_string()))?;

            if self.contains(tag) {
                self.write_table(tag)?;
            } else {
                return Err(SubsettingError::MissingTable(tag.to_string()));
            }
        }

        // Now iterate through the optional tables
        for table in optional_tables.into_iter() {
            let table = table.as_ref();
            let tag: Tag = table.parse()
                .map_err(|_| SubsettingError::UnsupportedTable(table.to_string()))?;

            if self.contains(tag) {
                self.write_table(tag)?;
            }
        }

        self.write_header()?;

        let mapping = self.chars.into_iter().enumerate().map(|(i, c)| (c, i as u16))
            .collect::<HashMap<char, u16>>();

        Ok((self.body, mapping))
    }

    fn build_glyphs(&mut self) -> SubsetResult<()> {
        self.read_cmap()?;
        let cmap = self.cmap.as_ref().unwrap();

        for &c in &self.chars {
            self.glyphs.push(take_char(cmap.get(c), c)?)
        }

        // Composite glyphs may need additional glyphs we have not yet in our list.
        // So now we have a look at the glyf table to check that and add glyphs
        // we need additionally.
        if self.contains("glyf".parse().unwrap()) {
            self.read_loca()?;
            let loca = self.loca.as_ref().unwrap();
            let table = self.get_table_data("glyf".parse().unwrap())?;

            let mut i = 0;
            while i < self.glyphs.len() {
                let glyph = self.glyphs[i];

                let start = *take_invalid(loca.get(glyph as usize))? as usize;
                let end = *take_invalid(loca.get(glyph as usize + 1))? as usize;

                let glyph = table.get(start..end).ok_or(SubsettingError::InvalidFont)?;

                if end > start {
                    let mut cursor = Cursor::new(&glyph);
                    let num_contours = cursor.read_i16::<BE>()?;

                    // This is a composite glyph
                    if num_contours < 0 {
                        cursor.seek(SeekFrom::Current(8))?;
                        loop {
                            let flags = cursor.read_u16::<BE>()?;
                            let glyph_index = cursor.read_u16::<BE>()?;

                            if self.glyphs.iter().rev().find(|&&x| x == glyph_index).is_none() {
                                self.glyphs.push(glyph_index);
                            }

                            // This was the last component
                            if flags & 0x0020 == 0 {
                                break;
                            }

                            let args_len = if flags & 0x0001 == 1 { 4 } else { 2 };
                            cursor.seek(SeekFrom::Current(args_len))?;
                        }
                    }
                }

                i += 1;
            }
        }

        Ok(())
    }

    fn write_header(&mut self) -> SubsetResult<()> {
        // Create an output buffer
        let header_len = 12 + self.records.len() * 16;
        let mut header = Vec::with_capacity(header_len);

        let num_tables = self.records.len() as u16;

        // The highester power lower than the table count.
        let mut max_power = 1u16;
        while max_power * 2 <= num_tables {
            max_power *= 2;
        }
        max_power = std::cmp::min(max_power, num_tables);

        let search_range = max_power * 16;
        let entry_selector = (max_power as f32).log2() as u16;
        let range_shift = num_tables * 16 - search_range;

        // Write the base header
        header.write_u32::<BE>(match self.outlines {
            Outlines::TrueType => 0x00010000,
            Outlines::CFF => 0x4f54544f,
        })?;
        header.write_u16::<BE>(num_tables)?;
        header.write_u16::<BE>(search_range)?;
        header.write_u16::<BE>(entry_selector)?;
        header.write_u16::<BE>(range_shift)?;

        // Write the table records
        for record in &self.records {
            header.extend(record.tag.value());
            header.write_u32::<BE>(record.check_sum)?;
            header.write_u32::<BE>(header_len as u32 + record.offset)?;
            header.write_u32::<BE>(record.length)?;
        }

        header.append(&mut self.body);
        self.body = header;

        Ok(())
    }

    fn write_table(&mut self, tag: Tag) -> SubsetResult<()> {
        match tag.value() {
            b"head" | b"cvt " | b"prep" | b"fpgm" | b"name" | b"post" | b"OS/2" => {
                self.copy_table(tag)
            },
            b"hhea" => {
                let table = self.get_table_data(tag)?;
                let glyph_count = self.glyphs.len() as u16;
                self.write_table_body(tag, |this| {
                    this.body.extend(&table[..table.len() - 2]);
                    Ok(this.body.write_u16::<BE>(glyph_count)?)
                })
            },
            b"maxp" => {
                let table = self.get_table_data(tag)?;
                let glyph_count = self.glyphs.len() as u16;
                self.write_table_body(tag, |this| {
                    this.body.extend(&table[..4]);
                    this.body.write_u16::<BE>(glyph_count)?;
                    Ok(this.body.extend(&table[6..]))
                })
            },
            b"hmtx" => {
                self.write_table_body(tag, |this| {
                    this.read_hmtx()?;
                    let metrics = this.hmtx.as_ref().unwrap();

                    for &glyph in &this.glyphs {
                        let metrics = take_invalid(metrics.get(glyph))?;

                        this.body.write_i16::<BE>(metrics.advance_width)?;
                        this.body.write_i16::<BE>(metrics.left_side_bearing)?;
                    }
                    Ok(())
                })
            },
            b"loca" => {
                self.write_table_body(tag, |this| {
                    this.read_loca()?;
                    let loca = this.loca.as_ref().unwrap();

                    let mut offset = 0;
                    for &glyph in &this.glyphs {
                        this.body.write_u32::<BE>(offset)?;
                        let len = take_invalid(loca.get(glyph as usize + 1))?
                                - take_invalid(loca.get(glyph as usize))?;
                        offset += len;
                    }
                    this.body.write_u32::<BE>(offset)?;
                    Ok(())
                })
            },

            b"glyf" => {
                self.write_table_body(tag, |this| {
                    this.read_loca()?;
                    let loca = this.loca.as_ref().unwrap();
                    let table = this.get_table_data(tag)?;

                    for &glyph in &this.glyphs {
                        let start = *take_invalid(loca.get(glyph as usize))? as usize;
                        let end = *take_invalid(loca.get(glyph as usize + 1))? as usize;

                        let mut data = table.get(start..end)
                            .ok_or(SubsettingError::InvalidFont)?.to_vec();

                        if end > start {
                            let mut cursor = Cursor::new(&mut data);
                            let num_contours = cursor.read_i16::<BE>()?;

                            // This is a composite glyph
                            if num_contours < 0 {
                                cursor.seek(SeekFrom::Current(8))?;
                                loop {
                                    let flags = cursor.read_u16::<BE>()?;

                                    let glyph_index = cursor.read_u16::<BE>()?;
                                    let new_glyph_index = this.glyphs.iter()
                                        .position(|&g| g == glyph_index)
                                        .ok_or(SubsettingError::InvalidFont)? as u16;

                                    cursor.seek(SeekFrom::Current(-2))?;
                                    cursor.write_u16::<BE>(new_glyph_index)?;

                                    // This was the last component
                                    if flags & 0x0020 == 0 {
                                        break;
                                    }


                                    let args_len = if flags & 0x0001 == 1 { 4 } else { 2 };
                                    cursor.seek(SeekFrom::Current(args_len))?;
                                }
                            }
                        }

                        this.body.extend(data);
                    }
                    Ok(())
                })
            },

            b"cmap" => {
                // Always uses format 12 for simplicity
                self.write_table_body(tag, |this| {
                    // Find out which chars are in consecutive groups
                    let mut groups = Vec::new();
                    let len = this.chars.len();
                    let mut i = 0;
                    while i < len {
                        let start = i;
                        while i + 1 < len && this.chars[i+1] as u32 == this.chars[i] as u32 + 1 {
                            i += 1;
                        }
                        groups.push((this.chars[start], this.chars[i], start));
                        i += 1;
                    }

                    // Table header
                    this.body.write_u16::<BE>(0)?;
                    this.body.write_u16::<BE>(1)?;
                    this.body.write_u16::<BE>(3)?;
                    this.body.write_u16::<BE>(1)?;
                    this.body.write_u32::<BE>(12)?;

                    // Subtable header
                    this.body.write_u16::<BE>(12)?;
                    this.body.write_u16::<BE>(0)?;
                    this.body.write_u32::<BE>((16 + 12 * groups.len()) as u32)?;
                    this.body.write_u32::<BE>(0)?;
                    this.body.write_u32::<BE>(groups.len() as u32)?;

                    // Subtable body
                    for group in &groups {
                        this.body.write_u32::<BE>(group.0 as u32)?;
                        this.body.write_u32::<BE>(group.1 as u32)?;
                        this.body.write_u32::<BE>(group.2 as u32)?;
                    }

                    Ok(())
                })
            },

            _ => Err(SubsettingError::UnsupportedTable(tag.to_string())),
        }
    }

    fn copy_table(&mut self, tag: Tag) -> SubsetResult<()> {
        self.write_table_body(tag, |this| {
            let table = this.get_table_data(tag)?;
            Ok(this.body.extend(table))
        })
    }

    fn write_table_body<F>(&mut self, tag: Tag, writer: F) -> SubsetResult<()>
    where F: FnOnce(&mut Self) -> SubsetResult<()> {
        let start = self.body.len();
        writer(self)?;
        let end = self.body.len();
        while (self.body.len() - start) % 4 != 0 {
            self.body.push(0);
        }

        Ok(self.records.push(TableRecord {
            tag,
            check_sum: calculate_check_sum(&self.body[start..]),
            offset: start as u32,
            length: (end - start) as u32,
        }))
    }

    fn get_table_data(&self, tag: Tag) -> SubsetResult<&'p [u8]> {
        let record = match self.tables.binary_search_by_key(&tag, |r| r.tag) {
            Ok(index) => &self.tables[index],
            Err(_) => return Err(SubsettingError::MissingTable(tag.to_string())),
        };

        self.program.get(record.offset as usize .. (record.offset + record.length) as usize)
            .ok_or(SubsettingError::InvalidFont)
    }

    fn contains(&self, tag: Tag) -> bool {
        self.tables.binary_search_by_key(&tag, |r| r.tag).is_ok()
    }

    fn read_cmap(&mut self) -> SubsetResult<()> {
        Ok(if self.cmap.is_none() {
            self.cmap = Some(self.reader.read_table::<CharMap>()?);
        })
    }

    fn read_hmtx(&mut self) -> SubsetResult<()> {
        Ok(if self.hmtx.is_none() {
            self.hmtx = Some(self.reader.read_table::<HorizontalMetrics>()?);
        })
    }

    fn read_loca(&mut self) -> SubsetResult<()> {
        Ok(if self.loca.is_none() {
            let mut table = self.get_table_data("loca".parse().unwrap())?;
            let format = self.reader.read_table::<Header>()?.index_to_loc_format;
            let count = self.reader.read_table::<MaximumProfile>()?.num_glyphs + 1;

            let loca = if format == 0 {
                (0..count).map(|_| table.read_u16::<BE>()
                    .map(|x| (x as u32) * 2))
                    .collect::<io::Result<Vec<u32>>>()
            } else {
                (0..count).map(|_| table.read_u32::<BE>())
                    .collect::<io::Result<Vec<u32>>>()
            }?;

            self.loca = Some(loca);
        })
    }
}


/// Calculate a checksum over the sliced data as sum of u32's.
/// The data length has to be a multiple of four.
fn calculate_check_sum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    data.chunks_exact(4).for_each(|c| {
        sum = sum.wrapping_add(
            ((c[0] as u32) << 24)
          + ((c[1] as u32) << 16)
          + ((c[2] as u32) << 8)
          + (c[3] as u32)
        );
    });
    sum
}

/// Returns an error about a missing character or the wrapped data.
fn take_char<T>(opt: Option<T>, character: char) -> SubsetResult<T> {
    opt.ok_or(SubsettingError::MissingCharacter(character))
}

/// Returns an error about a missing glyph or the wrapped data.
fn take_invalid<T>(opt: Option<T>) -> SubsetResult<T> {
    opt.ok_or(SubsettingError::InvalidFont)
}



type SubsetResult<T> = Result<T, SubsettingError>;

/// A failure when subsetting a font.
#[derive(Debug)]
pub enum SubsettingError {
    /// A requested table was not present in the source font.
    MissingTable(String),
    /// The table is unknown to the engine (unimplemented or invalid).
    UnsupportedTable(String),
    /// A requested character was not present in the source.
    MissingCharacter(char),
    /// The font is invalid.
    InvalidFont,
    /// There was an error while parsing the font file.
    FontError(opentype::Error),
    /// A general I/O error.
    IoError(io::Error),
}

impl From<io::Error> for SubsettingError {
    fn from(err: io::Error) -> SubsettingError {
        SubsettingError::IoError(err)
    }
}

impl From<opentype::Error> for SubsettingError {
    fn from(err: opentype::Error) -> SubsettingError {
        SubsettingError::FontError(err)
    }
}

impl fmt::Display for SubsettingError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use SubsettingError::*;
        write!(f, "subsetting error: ")?;
        match self {
            MissingTable(table) => write!(f, "missing table: {}", table),
            UnsupportedTable(table) => write!(f, "unsupported table: {}", table),
            MissingCharacter(c) => write!(f, "missing character: {}", c),
            InvalidFont => write!(f, "invalid font"),
            FontError(err) => write!(f, "font error: {}", err),
            IoError(err) => write!(f, "io error: {}", err),
        }
    }
}
