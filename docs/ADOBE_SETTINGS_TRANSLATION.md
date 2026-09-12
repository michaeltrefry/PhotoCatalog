# Bounded Adobe settings extraction

`lightroom::adobe::extract(bytes, input, limits)` interprets a retained source payload. It does not open files, choose current history, modify metadata or reproduce Adobe rendering. Input binds BLAKE3/length and the source adapter's source ID, revision, retained locator, source kind and Current/Historical/Unresolved association. The adapter must establish these associations; naming a record Current is not proof supplied by the parser.

Every parsed property has a typed value, address, exact lexical substring and offsets. XML offsets refer to decoded UTF-8, including when retained bytes were UTF-16/32; catalog offsets refer to original UTF-8. Unknown namespaces, nested values and adjustment dependencies remain addressable. Complete original payloads remain externally retained. A bounded parse or output failure returns the input reference, an explicit failure and no partial properties/missing list/recipe. Caller identity/digest mismatch is an error. For oversized streamed sources, the adapter can use `retained_failure` without loading the payload; that helper explicitly does not verify its digest or existence.

Default/hard maxima:4MiB input/decoded XML,10,000 properties,50,000 catalog tokens/XML nodes,32 data levels,64KiB decoded strings,8MiB serialized extraction. Callers may lower these. Parser failures never authorize discarding retained bytes. Output size is checked after bounded parse; these are data limits, not hard process-RSS enforcement.

## Data-only catalog grammar

The parser accepts a table literal, `return` followed by a table literal, or a single identifier assignment to a table. The latter is interpreted as an explicit wrapper address, not code. `Input.settings_path` selects a catalog table; root is `[]`, `s={...}` requires `[Name("s")]`. No wrapper or active history is guessed.

Tables contain identifier/string/positive-integer keys and implicit1-based entries, nested tables, decimal finite numbers, booleans, nil, UTF-8 quoted strings and Lua-style long strings. Comma/semicolon separators, line comments and long comments are accepted. Duplicate normalized keys are conflicts, even when one entry is nil. Numeric overflow/underflow, expressions, function calls, executable identifiers, trailing code and unsupported syntax fail the entire parse. Strings support standard single-byte escapes, decimal and hex byte escapes, escaped newlines and bounded long-bracket delimiters. Non-UTF8 decoded byte strings remain unparsed retained evidence. This is a deliberate data subset, not a general Lua engine.

CRS XML must pass the existing XMP SDK plus independent RDF preservation boundary. Names are identified by expanded namespace, not prefix. External RDF subjects require additional adapter association and are not translated. Duplicate top-level XML properties are conflicts; structured numeric values are not coerced to scalars.

## Mapping boundary

All agreed basic fields are listed in `BASIC_PROPERTIES`, including crop/angle, exposure, WB, tone/color, sharpening/noise and supporting flags. Explicit absence, invalid values, conflicts and retained-only properties are distinct. Absence never invents a neutral setting.

Only serialized process `11.0`, an explicitly Current record and a known Raw/Raster source kind are presently eligible. This serialized value is evidenced by Adobe's [official CRS sample](https://github.com/AdobeDocs/cis-photoshop-api-docs/blob/main/sample-code/lr-sample-app/crs.xml). Producer Version and UI process labels do not substitute. Disabled/invalid Enable switches or nontrue HasSettings prevent mappings. Other versions are extracted but retained without application.

* `Exposure2012` maps directly to EV within−5..5, without a concurrent legacy Exposure value or unresolved/true AutoExposure. Tests verify−2,−1,0,+1,+2 become gains0.25,0.5,1,2,4. [Adobe's exposure semantics](https://helpx.adobe.com/camera-raw/desktop/using/make-color-tonal-adjustments-camera.html) establish stop units, not matching highlight recovery or pixels.
* Raw Custom WB requires explicit integer Kelvin2000..50000 and finite tint−150..150, no incremental raster fields, and a valid existing Adobe DNG SDK white-point conversion. Source numbers are preserved; target tint/EV use the recipe's f32 representation. Raw As Shot requires the adapter's explicit availability proof and no unresolved temperature/tint override. Raster incremental WB, Auto and unresolved presets remain retained. Matching white-point coordinates does not establish camera profile or demosaic parity.
* Crop/straighten is retained pending coordinate/orientation/angle/canvas reference proof. Tone, color, sharpening and noise values are typed and retained; there is no invented `/100` or other processing conversion. Native controls remain available for user editing. Nonzero Adobe operations and unknown settings remain visible with compatibility reasons.

Historical and unresolved settings never become current recipe contributions. Missing or unsupported source values never reset an existing recipe: `RecipeContribution.apply_to` changes only present mapped EV/WB fields on an explicitly supplied validated target. `adobe_rendering_equivalent` is always false.

Domains are source-specific validation contracts, not a claim that Adobe's legacy [CRS schema](https://developer.adobe.com/xmp/docs/xmp-namespaces/crs/) and modern [API controls](https://developer.adobe.com/firefly-services/docs/lightroom/guides/apply-edits/) share algorithms. S10 must still retain all source payloads and present unresolved compatibility. This component alone is not migration acceptance.
