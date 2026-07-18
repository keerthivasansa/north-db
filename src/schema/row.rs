use std::error::Error;
use std::fmt;

use crate::heap::SLOT_ENTRY_SIZE;
use crate::storage::{PAGE_HEADER_SIZE, PAGE_SIZE};

use super::table::{ColumnSchema, DataType};

const ROW_FORMAT_VERSION: u8 = 1;
const VARIABLE_DESCRIPTOR_SIZE: usize = 8;

pub const MAX_ROW_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE - SLOT_ENTRY_SIZE;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueRef<'row> {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(&'row str),
    Bytes(&'row [u8]),
}

impl ValueRef<'_> {
    pub fn to_owned(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Int(value) => Value::Int(value),
            Self::Float(value) => Value::Float(value),
            Self::Bool(value) => Value::Bool(value),
            Self::Text(value) => Value::Text(value.to_owned()),
            Self::Bytes(value) => Value::Bytes(value.to_vec()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowLayout {
    null_bitmap_size: usize,
    columns: Vec<ColumnLayout>,
    minimum_row_size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ColumnLayout {
    offset: usize,
    width: usize,
}

impl RowLayout {
    pub(super) fn compile(columns: &[ColumnSchema]) -> Self {
        let null_bitmap_size = columns.len().div_ceil(8);
        let mut offset = 1 + null_bitmap_size;
        let mut layouts = Vec::with_capacity(columns.len());
        for column in columns {
            let width = match column.data_type() {
                DataType::Int | DataType::Float => 8,
                DataType::Bool => 1,
                DataType::Text | DataType::Bytes => VARIABLE_DESCRIPTOR_SIZE,
            };
            layouts.push(ColumnLayout { offset, width });
            offset += width;
        }
        Self {
            null_bitmap_size,
            columns: layouts,
            minimum_row_size: offset,
        }
    }

    pub const fn null_bitmap_size(&self) -> usize {
        self.null_bitmap_size
    }

    pub const fn minimum_row_size(&self) -> usize {
        self.minimum_row_size
    }

    pub fn column_offset(&self, index: usize) -> Option<usize> {
        self.columns.get(index).map(|column| column.offset)
    }

    pub(super) fn encode(
        &self,
        columns: &[ColumnSchema],
        values: &[Value],
    ) -> Result<Vec<u8>, RowError> {
        if values.len() != columns.len() {
            return Err(RowError::WrongValueCount {
                expected: columns.len(),
                found: values.len(),
            });
        }

        let mut row = vec![0; self.minimum_row_size];
        row[0] = ROW_FORMAT_VERSION;
        for (index, ((column, layout), value)) in
            columns.iter().zip(&self.columns).zip(values).enumerate()
        {
            if matches!(value, Value::Null) {
                if !column.is_nullable() {
                    return Err(RowError::NullNotAllowed {
                        column: column.name().to_owned(),
                    });
                }
                set_null(&mut row, index);
                continue;
            }

            match (column.data_type(), value) {
                (DataType::Int, Value::Int(value)) => {
                    row[layout.offset..layout.offset + 8].copy_from_slice(&value.to_le_bytes());
                }
                (DataType::Float, Value::Float(value)) => {
                    row[layout.offset..layout.offset + 8].copy_from_slice(&value.to_le_bytes());
                }
                (DataType::Bool, Value::Bool(value)) => {
                    row[layout.offset] = u8::from(*value);
                }
                (DataType::Text, Value::Text(value)) => {
                    append_variable(&mut row, layout.offset, value.as_bytes())?;
                }
                (DataType::Bytes, Value::Bytes(value)) => {
                    append_variable(&mut row, layout.offset, value)?;
                }
                (expected, found) => {
                    return Err(RowError::TypeMismatch {
                        column: column.name().to_owned(),
                        expected,
                        found: value_type_name(found),
                    });
                }
            }
        }

        if row.len() > MAX_ROW_SIZE {
            return Err(RowError::RowTooLarge {
                size: row.len(),
                maximum: MAX_ROW_SIZE,
            });
        }
        Ok(row)
    }

    pub(super) fn decode<'row>(
        &self,
        columns: &[ColumnSchema],
        row: &'row [u8],
    ) -> Result<Vec<ValueRef<'row>>, RowError> {
        self.validate(columns, row)?;
        (0..columns.len())
            .map(|index| self.decode_column_unchecked(columns, row, index))
            .collect()
    }

    pub(super) fn project<'row>(
        &self,
        schema_columns: &[ColumnSchema],
        row: &'row [u8],
        projected_columns: &[usize],
    ) -> Result<Vec<ValueRef<'row>>, RowError> {
        for &index in projected_columns {
            if index >= schema_columns.len() {
                return Err(RowError::ColumnOutOfBounds {
                    index,
                    column_count: schema_columns.len(),
                });
            }
        }
        self.validate(schema_columns, row)?;
        projected_columns
            .iter()
            .map(|&index| self.decode_column_unchecked(schema_columns, row, index))
            .collect()
    }

    pub(super) fn decode_column<'row>(
        &self,
        columns: &[ColumnSchema],
        row: &'row [u8],
        index: usize,
    ) -> Result<ValueRef<'row>, RowError> {
        if index >= columns.len() {
            return Err(RowError::ColumnOutOfBounds {
                index,
                column_count: columns.len(),
            });
        }
        self.validate(columns, row)?;
        self.decode_column_unchecked(columns, row, index)
    }

    fn validate(&self, columns: &[ColumnSchema], row: &[u8]) -> Result<(), RowError> {
        if row.len() < self.minimum_row_size {
            return Err(RowError::Corrupt("row is shorter than its fixed layout"));
        }
        if row.len() > MAX_ROW_SIZE {
            return Err(RowError::Corrupt("row exceeds maximum page payload"));
        }
        if row[0] != ROW_FORMAT_VERSION {
            return Err(RowError::Corrupt("unsupported row format version"));
        }

        let mut expected_payload_offset = self.minimum_row_size;
        for (index, (column, layout)) in columns.iter().zip(&self.columns).enumerate() {
            let bytes = &row[layout.offset..layout.offset + layout.width];
            if is_null(row, index) {
                if !column.is_nullable() {
                    return Err(RowError::Corrupt("required column is encoded as null"));
                }
                if bytes.iter().any(|byte| *byte != 0) {
                    return Err(RowError::Corrupt("null column has nonzero fixed bytes"));
                }
                continue;
            }

            match column.data_type() {
                DataType::Bool if !matches!(bytes[0], 0 | 1) => {
                    return Err(RowError::Corrupt("boolean column is not zero or one"));
                }
                DataType::Text | DataType::Bytes => {
                    let offset = read_u32(bytes, 0) as usize;
                    let length = read_u32(bytes, 4) as usize;
                    if offset != expected_payload_offset {
                        return Err(RowError::Corrupt(
                            "variable column payload is not canonical",
                        ));
                    }
                    let end = offset
                        .checked_add(length)
                        .filter(|end| *end <= row.len())
                        .ok_or(RowError::Corrupt("variable column exceeds row boundary"))?;
                    if column.data_type() == DataType::Text {
                        std::str::from_utf8(&row[offset..end])
                            .map_err(|_| RowError::Corrupt("text column is not UTF-8"))?;
                    }
                    expected_payload_offset = end;
                }
                _ => {}
            }
        }
        if expected_payload_offset != row.len() {
            return Err(RowError::Corrupt(
                "row has trailing or unreferenced payload",
            ));
        }
        Ok(())
    }

    fn decode_column_unchecked<'row>(
        &self,
        columns: &[ColumnSchema],
        row: &'row [u8],
        index: usize,
    ) -> Result<ValueRef<'row>, RowError> {
        if is_null(row, index) {
            return Ok(ValueRef::Null);
        }
        let column = &columns[index];
        let layout = &self.columns[index];
        let bytes = &row[layout.offset..layout.offset + layout.width];
        match column.data_type() {
            DataType::Int => Ok(ValueRef::Int(i64::from_le_bytes(
                bytes.try_into().expect("validated integer width"),
            ))),
            DataType::Float => Ok(ValueRef::Float(f64::from_le_bytes(
                bytes.try_into().expect("validated float width"),
            ))),
            DataType::Bool => Ok(ValueRef::Bool(bytes[0] == 1)),
            DataType::Text => {
                let payload = variable_payload(row, bytes);
                Ok(ValueRef::Text(
                    std::str::from_utf8(payload).expect("validated UTF-8 text"),
                ))
            }
            DataType::Bytes => Ok(ValueRef::Bytes(variable_payload(row, bytes))),
        }
    }
}

