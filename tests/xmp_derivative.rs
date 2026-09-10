use photocatalog::xmp::{self, DerivativeFields, Edit};
#[path = "../src/xmp_rdf.rs"]
mod xmp_rdf;

const TIFF: &str = "http://ns.adobe.com/tiff/1.0/";
const EXIF: &str = "http://ns.adobe.com/exif/1.0/";
const CRS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";

#[test]
fn derivative_preserves_subject_and_unknown_metadata_without_reapplying_develop() {
    let base = br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="urn:photo:original" xmlns:u="urn:opaque:" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:tiff="http://ns.adobe.com/tiff/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"><u:Record rdf:parseType="Resource"><u:Name>Snow</u:Name><u:Items><rdf:Seq><rdf:li>one</rdf:li><rdf:li>two</rdf:li></rdf:Seq></u:Items></u:Record><u:Value rdf:parseType="Resource"><rdf:value>kept</rdf:value><u:quality>precise</u:quality></u:Value><dc:title><rdf:Alt><rdf:li xml:lang="x-default">Snow</rdf:li><rdf:li xml:lang="fr">Neige</rdf:li></rdf:Alt></dc:title><crs:Exposure2012>2</crs:Exposure2012><tiff:Orientation>6</tiff:Orientation><tiff:ImageWidth>6000</tiff:ImageWidth></rdf:Description></rdf:RDF>"#;
    let before = base.to_vec();
    let fields = DerivativeFields {
        width: 1200,
        height: 800,
        channels: 4,
        bits_per_sample: 16,
        mime_type: "image/png".into(),
        profile_name: "sRGB".into(),
        is_srgb: true,
    };
    let result = xmp::rendered_derivative(base, &fields).unwrap();
    assert_eq!(base.as_slice(), before);
    let model = xmp::parse(&result).unwrap();
    assert_eq!(model.name(), "urn:photo:original");
    for (ns, key, value) in [
        (TIFF, "ImageWidth", "1200"),
        (TIFF, "ImageLength", "800"),
        (TIFF, "Orientation", "1"),
        (TIFF, "SamplesPerPixel", "4"),
        (EXIF, "ColorSpace", "1"),
    ] {
        assert_eq!(model.property(ns, key).unwrap().value, value);
    }
    assert!(model.property(CRS, "Exposure2012").is_none());
    for n in 1..=4 {
        assert_eq!(
            model
                .property(TIFF, &format!("BitsPerSample[{n}]"))
                .unwrap()
                .value,
            "16"
        );
    }
    assert!(model.property(TIFF, "BitsPerSample[5]").is_none());
    let remove = |ns: &str, path: &str| Edit::Remove {
        namespace: ns.into(),
        path: path.into(),
    };
    let old = xmp::apply_edits(
        base,
        &[
            remove(CRS, "Exposure2012"),
            remove(TIFF, "Orientation"),
            remove(TIFF, "ImageWidth"),
        ],
    )
    .unwrap();
    let mut removals = Vec::new();
    for (ns, paths) in [
        (
            TIFF,
            vec![
                "ImageWidth",
                "ImageLength",
                "Orientation",
                "SamplesPerPixel",
                "PhotometricInterpretation",
                "BitsPerSample",
            ],
        ),
        (
            EXIF,
            vec!["PixelXDimension", "PixelYDimension", "ColorSpace"],
        ),
        ("http://ns.adobe.com/photoshop/1.0/", vec!["ICCProfile"]),
        (xmp::DC, vec!["format"]),
        (xmp::XMP, vec!["CreatorTool"]),
    ] {
        for path in paths {
            removals.push(remove(ns, path));
        }
    }
    let stripped = xmp::apply_edits(&result, &removals).unwrap();
    xmp_rdf::assert_equivalent(
        &xmp::xml_text(&old).unwrap(),
        &xmp::xml_text(&stripped).unwrap(),
    )
    .unwrap();
}
