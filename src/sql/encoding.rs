use crate::catalog::DataType;
use crate::error::{Result, TitanError};

/// Runtime row value. Distinct from `sqlparser::ast::Value` (parse-time).
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    Text(String),
    Bytes(Vec<u8>),
}

const TAG_NULL: u8 = 0;
const TAG_INT: u8 = 1;
const TAG_FLOAT: u8 = 2;
const TAG_BOOL: u8 = 3;
const TAG_TEXT: u8 = 4;
const TAG_BYTES: u8 = 5;
const TAG_DATE: u8 = 6; // stored as YYYY-MM-DD text payload

/// Encode a row: `col_count u32 BE` + per-col `(tag u8 + len u32 BE + payload)`.
pub fn encode_row(values: &[SqlValue]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + values.len() * 16);
    out.extend_from_slice(&(values.len() as u32).to_be_bytes());
    for v in values {
        match v {
            SqlValue::Null => {
                out.push(TAG_NULL);
                out.extend_from_slice(&0u32.to_be_bytes());
            }
            SqlValue::Int64(n) => {
                out.push(TAG_INT);
                out.extend_from_slice(&8u32.to_be_bytes());
                out.extend_from_slice(&n.to_be_bytes());
            }
            SqlValue::Float64(f) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&8u32.to_be_bytes());
                out.extend_from_slice(&f.to_be_bytes());
            }
            SqlValue::Bool(b) => {
                out.push(TAG_BOOL);
                out.extend_from_slice(&1u32.to_be_bytes());
                out.push(u8::from(*b));
            }
            SqlValue::Text(s) => {
                out.push(TAG_TEXT);
                out.extend_from_slice(&(s.len() as u32).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            SqlValue::Bytes(b) => {
                out.push(TAG_BYTES);
                out.extend_from_slice(&(b.len() as u32).to_be_bytes());
                out.extend_from_slice(b);
            }
        }
    }
    out
}

/// Decode a row, validating column count against the catalog schema.
/// Malformed bytes -> `CorruptPage`, never panic or silent NULL.
pub fn decode_row(bytes: &[u8], schema: &[DataType]) -> Result<Vec<SqlValue>> {
    let corrupt = |msg: String| TitanError::CorruptPage(0, msg);
    if bytes.len() < 4 {
        return Err(corrupt("row header truncated".to_string()));
    }
    let count = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if count != schema.len() {
        return Err(corrupt(format!(
            "column count mismatch: payload has {count}, schema has {}",
            schema.len()
        )));
    }
    let mut out = Vec::with_capacity(count);
    let mut pos = 4;
    for (i, dtype) in schema.iter().enumerate() {
        if pos + 5 > bytes.len() {
            return Err(corrupt(format!("column {i} header truncated")));
        }
        let tag = bytes[pos];
        pos += 1;
        let len = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        pos += 4;
        if pos + len > bytes.len() {
            return Err(corrupt(format!("column {i} payload truncated")));
        }
        let payload = &bytes[pos..pos + len];
        pos += len;
        let v = match tag {
            TAG_NULL => SqlValue::Null,
            TAG_INT => {
                if len != 8 {
                    return Err(corrupt(format!("column {i} bad int len")));
                }
                SqlValue::Int64(i64::from_be_bytes(payload.try_into().map_err(|_| {
                    corrupt(format!("column {i} bad int payload"))
                })?))
            }
            TAG_FLOAT => {
                if len != 8 {
                    return Err(corrupt(format!("column {i} bad float len")));
                }
                SqlValue::Float64(f64::from_be_bytes(payload.try_into().map_err(|_| {
                    corrupt(format!("column {i} bad float payload"))
                })?))
            }
            TAG_BOOL => {
                if len != 1 {
                    return Err(corrupt(format!("column {i} bad bool len")));
                }
                SqlValue::Bool(payload[0] != 0)
            }
            TAG_TEXT => SqlValue::Text(
                String::from_utf8(payload.to_vec())
                    .map_err(|_| corrupt(format!("column {i} bad utf8")))?,
            ),
            TAG_BYTES => SqlValue::Bytes(payload.to_vec()),
            TAG_DATE => {
                // Date arrives as text; accept only under Date/Text columns.
                let s = String::from_utf8(payload.to_vec())
                    .map_err(|_| corrupt(format!("column {i} bad date utf8")))?;
                match dtype {
                    DataType::Date => SqlValue::Text(s),
                    DataType::Text => SqlValue::Text(s),
                    _ => return Err(corrupt(format!("column {i} date under non-date type"))),
                }
            }
            other => return Err(corrupt(format!("column {i} unknown tag {other}"))),
        };
        // Type cross-check: Null is legal for any nullable column; tags must
        // agree with the catalog type (Text accepts Text only, etc.).
        let ok = match (&v, dtype) {
            (SqlValue::Null, _) => true,
            (SqlValue::Int64(_), DataType::Integer) => true,
            (SqlValue::Float64(_), DataType::Float) => true,
            (SqlValue::Bool(_), DataType::Boolean) => true,
            (SqlValue::Text(_), DataType::Text) => true,
            (SqlValue::Text(_), DataType::Date) => true,
            (SqlValue::Bytes(_), DataType::Bytes) => true,
            _ => false,
        };
        if !ok {
            return Err(corrupt(format!("column {i} tag/type mismatch")));
        }
        out.push(v);
    }
    if pos != bytes.len() {
        return Err(corrupt("trailing bytes after row".to_string()));
    }
    Ok(out)
}

/// Display rendering for `ResultSet` (strings). NULL renders as `NULL`.
pub fn value_to_display(v: &SqlValue) -> String {
    match v {
        SqlValue::Null => "NULL".to_string(),
        SqlValue::Int64(n) => n.to_string(),
        SqlValue::Float64(f) => f.to_string(),
        SqlValue::Bool(b) => b.to_string(),
        SqlValue::Text(s) => s.clone(),
        SqlValue::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// Parse a SQL literal (`sqlparser` display form) into a typed value using the
/// column type. `NULL` (any case) -> `SqlValue::Null`.
pub fn parse_literal(lit: &str, dtype: &DataType) -> SqlValue {
    if lit.eq_ignore_ascii_case("null") {
        return SqlValue::Null;
    }
    match dtype {
        DataType::Integer => lit.parse::<i64>().map(SqlValue::Int64).unwrap_or(SqlValue::Text(lit.to_string())),
        DataType::Float => lit.parse::<f64>().map(SqlValue::Float64).unwrap_or(SqlValue::Null),
        DataType::Boolean => match lit.to_ascii_lowercase().as_str() {
            "true" => SqlValue::Bool(true),
            "false" => SqlValue::Bool(false),
            _ => SqlValue::Null,
        },
        DataType::Text | DataType::Date => SqlValue::Text(lit.to_string()),
        DataType::Bytes => SqlValue::Bytes(lit.as_bytes().to_vec()),
    }
}