fn append_variable(
    row: &mut Vec<u8>,
    descriptor_offset: usize,
    value: &[u8],
) -> Result<(), RowError> {
    let encoded_size = row.len().saturating_add(value.len());
    if encoded_size > MAX_ROW_SIZE {
        return Err(RowError::RowTooLarge {
            size: encoded_size,
            maximum: MAX_ROW_SIZE,
        });
    }
    let offset = u32::try_from(row.len()).map_err(|_| RowError::RowTooLarge {
        size: row.len(),
        maximum: MAX_ROW_SIZE,
    })?;
    let length = u32::try_from(value.len()).map_err(|_| RowError::RowTooLarge {
        size: encoded_size,
        maximum: MAX_ROW_SIZE,
    })?;
    row[descriptor_offset..descriptor_offset + 4].copy_from_slice(&offset.to_le_bytes());
    row[descriptor_offset + 4..descriptor_offset + 8].copy_from_slice(&length.to_le_bytes());
    row.extend_from_slice(value);
    Ok(())
}

fn variable_payload<'row>(row: &'row [u8], descriptor: &[u8]) -> &'row [u8] {
    let offset = read_u32(descriptor, 0) as usize;
    let length = read_u32(descriptor, 4) as usize;
    &row[offset..offset + length]
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn set_null(row: &mut [u8], column_index: usize) {
    let byte = 1 + column_index / 8;
    let bit = column_index % 8;
    row[byte] |= 1 << bit;
}

