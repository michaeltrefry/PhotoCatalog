//! Lexical prefix equivalence only; never resolve filesystem aliases or rewrite
//! retained source evidence. Registration still checks the exact stored locator.
use crate::storage_volume::NativePath;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension};

pub(super) fn existing(db: &Connection, path: &NativePath) -> Result<Option<(String, NativePath)>> {
    let alternate = alternate(path);
    let mut found = None;
    for candidate in std::iter::once(path).chain(alternate.as_ref()) {
        let id: Option<String> = db
            .query_row(
                "SELECT id FROM assets WHERE location=?",
                [crate::catalog_storage::encoded_bytes(candidate)],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            ensure!(
                found.is_none(),
                "Ambiguous Windows original path: both plain and verbatim locators already exist; resolve the overlap before retrying migration"
            );
            found = Some((id, candidate.clone()));
        }
    }
    Ok(found)
}

fn alternate(path: &NativePath) -> Option<NativePath> {
    let NativePath::WindowsWide(units) = path else {
        return None;
    };
    if units.len() > 32768 {
        return None;
    }
    let verbatim = units.starts_with(&[92, 92, 63, 92]);
    let body = if verbatim {
        &units[4..]
    } else {
        units.as_slice()
    };
    let unc = if verbatim {
        body.starts_with(&[85, 78, 67, 92])
    } else {
        body.starts_with(&[92, 92])
    };
    let tail = if unc {
        &body[if verbatim { 4 } else { 2 }..]
    } else {
        if body.len() < 4 || !matches!(body[0], 65..=90 | 97..=122) || body[1..3] != [58, 92] {
            return None;
        }
        &body[3..]
    };
    let components: Vec<_> = tail.split(|u| *u == 92).collect();
    if (unc && components.len() < 3) || !components.iter().all(|c| safe_component(c)) {
        return None;
    }
    let mut result = if verbatim {
        if unc { vec![92, 92] } else { Vec::new() }
    } else if unc {
        vec![92, 92, 63, 92, 85, 78, 67, 92]
    } else {
        vec![92, 92, 63, 92]
    };
    result.extend_from_slice(if unc { tail } else { body });
    (result.len() <= 32768).then_some(NativePath::WindowsWide(result))
}

fn safe_component(c: &[u16]) -> bool {
    if c.is_empty()
        || matches!(c.last(), Some(32 | 46))
        || c.iter()
            .any(|u| matches!(u, 0..=31 | 34 | 42 | 47 | 58 | 60 | 62 | 63 | 124))
    {
        return false;
    }
    // DOS devices have different meaning in ordinary and verbatim namespaces.
    let stem: Vec<_> = c
        .iter()
        .take_while(|u| **u != 46)
        .map(|u| if matches!(u, 97..=122) { u - 32 } else { *u })
        .collect();
    !matches!(stem.last(), Some(32))
        && !matches!(
            stem.as_slice(),
            [67, 79, 78]
                | [80, 82, 78]
                | [65, 85, 88]
                | [78, 85, 76]
                | [67, 79, 78, 73, 78, 36]
                | [67, 79, 78, 79, 85, 84, 36]
                | [67, 79, 77, 49..=57 | 178 | 179 | 185]
                | [76, 80, 84, 49..=57 | 178 | 179 | 185]
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn native(s: &str) -> NativePath {
        NativePath::WindowsWide(s.encode_utf16().collect())
    }
    #[test]
    fn only_strict_drive_and_unc_prefixes_are_equivalent() {
        for (plain, verbatim) in [
            (r"C:\Photos\é.jpg", r"\\?\C:\Photos\é.jpg"),
            (
                r"\\server\share\photo.jpg",
                r"\\?\UNC\server\share\photo.jpg",
            ),
        ] {
            assert_eq!(alternate(&native(plain)), Some(native(verbatim)));
            assert_eq!(alternate(&native(verbatim)), Some(native(plain)));
        }
        for invalid in [
            r"C:photo.jpg",
            r"C:/Photos/a.jpg",
            r"C:\Photos\..\a.jpg",
            r"C:\Photos\.\a.jpg",
            r"C:\Photos\\a.jpg",
            r"C:\Photos \a.jpg",
            r"C:\Photos.\a.jpg",
            r"C:\NUL.jpg",
            r"C:\com1\a.jpg",
            r"\\.\C:\a.jpg",
            r"\\?\GLOBALROOT\Device\a.jpg",
            r"\\?\Volume{abc}\a.jpg",
            r"\\?\unc\server\share\a.jpg",
            r"\\server\share",
            r"C:\a.jpg:stream",
        ] {
            assert_eq!(alternate(&native(invalid)), None, "{invalid}");
        }
        assert_eq!(
            alternate(&NativePath::UnixBytes(b"C:\\a.jpg".to_vec())),
            None
        );
    }
    #[test]
    fn lookup_is_unique_and_preserves_exact_stored_spelling() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE assets(id TEXT,location BLOB UNIQUE)")?;
        for (plain, verbatim) in [
            (r"C:\Photos\a.jpg", r"\\?\C:\Photos\a.jpg"),
            (r"\\server\share\a.jpg", r"\\?\UNC\server\share\a.jpg"),
        ] {
            db.execute("DELETE FROM assets", [])?;
            db.execute(
                "INSERT INTO assets VALUES('original',?)",
                [crate::catalog_storage::encoded_bytes(&native(verbatim))],
            )?;
            assert_eq!(
                existing(&db, &native(plain))?,
                Some(("original".into(), native(verbatim)))
            );
            assert_eq!(
                existing(
                    &db,
                    &native(
                        &plain
                            .replace("Photos", "photos")
                            .replace("server", "SERVER")
                    )
                )?,
                None
            );
            db.execute(
                "INSERT INTO assets VALUES('duplicate',?)",
                [crate::catalog_storage::encoded_bytes(&native(plain))],
            )?;
            assert!(
                existing(&db, &native(plain))
                    .unwrap_err()
                    .to_string()
                    .contains("Ambiguous")
            );
            assert!(existing(&db, &native(verbatim)).is_err());
        }
        Ok(())
    }
}
