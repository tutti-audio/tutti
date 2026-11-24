//! A std-only reader of the SCIP index `rust-analyzer scip` writes: the protobuf
//! wire format's varints and length-delimited fields, and the few messages the
//! `unused-public` recipe reads.
//!
//! `Index { documents = 2 }`, `Document { relative_path = 1, occurrences = 2 }`,
//! `Occurrence { range = 1 (packed int32), symbol = 2, symbol_roles = 3 }`. Every
//! other field is skipped by its wire type.

/// The `symbol_roles` bit of a definition.
const DEFINITION: i32 = 1;

/// One occurrence of a symbol in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Occurrence {
    /// The 0-based line the occurrence starts on.
    pub(super) line: u32,
    /// The 0-based line it ends on.
    pub(super) end_line: u32,
    pub(super) symbol: String,
    pub(super) roles: i32,
}

impl Occurrence {
    pub(super) fn is_definition(&self) -> bool {
        self.roles & DEFINITION != 0
    }
}

/// A source file of the index and what occurs in it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Document {
    pub(super) path: String,
    pub(super) occurrences: Vec<Occurrence>,
}

/// The documents of a serialized `Index`.
pub(super) fn documents(
    bytes: &[u8],
) -> Result<impl Iterator<Item = Result<Document, String>> + '_, String> {
    Ok(fields(bytes)?
        .into_iter()
        .filter_map(|f| match (f.number, f.value) {
            (2, Value::Bytes(b)) => Some(document(b)),
            _ => None,
        }))
}

fn document(bytes: &[u8]) -> Result<Document, String> {
    let mut d = Document::default();
    for f in fields(bytes)? {
        match (f.number, f.value) {
            (1, Value::Bytes(b)) => d.path = text(b)?,
            (2, Value::Bytes(b)) => d.occurrences.push(occurrence(b)?),
            _ => {}
        }
    }
    Ok(d)
}

fn occurrence(bytes: &[u8]) -> Result<Occurrence, String> {
    let (mut range, mut symbol, mut roles) = (Vec::new(), String::new(), 0);
    for f in fields(bytes)? {
        match (f.number, f.value) {
            (1, Value::Bytes(b)) => range = packed(b)?,
            (2, Value::Bytes(b)) => symbol = text(b)?,
            (3, Value::Varint(v)) => roles = v as i32,
            _ => {}
        }
    }
    // `[line, start, end]` or `[start line, start, end line, end]`.
    let (line, end_line) = match range[..] {
        [l, _, _] => (l, l),
        [l, _, e, _] => (l, e),
        _ => return Err(format!("an occurrence range of {} numbers", range.len())),
    };
    Ok(Occurrence {
        line: u32::try_from(line).map_err(|_| "a negative line".to_owned())?,
        end_line: u32::try_from(end_line).map_err(|_| "a negative line".to_owned())?,
        symbol,
        roles,
    })
}

fn text(b: &[u8]) -> Result<String, String> {
    String::from_utf8(b.to_vec()).map_err(|e| e.to_string())
}

/// A packed `repeated int32`.
fn packed(mut b: &[u8]) -> Result<Vec<i64>, String> {
    let mut out = Vec::new();
    while !b.is_empty() {
        out.push(varint(&mut b)? as i32 as i64);
    }
    Ok(out)
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    /// A fixed-width field, which the messages read here do not use.
    Fixed,
}

struct Field<'a> {
    number: u32,
    value: Value<'a>,
}

fn varint(b: &mut &[u8]) -> Result<u64, String> {
    let mut v = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = b.split_first().ok_or("a varint cut short")?;
        *b = rest;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err("a varint longer than ten bytes".to_owned())
}

/// The fields of one message, in order; a group (wire types 3 and 4) is an error.
fn fields(mut b: &[u8]) -> Result<Vec<Field<'_>>, String> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let key = varint(&mut b)?;
        let number = u32::try_from(key >> 3).map_err(|_| "a field number too large")?;
        let value = match key & 7 {
            0 => Value::Varint(varint(&mut b)?),
            1 | 5 => {
                let n = if key & 7 == 1 { 8 } else { 4 };
                b = b.get(n..).ok_or("a fixed field cut short")?;
                Value::Fixed
            }
            2 => {
                let len = usize::try_from(varint(&mut b)?).map_err(|_| "a length too large")?;
                let body = b.get(..len).ok_or("a length-delimited field cut short")?;
                b = &b[len..];
                Value::Bytes(body)
            }
            w => return Err(format!("wire type {w}")),
        };
        out.push(Field { number, value });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn len_field(number: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![number << 3 | 2, body.len() as u8];
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn varint_reads_one_and_many_bytes() {
        assert_eq!(varint(&mut &[0x05][..]).unwrap(), 5);
        assert_eq!(varint(&mut &[0xac, 0x02][..]).unwrap(), 300);
        assert!(varint(&mut &[0x80][..]).is_err());
    }

    #[test]
    fn a_packed_range_reads_negative_and_positive_numbers() {
        assert_eq!(packed(&[3, 4, 9]).unwrap(), [3, 4, 9]);
        let neg = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(packed(&neg).unwrap(), [-1]);
    }

    #[test]
    fn nested_messages_decode_to_documents_and_skip_unknown_fields() {
        let mut occ = len_field(1, &[7, 4, 9]);
        occ.extend(len_field(2, b"rust-analyzer cargo a 0.0.1 f()."));
        occ.extend([3 << 3, 1]);
        occ.extend([9 << 3 | 5, 0, 0, 0, 0]); // unknown fixed32
        let mut doc = len_field(1, b"crates/a/src/lib.rs");
        doc.extend(len_field(2, &occ));
        doc.extend(len_field(7, b"unknown"));
        let mut index = len_field(1, b"metadata");
        index.extend(len_field(2, &doc));
        let docs: Vec<_> = documents(&index).unwrap().map(Result::unwrap).collect();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, "crates/a/src/lib.rs");
        let o = &docs[0].occurrences[0];
        assert_eq!((o.line, o.end_line), (7, 7));
        assert_eq!(o.symbol, "rust-analyzer cargo a 0.0.1 f().");
        assert!(o.is_definition());
    }

    #[test]
    fn a_four_number_range_ends_on_its_own_line() {
        let occ = len_field(1, &[2, 0, 5, 1]);
        assert_eq!(occurrence(&occ).unwrap().end_line, 5);
    }

    #[test]
    fn a_truncated_field_is_an_error() {
        assert!(fields(&[2 << 3 | 2, 9, 1]).is_err());
    }
}