fn is_null(row: &[u8], column_index: usize) -> bool {
    let byte = 1 + column_index / 8;
    let bit = column_index % 8;
    row[byte] & (1 << bit) != 0
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::Text(_) => "text",
        Value::Bytes(_) => "bytes",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowError {
    WrongValueCount {
        expected: usize,
        found: usize,
    },
    TypeMismatch {
        column: String,
        expected: DataType,
        found: &'static str,
    },
    NullNotAllowed {
        column: String,
    },
    ColumnOutOfBounds {
        index: usize,
        column_count: usize,
    },
    RowTooLarge {
        size: usize,
        maximum: usize,
    },
    Corrupt(&'static str),
}

impl fmt::Display for RowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongValueCount { expected, found } => {
                write!(formatter, "row has {found} values; expected {expected}")
            }
            Self::TypeMismatch {
                column,
                expected,
                found,
            } => write!(
                formatter,
                "column {column} expects {}, found {found}",
                expected.name()
            ),
            Self::NullNotAllowed { column } => {
                write!(formatter, "column {column} does not allow null")
            }
            Self::ColumnOutOfBounds {
                index,
                column_count,
            } => write!(
                formatter,
                "column index {index} is out of bounds for {column_count} columns"
            ),
            Self::RowTooLarge { size, maximum } => {
                write!(
                    formatter,
                    "encoded row is {size} bytes; maximum is {maximum}"
                )
            }
            Self::Corrupt(reason) => write!(formatter, "corrupt row encoding: {reason}"),
        }
    }
}

