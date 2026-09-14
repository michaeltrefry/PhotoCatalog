use std::{cell::Cell, rc::Rc};
use xmp_toolkit::{ToStringOptions, XmpError, XmpMeta, XmpValue};

#[derive(Debug, PartialEq)]
enum Failure {
    Admission,
    Native(XmpError),
}
impl From<XmpError> for Failure {
    fn from(value: XmpError) -> Self {
        Self::Native(value)
    }
}
struct Grant(Rc<Cell<bool>>);
impl Drop for Grant {
    fn drop(&mut self) {
        self.0.set(false);
    }
}
fn canonical_options() -> ToStringOptions {
    ToStringOptions::default()
        .omit_packet_wrapper()
        .use_canonical_format()
        .omit_all_formatting()
}

#[test]
fn admitted_sdk_serialization_keeps_guard_and_preserves_canonical_text() -> anyhow::Result<()> {
    let mut meta = XmpMeta::new()?;
    meta.set_property(
        photocatalog::xmp::XMP,
        "Label",
        &XmpValue::new("café 📷 <&>".to_owned()),
    )?;
    let expected = meta.to_string_with_options(canonical_options())?;
    let live = Rc::new(Cell::new(false));
    let calls = Cell::new(0);
    let text = meta.to_string_with_options_admitted(canonical_options(), |bytes| {
        calls.set(calls.get() + 1);
        assert_eq!(bytes, expected.len());
        live.set(true);
        Ok::<_, XmpError>(Grant(live.clone()))
    })?;
    assert_eq!(calls.get(), 1);
    assert!(live.get());
    assert_eq!(text.as_ref(), expected);
    assert_eq!(text.len(), expected.len());
    let moved = text;
    assert!(live.get());
    drop(moved);
    assert!(!live.get());
    Ok(())
}

#[test]
fn admitted_sdk_serialization_preserves_callback_and_native_errors() -> anyhow::Result<()> {
    let meta = XmpMeta::new()?;
    let calls = Cell::new(0);
    match meta.to_string_with_options_admitted(canonical_options(), |_| {
        calls.set(calls.get() + 1);
        Err::<(), _>(Failure::Admission)
    }) {
        Err(error) => assert_eq!(error, Failure::Admission),
        Ok(_) => anyhow::bail!("refused admission produced output"),
    }
    assert_eq!(calls.get(), 1);
    let invalid = || {
        ToStringOptions::default()
            .exact_packet_length()
            .set_padding(1)
    };
    let native = meta.to_string_with_options(invalid()).unwrap_err();
    match meta.to_string_with_options_admitted(invalid(), |_| {
        calls.set(calls.get() + 1);
        Ok::<_, Failure>(())
    }) {
        Err(error) => assert_eq!(error, Failure::Native(native)),
        Ok(_) => anyhow::bail!("invalid native serialization produced output"),
    }
    assert_eq!(calls.get(), 1, "native error must precede admission");
    let retry =
        meta.to_string_with_options_admitted(canonical_options(), |_| Ok::<_, XmpError>(()))?;
    assert!(!retry.is_empty());
    Ok(())
}
