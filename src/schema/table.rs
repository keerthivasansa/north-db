use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use super::row::{RowError, RowLayout, Value, ValueRef, MAX_ROW_SIZE};

const SCHEMA_MAGIC: &[u8; 4] = b"NSCH";
const SCHEMA_FORMAT_VERSION: u16 = 1;
const MAX_COLUMNS: usize = 1_024;
const MAX_IDENTIFIER_BYTES: usize = 255;

const FLAG_NULLABLE: u8 = 1;
const FLAG_PRIMARY_KEY: u8 = 2;
const KNOWN_COLUMN_FLAGS: u8 = FLAG_NULLABLE | FLAG_PRIMARY_KEY;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Int,
    Float,
    Bool,
    Text,
    Bytes,
}

impl DataType {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::Text => "text",
            Self::Bytes => "bytes",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Int => 1,
            Self::Float => 2,
            Self::Bool => 3,
            Self::Text => 4,
            Self::Bytes => 5,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, SchemaError> {
        match tag {
            1 => Ok(Self::Int),
            2 => Ok(Self::Float),
            3 => Ok(Self::Bool),
            4 => Ok(Self::Text),
            5 => Ok(Self::Bytes),
            _ => Err(SchemaError::CorruptEncoding("unknown column type tag")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
    nullable: bool,
    primary_key: bool,
}

impl ColumnSchema {
    pub fn required(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable: false,
            primary_key: false,
        }
    }

    pub fn nullable(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable: true,
            primary_key: false,
        }
    }

    pub fn primary_key(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable: false,
            primary_key: true,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn data_type(&self) -> DataType {
        self.data_type
    }

    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    pub const fn is_primary_key(&self) -> bool {
        self.primary_key
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSchema {
    name: String,
    columns: Vec<ColumnSchema>,
    primary_key_index: usize,
    layout: RowLayout,
}

impl TableSchema {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnSchema>) -> Result<Self, SchemaError> {
        let name = name.into();
        validate_identifier("table", &name)?;
        if columns.is_empty() {
            return Err(SchemaError::NoColumns);
        }
        if columns.len() > MAX_COLUMNS {
            return Err(SchemaError::TooManyColumns {
                count: columns.len(),
                maximum: MAX_COLUMNS,
            });
        }

        let mut names = HashSet::with_capacity(columns.len());
        let mut primary_key_index = None;
        for (index, column) in columns.iter().enumerate() {
            validate_identifier("column", &column.name)?;
            if !names.insert(column.name.as_str()) {
                return Err(SchemaError::DuplicateColumn(column.name.clone()));
            }
            if column.primary_key {
                if primary_key_index.replace(index).is_some() {
                    return Err(SchemaError::MultiplePrimaryKeys);
                }
                if column.nullable {
                    return Err(SchemaError::NullablePrimaryKey(column.name.clone()));
                }
            }
        }
        let primary_key_index = primary_key_index.ok_or(SchemaError::MissingPrimaryKey)?;
        let layout = RowLayout::compile(&columns);
        if layout.minimum_row_size() > MAX_ROW_SIZE {
            return Err(SchemaError::SchemaTooWide {
                minimum_row_size: layout.minimum_row_size(),
                maximum: MAX_ROW_SIZE,
            });
        }

        Ok(Self {
            name,
            columns,
            primary_key_index,
            layout,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    pub fn column(&self, index: usize) -> Option<&ColumnSchema> {
        self.columns.get(index)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    pub const fn primary_key_index(&self) -> usize {
        self.primary_key_index
    }

    pub fn primary_key(&self) -> &ColumnSchema {
        &self.columns[self.primary_key_index]
    }

    pub const fn layout(&self) -> &RowLayout {
        &self.layout
    }

    pub fn encode_row(&self, values: &[Value]) -> Result<Vec<u8>, RowError> {
        self.layout.encode(&self.columns, values)
    }

    pub fn decode_row<'row>(&self, row: &'row [u8]) -> Result<Vec<ValueRef<'row>>, RowError> {
        self.layout.decode(&self.columns, row)
    }

    pub fn project_row<'row>(
        &self,
        row: &'row [u8],
        columns: &[usize],
    ) -> Result<Vec<ValueRef<'row>>, RowError> {
        self.layout.project(&self.columns, row, columns)
    }

    pub fn primary_key_value<'row>(&self, row: &'row [u8]) -> Result<ValueRef<'row>, RowError> {
        self.layout
            .decode_column(&self.columns, row, self.primary_key_index)
    }

    /// Encodes only schema declarations. The derived row layout is recomputed
    /// on decode and is never trusted as persisted metadata.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SCHEMA_MAGIC);
        bytes.extend_from_slice(&SCHEMA_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(self.columns.len() as u16).to_le_bytes());
        write_string(&mut bytes, &self.name);
        for column in &self.columns {
            write_string(&mut bytes, &column.name);
            bytes.push(column.data_type.tag());
            let mut flags = 0;
            if column.nullable {
                flags |= FLAG_NULLABLE;
            }
            if column.primary_key {
                flags |= FLAG_PRIMARY_KEY;
            }
            bytes.push(flags);
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut cursor = Cursor::new(bytes);
        if cursor.take(SCHEMA_MAGIC.len())? != SCHEMA_MAGIC {
            return Err(SchemaError::CorruptEncoding("invalid schema magic"));
        }
        if cursor.read_u16()? != SCHEMA_FORMAT_VERSION {
            return Err(SchemaError::CorruptEncoding(
                "unsupported schema format version",
            ));
        }
        let column_count = usize::from(cursor.read_u16()?);
        if column_count == 0 || column_count > MAX_COLUMNS {
            return Err(SchemaError::CorruptEncoding("invalid schema column count"));
        }
        let name = cursor.read_string()?;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            let column_name = cursor.read_string()?;
            let data_type = DataType::from_tag(cursor.read_u8()?)?;
            let flags = cursor.read_u8()?;
            if flags & !KNOWN_COLUMN_FLAGS != 0 {
                return Err(SchemaError::CorruptEncoding("unknown column flags"));
            }
            columns.push(ColumnSchema {
                name: column_name,
                data_type,
                nullable: flags & FLAG_NULLABLE != 0,
                primary_key: flags & FLAG_PRIMARY_KEY != 0,
            });
        }
        if !cursor.is_empty() {
            return Err(SchemaError::CorruptEncoding(
                "trailing bytes after schema declaration",
            ));
        }
        Self::new(name, columns)
    }
}

