#!/usr/bin/env python3
"""Reproduce the pinned XMP source-preservation vendor tree in a new directory.

Accepts the published crates.io .crate archive; never edits the Cargo cache.
The application enables the opt-in feature. Upstream default behavior is retained.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import tarfile

VERSION = "1.12.1"
SHA256 = "04517ad6f440d16e52ade0ad6d781029313bdacaac4590de9a9114f8d737af61"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    data = args.archive.read_bytes()
    if hashlib.sha256(data).hexdigest() != SHA256:
        raise ValueError("published XMP crate checksum mismatch")
    args.destination.mkdir(parents=True, exist_ok=False)
    prefix = f"xmp_toolkit-{VERSION}/"
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in archive.getmembers():
            if not member.name.startswith(prefix):
                raise ValueError(f"unexpected archive root: {member.name}")
            relative = Path(member.name[len(prefix):])
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("unsafe archive path")
            target = args.destination / relative
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(archive.extractfile(member).read())
            else:
                raise ValueError("unexpected archive link or special file")

    changes = []

    def patch(file, before, after):
        path = args.destination / file
        original = path.read_bytes()
        old, new = before.encode(), after.encode()
        if original.count(old) != 1:
            raise ValueError(f"pinned patch mismatch: {file}: {before!r}")
        updated = original.replace(old, new)
        path.write_bytes(updated)
        changes.append({"file": file, "before_sha256": hashlib.sha256(original).hexdigest(),
                        "after_sha256": hashlib.sha256(updated).hexdigest()})

    patch("Cargo.toml", "[features]\n", "[features]\nsource-preservation = []\n")
    patch("build.rs", '    println!("> git submodule init\\n");\n    git_command(["submodule", "init"]);\n\n    println!("> git submodule update\\n");\n    git_command(["submodule", "update"]);',
          "    // Published source is vendored completely; builds never mutate Git submodules.")
    patch("build.rs", "env, ffi::OsStr, fs, path::PathBuf", "env, fs, path::PathBuf")
    build_text = (args.destination / "build.rs").read_text()
    start = build_text.index("fn git_command<I, S>(args: I)")
    end = build_text.index("fn compile_for_docs()", start)
    patch("build.rs", build_text[start:end], "")
    patch("build.rs", "    let mut xmp_config = cc::Build::new();",
          '    let mut xmp_config = cc::Build::new();\n    if env::var_os("CARGO_FEATURE_SOURCE_PRESERVATION").is_some() {\n        xmp_config.define("PHOTOCATALOG_XMP_SOURCE_PRESERVATION", "1");\n    }')
    core = "external/xmp_toolkit/XMPCore/source/"
    block = "\t\tNormalizeDCArrays ( &this->tree );\n\t\tif ( this->tree.options & kXMP_PropHasAliases ) MoveExplicitAliases ( &this->tree, options, this->errorCallback );\n\t\tTouchUpDataModel ( this, this->errorCallback );"
    patch(core + "XMPMeta-Parse.cpp", block,
          "#ifndef PHOTOCATALOG_XMP_SOURCE_PRESERVATION\n" + block + "\n#endif")
    patch(core + "XMPMeta.cpp", "\tRegisterStandardAliases();",
          "#ifndef PHOTOCATALOG_XMP_SOURCE_PRESERVATION\n\tRegisterStandardAliases();\n#endif")
    patch(core + "XMPCore_Impl.cpp", "NormalizeLangArray ( XMP_Node * array )\n{",
          "NormalizeLangArray ( XMP_Node * array )\n{\n#ifdef PHOTOCATALOG_XMP_SOURCE_PRESERVATION\n\t// Parsing and serialization must preserve independent language values and order.\n\treturn;\n#endif")
    line = '\tif ( isTopLevel && (xmlNode.name == "iX:changes") ) return;\t// Strip old "punchcard" chaff.'
    patch(core + "ParseRDF.cpp", line,
          "#ifndef PHOTOCATALOG_XMP_SOURCE_PRESERVATION\n" + line + "\n#endif")
    (args.destination / "PHOTOCATALOG_PATCHES.json").write_text(json.dumps({
        "upstream_version": VERSION, "upstream_crate_sha256": SHA256,
        "feature": "source-preservation", "changes": changes,
    }, indent=2) + "\n")


if __name__ == "__main__":
    main()
