//! Borrowed SQLite guards for bounded preview construction. Check storage class
//! and byte length before making owned strings or decoding structured values.
use rusqlite::{Row, types::ValueRef};

fn rejected(index: usize, value: ValueRef<'_>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        value.data_type(),
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stored preview field exceeds its byte bound or has invalid storage type",
        )),
    )
}
pub(crate) fn text<'a>(
    row: &'a Row<'_>,
    index: usize,
    maximum: usize,
) -> rusqlite::Result<&'a str> {
    let value = row.get_ref(index)?;
    let ValueRef::Text(bytes) = value else {
        return Err(rejected(index, value));
    };
    if bytes.len() > maximum {
        return Err(rejected(index, value));
    }
    std::str::from_utf8(bytes).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), Box::new(error))
    })
}
pub(crate) fn owned_text(row: &Row<'_>, index: usize, maximum: usize) -> rusqlite::Result<String> {
    Ok(text(row, index, maximum)?.to_owned())
}
pub(crate) fn optional_text(
    row: &Row<'_>,
    index: usize,
    maximum: usize,
) -> rusqlite::Result<Option<String>> {
    if matches!(row.get_ref(index)?, ValueRef::Null) {
        Ok(None)
    } else {
        owned_text(row, index, maximum).map(Some)
    }
}
pub(crate) fn blob<'a>(
    row: &'a Row<'_>,
    index: usize,
    maximum: usize,
) -> rusqlite::Result<&'a [u8]> {
    let value = row.get_ref(index)?;
    match value {
        ValueRef::Blob(bytes) if bytes.len() <= maximum => Ok(bytes),
        _ => Err(rejected(index, value)),
    }
}

#[cfg(test)]
mod tests;