fn validate_identifier(kind: &'static str, identifier: &str) -> Result<(), SchemaError> {
    let bytes = identifier.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_IDENTIFIER_BYTES {
        return Err(SchemaError::InvalidIdentifier {
            kind,
            identifier: identifier.to_owned(),
        });
    }
    let mut chars = identifier.chars();
    let Some(first) = chars.next() else {
        return Err(SchemaError::InvalidIdentifier {
            kind,
            identifier: identifier.to_owned(),
        });
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(SchemaError::InvalidIdentifier {
            kind,
            identifier: identifier.to_owned(),
        });
    }
    Ok(())
}

fn write_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], SchemaError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(SchemaError::CorruptEncoding("truncated schema encoding"))?;
        let result = &self.bytes[self.position..end];
        self.position = end;
        Ok(result)
    }

    fn read_u8(&mut self) -> Result<u8, SchemaError> {
        Ok(self.take(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, SchemaError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_string(&mut self) -> Result<String, SchemaError> {
        let length = usize::from(self.read_u16()?);
        let bytes = self.take(length)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| SchemaError::CorruptEncoding("schema identifier is not UTF-8"))?;
        Ok(value.to_owned())
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaError {
    InvalidIdentifier {
        kind: &'static str,
        identifier: String,
    },
    NoColumns,
    TooManyColumns {
        count: usize,
        maximum: usize,
    },
    DuplicateColumn(String),
    MissingPrimaryKey,
    MultiplePrimaryKeys,
    NullablePrimaryKey(String),
    SchemaTooWide {
        minimum_row_size: usize,
        maximum: usize,
    },
    CorruptEncoding(&'static str),
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier { kind, identifier } => {
                write!(formatter, "invalid {kind} identifier: {identifier:?}")
            }
            Self::NoColumns => write!(formatter, "table must contain at least one column"),
            Self::TooManyColumns { count, maximum } => {
                write!(formatter, "table has {count} columns; maximum is {maximum}")
            }
            Self::DuplicateColumn(name) => write!(formatter, "duplicate column: {name}"),
            Self::MissingPrimaryKey => write!(formatter, "table must have exactly one primary key"),
            Self::MultiplePrimaryKeys => {
                write!(formatter, "table cannot have more than one primary key")
            }
            Self::NullablePrimaryKey(name) => {
                write!(formatter, "primary-key column {name} cannot be nullable")
            }
            Self::SchemaTooWide {
                minimum_row_size,
                maximum,
            } => write!(
                formatter,
                "schema requires at least {minimum_row_size} bytes per row; maximum is {maximum}"
            ),
            Self::CorruptEncoding(reason) => write!(formatter, "invalid schema encoding: {reason}"),
        }
    }
}

impl Error for SchemaError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_schema() -> TableSchema {
        TableSchema::new(
            "users",
            vec![
                ColumnSchema::primary_key("id", DataType::Int),
                ColumnSchema::required("name", DataType::Text),
                ColumnSchema::nullable("email", DataType::Text),
            ],
        )
        .unwrap()
    }

    #[test]
    fn requires_exactly_one_non_null_primary_key() {
        assert!(matches!(
            TableSchema::new("users", vec![ColumnSchema::required("id", DataType::Int)]),
            Err(SchemaError::MissingPrimaryKey)
        ));
        assert!(matches!(
            TableSchema::new(
                "users",
                vec![
                    ColumnSchema::primary_key("id", DataType::Int),
                    ColumnSchema::primary_key("email", DataType::Text),
                ]
            ),
            Err(SchemaError::MultiplePrimaryKeys)
        ));
    }

    #[test]
    fn rejects_duplicate_and_invalid_identifiers() {
        assert!(matches!(
            TableSchema::new(
                "users",
                vec![
                    ColumnSchema::primary_key("id", DataType::Int),
                    ColumnSchema::required("id", DataType::Text),
                ]
            ),
            Err(SchemaError::DuplicateColumn(name)) if name == "id"
        ));
        assert!(matches!(
            TableSchema::new(
                "not valid",
                vec![ColumnSchema::primary_key("id", DataType::Int)]
            ),
            Err(SchemaError::InvalidIdentifier { kind: "table", .. })
        ));
    }

    #[test]
    fn schema_encoding_round_trips_and_recompiles_layout() {
        let schema = valid_schema();
        let decoded = TableSchema::decode(&schema.encode()).unwrap();
        assert_eq!(decoded, schema);
        assert_eq!(decoded.primary_key().name(), "id");
        assert_eq!(decoded.column_index("email"), Some(2));
    }

    #[test]
    fn rejects_truncated_and_unknown_schema_encodings() {
        let schema = valid_schema();
        let encoded = schema.encode();
        assert!(matches!(
            TableSchema::decode(&encoded[..encoded.len() - 1]),
            Err(SchemaError::CorruptEncoding(_))
        ));

        let mut unknown_type = encoded;
        let first_type_offset = 8 + 2 + "users".len() + 2 + "id".len();
        unknown_type[first_type_offset] = 255;
        assert!(matches!(
            TableSchema::decode(&unknown_type),
            Err(SchemaError::CorruptEncoding("unknown column type tag"))
        ));
    }
}
