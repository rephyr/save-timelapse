//! Decode and encode Factorio's `mod-settings.dat`.
//!
//! The exporter sets one startup flag without disturbing anything else, so the
//! real structure is decoded rather than pattern matched: writing a minimal
//! replacement would discard every other mod's configuration.
//!
//! See docs/ARCHITECTURE.md for the wire format.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

const T_NONE: u8 = 0;
const T_BOOL: u8 = 1;
const T_F64: u8 = 2;
const T_STR: u8 = 3;
const T_LIST: u8 = 4;
const T_DICT: u8 = 5;
const T_I64: u8 = 6;
const T_U64: u8 = 7;

pub type Version = [u16; 4];

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Nothing,
    Flag(bool),
    Float(f64),
    Text(String),
    /// Order is preserved so untouched files re-encode byte for byte.
    Sequence(Vec<(String, Entry)>),
    Table(Vec<(String, Entry)>),
    Signed(i64),
    Unsigned(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub any_type: u8,
    pub payload: Payload,
}

impl Entry {
    /// Factorio wraps every setting value in a single key table.
    pub fn flag(value: bool) -> Entry {
        Entry {
            any_type: 0,
            payload: Payload::Table(vec![("value".to_string(), Entry { any_type: 0, payload: Payload::Flag(value) })]),
        }
    }
}

pub struct SettingsFile {
    pub version: Version,
    pub header_flag: u8,
    pub root: Entry,
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "truncated mod-settings.dat")
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> io::Result<u8> {
        let v = *self.bytes.get(self.at).ok_or_else(truncated)?;
        self.at += 1;
        Ok(v)
    }

    fn slice(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(truncated)?;
        let s = self.bytes.get(self.at..end).ok_or_else(truncated)?;
        self.at = end;
        Ok(s)
    }

    fn short(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.slice(2)?.try_into().unwrap()))
    }

    fn long(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.slice(4)?.try_into().unwrap()))
    }

    fn quad(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.slice(8)?.try_into().unwrap()))
    }

    fn text(&mut self) -> io::Result<String> {
        if self.byte()? != 0 {
            return Ok(String::new());
        }
        let mut len = self.byte()? as usize;
        if len == 0xFF {
            len = self.long()? as usize;
        }
        String::from_utf8(self.slice(len)?.to_vec()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn entry(&mut self) -> io::Result<Entry> {
        let kind = self.byte()?;
        let any_type = self.byte()?;
        let payload = match kind {
            T_NONE => Payload::Nothing,
            T_BOOL => Payload::Flag(self.byte()? != 0),
            T_F64 => Payload::Float(f64::from_le_bytes(self.slice(8)?.try_into().unwrap())),
            T_STR => Payload::Text(self.text()?),
            T_I64 => Payload::Signed(self.quad()? as i64),
            T_U64 => Payload::Unsigned(self.quad()?),
            T_LIST | T_DICT => {
                let count = self.long()? as usize;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    let key = self.text()?;
                    items.push((key, self.entry()?));
                }
                if kind == T_LIST {
                    Payload::Sequence(items)
                } else {
                    Payload::Table(items)
                }
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown property type {other} at offset {}", self.at - 2),
                ))
            }
        };
        Ok(Entry { any_type, payload })
    }
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    // Factorio emits the empty-string flag as 0 followed by a zero length,
    // never the flag=1 shortcut. Matching that keeps re-encoding exact.
    out.push(0);
    let raw = text.as_bytes();
    if raw.len() < 0xFF {
        out.push(raw.len() as u8);
    } else {
        out.push(0xFF);
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    }
    out.extend_from_slice(raw);
}

fn put_entry(out: &mut Vec<u8>, entry: &Entry) {
    let kind = match &entry.payload {
        Payload::Nothing => T_NONE,
        Payload::Flag(_) => T_BOOL,
        Payload::Float(_) => T_F64,
        Payload::Text(_) => T_STR,
        Payload::Sequence(_) => T_LIST,
        Payload::Table(_) => T_DICT,
        Payload::Signed(_) => T_I64,
        Payload::Unsigned(_) => T_U64,
    };
    out.push(kind);
    out.push(entry.any_type);

    match &entry.payload {
        Payload::Nothing => {}
        Payload::Flag(v) => out.push(u8::from(*v)),
        Payload::Float(v) => out.extend_from_slice(&v.to_le_bytes()),
        Payload::Text(s) => put_text(out, s),
        Payload::Signed(v) => out.extend_from_slice(&(*v as u64).to_le_bytes()),
        Payload::Unsigned(v) => out.extend_from_slice(&v.to_le_bytes()),
        Payload::Sequence(items) | Payload::Table(items) => {
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for (key, child) in items {
                put_text(out, key);
                put_entry(out, child);
            }
        }
    }
}

