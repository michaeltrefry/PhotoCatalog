use super::*;
fn input(bytes: &[u8], format: Format) -> Input {
    Input {
        source_id: "capture:row-id".into(),
        revision: "revision".into(),
        locator: "table/row/field".into(),
        payload_blake3: blake3::hash(bytes).to_hex().to_string(),
        payload_bytes: bytes.len() as u64,
        format,
        source_kind: SourceKind::Raw,
        association: Association::Current,
        settings_path: vec![],
        as_shot_available: false,
    }
}
fn catalog(s: &str) -> Extraction {
    extract(
        s.as_bytes(),
        input(s.as_bytes(), Format::CatalogData),
        Limits::default(),
    )
    .unwrap()
}
fn prop<'a>(r: &'a Extraction, name: &str) -> &'a Property {
    r.properties
        .iter()
        .find(|p| p.name == name && eligible(p, &r.input))
        .unwrap()
}
fn xml_input(attrs: &str, children: &str) -> Vec<u8> {
    format!(r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:c="{CRS}" xmlns:u="urn:unknown" {attrs}>{children}</rdf:Description></rdf:RDF></x:xmpmeta>"#).into_bytes()
}
fn xml_case(attrs: &str, children: &str) -> Extraction {
    let b = xml_input(attrs, children);
    extract(&b, input(&b, Format::CrsXml), Limits::default()).unwrap()
}
#[test]
fn all_basic_fields_accounted_without_invented_defaults() {
    let r = catalog("{}");
    assert!(r.failure.is_none());
    assert_eq!(r.missing.len(), BASIC_PROPERTIES.len());
    assert_eq!(r.contribution, RecipeContribution::default());
    assert!(!r.adobe_rendering_equivalent);
    let body = BASIC_PROPERTIES
        .iter()
        .map(|n| format!("{n} = nil"))
        .collect::<Vec<_>>()
        .join(",");
    let r = catalog(&format!("{{{body}}}"));
    assert!(r.failure.is_none());
    assert!(r.missing.is_empty());
    assert_eq!(r.contribution, RecipeContribution::default());
}
#[test]
fn ev_units_and_sparse_application() {
    for (ev, gain) in [(-2., 0.25), (-1., 0.5), (0., 1.), (1., 2.), (2., 4.)] {
        let r = catalog(&format!("{{ProcessVersion='11.0',Exposure2012={ev}}}"));
        assert!(r.failure.is_none());
        assert_eq!(r.contribution.exposure_ev, Some(ev));
        assert_eq!(r.contribution.exposure_ev.unwrap().exp2(), gain);
        let target = crate::edit::RecipeV1 {
            contrast: 0.75,
            shadows: -0.25,
            ..Default::default()
        };
        let applied = r
            .contribution
            .apply_to(&Recipe::V1(target.clone()))
            .unwrap();
        assert_eq!(applied.settings().contrast, target.contrast);
        assert_eq!(applied.settings().shadows, target.shadows);
        assert_eq!(applied.settings().exposure_ev, ev);
    }
}
#[test]
fn process_current_state_auto_and_switches_are_required() {
    for fields in [
        "ProcessVersion='6.7',Exposure2012=1",
        "Exposure2012=1",
        "ProcessVersion=11,Exposure2012=1",
        "ProcessVersion='11.0',Exposure2012=1,Exposure=1",
        "ProcessVersion='11.0',Exposure2012=1,AutoExposure=true",
        "ProcessVersion='11.0',Exposure2012=1,AutoExposure='invalid'",
        "ProcessVersion='11.0',Exposure2012=1,HasSettings=false",
        "ProcessVersion='11.0',Exposure2012=1,EnableBasicAdjustments=false",
    ] {
        assert_eq!(
            catalog(&format!("{{{fields}}}")).contribution,
            RecipeContribution::default(),
            "{fields}"
        );
    }
    for association in [Association::Historical, Association::Unresolved] {
        let b = b"{ProcessVersion='11.0',Exposure2012=1}";
        let mut i = input(b, Format::CatalogData);
        i.association = association;
        assert_eq!(
            extract(b, i, Limits::default()).unwrap().contribution,
            RecipeContribution::default()
        );
    }
}
#[test]
fn raw_custom_wb_uses_existing_adobe_sdk_numeric_bridge() {
    for k in [2000, 2850, 5500, 6500, 50000] {
        for tint in [-150., 0., 150.] {
            let r = catalog(&format!(
                "{{ProcessVersion='11.0',WhiteBalance='Custom',Temperature={k},Tint={tint}}}"
            ));
            let wb = WhiteBalance::TemperatureTint { kelvin: k, tint };
            match crate::edit::color::white_point(&wb) {
                Ok(Some(xy)) => {
                    assert_eq!(r.contribution.white_balance, Some(wb));
                    assert_eq!(r.contribution.white_point_xy, Some(xy));
                    assert!(xy.into_iter().all(f64::is_finite));
                }
                _ => assert!(r.contribution.white_balance.is_none()),
            }
        }
    }
    for fields in [
        "Temperature=1999,Tint=0",
        "Temperature=50001,Tint=0",
        "Temperature=5500.5,Tint=0",
        "Temperature=5500,Tint=151",
        "Temperature=5500",
        "Temperature=5500,Tint=0,IncrementalTint=0",
    ] {
        assert!(
            catalog(&format!(
                "{{ProcessVersion='11.0',WhiteBalance='Custom',{fields}}}"
            ))
            .contribution
            .white_balance
            .is_none()
        );
    }
}
#[test]
fn wb_source_type_and_as_shot_availability_not_guessed() {
    let b = b"{ProcessVersion='11.0',WhiteBalance='Custom',Temperature=5500,Tint=10}";
    for kind in [SourceKind::Raster, SourceKind::Unknown] {
        let mut i = input(b, Format::CatalogData);
        i.source_kind = kind;
        assert!(
            extract(b, i, Limits::default())
                .unwrap()
                .contribution
                .white_balance
                .is_none()
        );
    }
    let b = b"{ProcessVersion='11.0',WhiteBalance='As Shot'}";
    let mut i = input(b, Format::CatalogData);
    assert!(
        extract(b, i.clone(), Limits::default())
            .unwrap()
            .contribution
            .white_balance
            .is_none()
    );
    i.as_shot_available = true;
    assert_eq!(
        extract(b, i, Limits::default())
            .unwrap()
            .contribution
            .white_balance,
        Some(WhiteBalance::AsShot)
    );
    for mode in ["Auto", "Daylight", "unknown"] {
        assert!(
            catalog(&format!(
                "{{ProcessVersion='11.0',WhiteBalance='{mode}',Temperature=5500,Tint=0}}"
            ))
            .contribution
            .white_balance
            .is_none()
        );
    }
}
#[test]
fn nonzero_native_controls_retained_without_arbitrary_normalization() {
    let r = catalog(
        "{ProcessVersion='11.0',HasCrop=true,CropLeft=.1,CropTop=.2,CropRight=.8,CropBottom=.9,CropAngle=3,Contrast2012=50,Highlights2012=-20,Shadows2012=25,Saturation=30,Vibrance=50,Sharpness=100,SharpenRadius=1.5,LuminanceSmoothing=20,ColorNoiseReduction=30}",
    );
    assert!(r.failure.is_none());
    assert_eq!(r.contribution, RecipeContribution::default());
    for p in &r.properties {
        assert_ne!(p.disposition, Disposition::MappedParameter);
    }
    assert!(prop(&r, "CropAngle").reason.contains("coordinate"));
}
#[test]
fn catalog_nested_wrappers_unknown_values_and_lexical_addresses_retained() {
    let s = "s = { ProcessVersion='11.0', Exposure2012=+1.25e0, unknown={ [1]='hello\\nworld', ['unicode']='café', false, } }";
    // Explicit index1 and implicit1 collide, matching table semantics rather than choosing a winner.
    assert_eq!(catalog(s).failure.unwrap().kind, FailureKind::Conflict);
    let s = "s = { ProcessVersion='11.0', Exposure2012=+1.25e0, unknown={ [3]='hello\\nworld', ['unicode']='café', false, } }";
    let mut i = input(s.as_bytes(), Format::CatalogData);
    i.settings_path = vec![Key::Name("s".into())];
    let r = extract(s.as_bytes(), i, Limits::default()).unwrap();
    assert_eq!(r.contribution.exposure_ev, Some(1.25));
    assert_eq!(prop(&r, "Exposure2012").lexical, "+1.25e0");
    for p in &r.properties {
        assert_eq!(&s[p.start..p.end], p.lexical);
    }
    assert!(r.properties.iter().any(|p| p.path
        == vec![
            Key::Name("s".into()),
            Key::Name("unknown".into()),
            Key::Index(3)
        ]
        && p.value == Value::Text("hello\nworld".into())));
    assert!(
        catalog(s).contribution.exposure_ev.is_none(),
        "wrapper not guessed"
    );
}
#[test]
fn no_execution_partial_parse_or_duplicate_key_winner() {
    for s in [
        "return os.execute('never')",
        "{Exposure2012=1+1}",
        "{x=setmetatable({}, {})}",
        "{x=function()end}",
        "{x=a.b}",
        "{x=1} os.execute('never')",
        "{x='unfinished}",
        "{x=0/0}",
        "{x=0x1}",
        "{x=1e999}",
        "{x=1e-999}",
    ] {
        let r = catalog(s);
        assert!(r.failure.is_some(), "{s}");
        assert!(r.properties.is_empty() && r.missing.is_empty());
        assert_eq!(r.contribution, RecipeContribution::default());
    }
    for s in ["{x=1,x=2}", "{x=1,['x']=2}", "{[1]=1,2}"] {
        assert_eq!(catalog(s).failure.unwrap().kind, FailureKind::Conflict);
    }
}
#[test]
fn strings_and_comments_are_data_and_fully_consumed() {
    let r = catalog(
        "-- header\nreturn { x=[=[\ncall('only text')]=], y='a\\x62\\099', -- comment\n z='\\\"\\\\', flag=true, empty=nil }; -- end",
    );
    assert!(r.failure.is_none(), "{:?}", r.failure);
    assert_eq!(prop(&r, "x").value, Value::Text("call('only text')".into()));
    assert_eq!(prop(&r, "y").value, Value::Text("abc".into()));
    for s in [
        "{x='\\999'}",
        "{x='\\255'}",
        "{x='\\q'}",
        "{x=[=[unterminated]}",
    ] {
        assert!(catalog(s).failure.is_some());
    }
}
#[test]
fn xml_namespace_attributes_elements_encoding_and_unknown_structure() {
    let r = xml_case(
        "c:ProcessVersion='11.0' c:Exposure2012='+1.00' u:Exposure2012='4'",
        "<c:WhiteBalance>Custom</c:WhiteBalance><c:Temperature>5500</c:Temperature><c:Tint>0</c:Tint><u:future><rdf:Seq><rdf:li>opaque</rdf:li></rdf:Seq></u:future>",
    );
    assert!(r.failure.is_none(), "{:?}", r.failure);
    assert_eq!(r.contribution.exposure_ev, Some(1.));
    assert!(r.contribution.white_balance.is_some());
    assert!(
        r.properties
            .iter()
            .any(|p| p.namespace.as_deref() == Some("urn:unknown") && p.name == "future")
    );
    assert_eq!(
        r.properties
            .iter()
            .filter(|p| p.disposition == Disposition::MappedParameter && p.name == "Exposure2012")
            .count(),
        1
    );
    let b = xml_input(
        "c:ProcessVersion='11.0'",
        "<c:Exposure2012>2<!-- ignored -->.0</c:Exposure2012>",
    );
    let text = std::str::from_utf8(&b).unwrap();
    let mut utf16 = vec![0xff, 0xfe];
    for u in text.encode_utf16() {
        utf16.extend(u.to_le_bytes());
    }
    let r = extract(&utf16, input(&utf16, Format::CrsXml), Limits::default()).unwrap();
    assert!(r.failure.is_none(), "{:?}", r.failure);
    assert_eq!(r.contribution.exposure_ev, Some(2.));
    assert_eq!(r.coordinate_space, "decoded_xml_utf8");
}
#[test]
fn xml_structured_numeric_or_bad_domain_never_maps() {
    let r = xml_case("c:ProcessVersion='11.0' c:Exposure2012='5.01'", "");
    assert_eq!(prop(&r, "Exposure2012").disposition, Disposition::Invalid);
    assert!(r.contribution.exposure_ev.is_none());
    let r = xml_case(
        "c:ProcessVersion='11.0'",
        "<c:Exposure2012><rdf:Seq><rdf:li>1</rdf:li></rdf:Seq></c:Exposure2012>",
    );
    assert!(r.contribution.exposure_ev.is_none());
    let b = b"<!DOCTYPE x [<!ENTITY x SYSTEM 'file:///never'>]><x/>";
    assert!(
        extract(b, input(b, Format::CrsXml), Limits::default())
            .unwrap()
            .failure
            .is_some()
    );
}
#[test]
fn bounds_identity_and_retained_errors() {
    let b = b"{ProcessVersion='11.0',Exposure2012=1}";
    let mut i = input(b, Format::CatalogData);
    i.payload_blake3 = "0".repeat(64);
    assert!(extract(b, i, Limits::default()).is_err());
    for limits in [
        Limits {
            bytes: 10,
            ..Limits::default()
        },
        Limits {
            properties: 1,
            ..Limits::default()
        },
        Limits {
            tokens: 2,
            ..Limits::default()
        },
        Limits {
            string_bytes: 2,
            ..Limits::default()
        },
        Limits {
            output_bytes: 10,
            ..Limits::default()
        },
    ] {
        let r = extract(b, input(b, Format::CatalogData), limits).unwrap();
        assert_eq!(r.failure.unwrap().kind, FailureKind::ResourceLimit);
        assert!(r.properties.is_empty() && r.missing.is_empty());
    }
    let deep = "{x=".repeat(8) + "0" + &"}".repeat(8);
    assert_eq!(
        extract(
            deep.as_bytes(),
            input(deep.as_bytes(), Format::CatalogData),
            Limits {
                depth: 3,
                ..Limits::default()
            }
        )
        .unwrap()
        .failure
        .unwrap()
        .kind,
        FailureKind::ResourceLimit
    );
}

#[test]
fn f32_underflow_does_not_invent_neutral_and_raster_units_remain_unknown() {
    let r = catalog(
        "{ProcessVersion='11.0',Exposure2012=1e-50,WhiteBalance='Custom',Temperature=5500,Tint=1e-50}",
    );
    assert!(r.failure.is_none());
    assert_eq!(r.contribution, RecipeContribution::default());
    assert_eq!(prop(&r, "Exposure2012").value, Value::Number(1e-50));
    let b = b"{ProcessVersion='11.0',WhiteBalance='Custom',Temperature=50,Tint=0}";
    let mut i = input(b, Format::CatalogData);
    i.source_kind = SourceKind::Raster;
    let r = extract(b, i, Limits::default()).unwrap();
    assert_eq!(
        prop(&r, "Temperature").disposition,
        Disposition::RetainedOnly
    );
    assert!(r.contribution.white_balance.is_none());
}
#[test]
fn duplicate_xml_is_conflict_and_unknown_source_never_applies() {
    let r = xml_case(
        "c:ProcessVersion='11.0' c:Exposure2012='1'",
        "<c:Exposure2012>2</c:Exposure2012>",
    );
    assert_eq!(r.failure.unwrap().kind, FailureKind::Conflict);
    assert_eq!(r.contribution, RecipeContribution::default());
    assert!(r.properties.is_empty());
    let b = b"{ProcessVersion='11.0',Exposure2012=1}";
    let mut i = input(b, Format::CatalogData);
    i.source_kind = SourceKind::Unknown;
    assert!(
        extract(b, i, Limits::default())
            .unwrap()
            .contribution
            .exposure_ev
            .is_none()
    );
}

#[test]
fn unqualified_crop_coordinates_not_rejected_using_native_units() {
    let r = catalog("{ProcessVersion='11.0',HasCrop=true,CropLeft=-.1,CropRight=1.2,CropAngle=90}");
    assert!(r.failure.is_none());
    for name in ["CropLeft", "CropRight", "CropAngle"] {
        assert_eq!(prop(&r, name).disposition, Disposition::RetainedOnly);
        assert!(prop(&r, name).reason.contains("coordinate"));
    }
    assert_eq!(r.contribution, RecipeContribution::default());
}