impl Error for RowError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::TableSchema;

    fn schema() -> TableSchema {
        TableSchema::new(
            "items",
            vec![
                ColumnSchema::primary_key("id", DataType::Int),
                ColumnSchema::required("score", DataType::Float),
                ColumnSchema::required("active", DataType::Bool),
                ColumnSchema::required("name", DataType::Text),
                ColumnSchema::nullable("metadata", DataType::Bytes),
            ],
        )
        .unwrap()
    }

    #[test]
    fn fixed_encoding_is_stable_and_little_endian() {
        let schema = TableSchema::new(
            "flags",
            vec![
                ColumnSchema::primary_key("id", DataType::Int),
                ColumnSchema::required("active", DataType::Bool),
            ],
        )
        .unwrap();
        let row = schema
            .encode_row(&[Value::Int(42), Value::Bool(true)])
            .unwrap();
        assert_eq!(row, vec![1, 0, 42, 0, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn round_trips_all_types_and_borrows_variable_values() {
        let schema = schema();
        let row = schema
            .encode_row(&[
                Value::Int(7),
                Value::Float(4.5),
                Value::Bool(true),
                Value::Text("north".to_owned()),
                Value::Bytes(vec![1, 2, 3]),
            ])
            .unwrap();
        assert_eq!(
            schema.decode_row(&row).unwrap(),
            vec![
                ValueRef::Int(7),
                ValueRef::Float(4.5),
                ValueRef::Bool(true),
                ValueRef::Text("north"),
                ValueRef::Bytes(&[1, 2, 3]),
            ]
        );
    }

    #[test]
    fn supports_null_and_projected_decoding() {
        let schema = schema();
        let row = schema
            .encode_row(&[
                Value::Int(9),
                Value::Float(1.0),
                Value::Bool(false),
                Value::Text("potato".to_owned()),
                Value::Null,
            ])
            .unwrap();
        assert_eq!(
            schema.project_row(&row, &[3, 0, 4]).unwrap(),
            vec![ValueRef::Text("potato"), ValueRef::Int(9), ValueRef::Null]
        );
        assert_eq!(schema.primary_key_value(&row).unwrap(), ValueRef::Int(9));
    }

    #[test]
    fn rejects_wrong_types_counts_and_required_nulls() {
        let schema = schema();
        assert!(matches!(
            schema.encode_row(&[Value::Int(1)]),
            Err(RowError::WrongValueCount { .. })
        ));
        assert!(matches!(
            schema.encode_row(&[
                Value::Text("wrong".into()),
                Value::Float(1.0),
                Value::Bool(true),
                Value::Text("name".into()),
                Value::Null,
            ]),
            Err(RowError::TypeMismatch { column, .. }) if column == "id"
        ));
        assert!(matches!(
            schema.encode_row(&[
                Value::Null,
                Value::Float(1.0),
                Value::Bool(true),
                Value::Text("name".into()),
                Value::Null,
            ]),
            Err(RowError::NullNotAllowed { column }) if column == "id"
        ));
    }

    #[test]
    fn rejects_noncanonical_or_invalid_row_bytes() {
        let schema = schema();
        let mut row = schema
            .encode_row(&[
                Value::Int(1),
                Value::Float(1.0),
                Value::Bool(true),
                Value::Text("valid".into()),
                Value::Null,
            ])
            .unwrap();
        let bool_offset = schema.layout().column_offset(2).unwrap();
        row[bool_offset] = 3;
        assert!(matches!(
            schema.decode_row(&row),
            Err(RowError::Corrupt("boolean column is not zero or one"))
        ));

        row[bool_offset] = 1;
        let text_offset = schema.layout().column_offset(3).unwrap();
        row[text_offset..text_offset + 4].copy_from_slice(&0_u32.to_le_bytes());
        assert!(matches!(
            schema.decode_row(&row),
            Err(RowError::Corrupt(
                "variable column payload is not canonical"
            ))
        ));
    }

    #[test]
    fn rejects_rows_too_large_for_one_heap_page() {
        let schema = TableSchema::new(
            "documents",
            vec![
                ColumnSchema::primary_key("id", DataType::Int),
                ColumnSchema::required("body", DataType::Text),
            ],
        )
        .unwrap();
        assert!(matches!(
            schema.encode_row(&[Value::Int(1), Value::Text("x".repeat(PAGE_SIZE))]),
            Err(RowError::RowTooLarge { .. })
        ));
    }
}