pub fn decode(bytes: &[u8]) -> io::Result<SettingsFile> {
    let mut c = Cursor { bytes, at: 0 };
    let version = [c.short()?, c.short()?, c.short()?, c.short()?];
    let header_flag = c.byte()?;
    let root = c.entry()?;
    if c.at != bytes.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("trailing data: read {} of {} bytes", c.at, bytes.len())));
    }
    Ok(SettingsFile { version, header_flag, root })
}

pub fn encode(file: &SettingsFile) -> Vec<u8> {
    let mut out = Vec::new();
    for part in file.version {
        out.extend_from_slice(&part.to_le_bytes());
    }
    out.push(file.header_flag);
    put_entry(&mut out, &file.root);
    out
}

impl SettingsFile {
    /// The three sections Factorio expects, all empty.
    pub fn blank(version: Version) -> SettingsFile {
        let sections = ["startup", "runtime-global", "runtime-per-user"]
            .iter()
            .map(|name| (name.to_string(), Entry { any_type: 0, payload: Payload::Table(Vec::new()) }))
            .collect();
        SettingsFile { version, header_flag: 0, root: Entry { any_type: 0, payload: Payload::Table(sections) } }
    }

    /// Insert or replace one setting, leaving every other entry as it was.
    pub fn put(&mut self, section: &str, name: &str, entry: Entry) {
        let Payload::Table(sections) = &mut self.root.payload else { return };

        if let Some((_, found)) = sections.iter_mut().find(|(key, _)| key == section) {
            if let Payload::Table(entries) = &mut found.payload {
                match entries.iter_mut().find(|(key, _)| key == name) {
                    Some(slot) => slot.1 = entry,
                    None => entries.push((name.to_string(), entry)),
                }
            }
            return;
        }

        sections.push((section.to_string(), Entry { any_type: 0, payload: Payload::Table(vec![(name.to_string(), entry)]) }));
    }

    /// Flat `(section, name) -> value` view, for assertions and diffing.
    pub fn listing(&self) -> BTreeMap<(String, String), Payload> {
        let mut out = BTreeMap::new();
        let Payload::Table(sections) = &self.root.payload else { return out };

        for (section, holder) in sections {
            let Payload::Table(entries) = &holder.payload else { continue };
            for (name, entry) in entries {
                if let Payload::Table(inner) = &entry.payload {
                    if let Some((_, value)) = inner.iter().find(|(key, _)| key == "value") {
                        out.insert((section.clone(), name.clone()), value.payload.clone());
                    }
                }
            }
        }
        out
    }
}

/// Write `source` to `target` with `changes` applied, preserving all else.
///
/// A missing or undecodable source yields a blank file containing only
/// `changes`. Nothing is lost in that case because nothing was readable.
pub fn apply_to_file(source: &Path, target: &Path, changes: &[(&str, &str, bool)], fallback: Version) -> io::Result<()> {
    let mut file = match std::fs::read(source) {
        Ok(bytes) => decode(&bytes).unwrap_or_else(|err| {
            eprintln!(
                "warning: {} could not be decoded ({err}); other mods will use \
                 their defaults for this export",
                source.display()
            );
            SettingsFile::blank(fallback)
        }),
        Err(_) => SettingsFile::blank(fallback),
    };

    for (section, name, value) in changes {
        file.put(section, name, Entry::flag(*value));
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(target, encode(&file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_encodes_as_zero_length_not_the_flag() {
        let mut out = Vec::new();
        put_text(&mut out, "");
        assert_eq!(out, vec![0, 0]);
    }

    #[test]
    fn re_encoding_is_byte_identical() {
        let mut file = SettingsFile::blank([2, 0, 77, 0]);
        file.put("startup", "save-timelapse-headless-scan", Entry::flag(true));
        file.put("runtime-global", "unrelated-mod-option", Entry::flag(false));

        let bytes = encode(&file);
        let decoded = decode(&bytes).expect("decodes");
        assert_eq!(encode(&decoded), bytes);
        assert_eq!(decoded.version, [2, 0, 77, 0]);
    }

    #[test]
    fn replacing_a_value_keeps_its_neighbours() {
        let mut file = SettingsFile::blank([2, 0, 77, 0]);
        file.put("startup", "first", Entry::flag(true));
        file.put("startup", "second", Entry::flag(true));
        file.put("startup", "first", Entry::flag(false));

        let listing = file.listing();
        assert_eq!(listing.len(), 2);
        assert_eq!(listing[&("startup".into(), "first".into())], Payload::Flag(false));
        assert_eq!(listing[&("startup".into(), "second".into())], Payload::Flag(true));
    }

    #[test]
    fn long_names_use_the_extended_length_form() {
        let name = "x".repeat(300);
        let mut file = SettingsFile::blank([2, 0, 77, 0]);
        file.put("startup", &name, Entry::flag(true));

        let bytes = encode(&file);
        let decoded = decode(&bytes).expect("decodes");
        assert!(decoded.listing().contains_key(&("startup".into(), name)));
    }
}
