# Third-party notice inputs

The reference macOS notice inventory is generated from the exact desktop
Cargo lockfile, frontend lockfile, installed native library versions, Adobe DNG
SDK and Rust standard-library notice files. It is an input to
`package_desktop.py --notices`, not a new PhotoCatalog software license.
PhotoCatalog's own source currently has no project license declaration; this
collector does not invent one.

Use Python 3.11 or later; no Cargo, npm install, native compilation or decoder
runs occur:

```sh
python3 scripts/collect_desktop_notices.py \
  --checkout /absolute/desktop-checkout \
  --sdk /absolute/existing/dng_sdk_1_7_1 \
  --rust-sysroot /absolute/.rustup/toolchains/1.98.0-aarch64-apple-darwin \
  --output /absolute/new-notice-directory \
  --fetch
```

The explicit `--fetch` permits only the collector's public source retrieval;
crate archives must match Cargo.lock SHA-256 values, frontend archives must
match npm integrity and the installed notice/font bytes, and upstream monorepo
notices use the archive's exact VCS revision. Missing notices fail the command
and leave a held inventory rather than a fabricated SPDX/copyright substitute.
The output directory must be new. Earlier runs remain evidence, including any
held result. Do not use the system Python on macOS if it lacks `tomllib`.

`manifest.json` binds all files consumed by packaging. `inventory.json` records
versions, source checksums, collected text hashes, native receipts/library
hashes, and input hashes checked again at completion. Every upstream file is
preserved verbatim inside a delimited aggregate; source archives stay separate
binary files. The generated directory is passed as an explicit reviewed build
input, rather than committing a changing local-machine inventory to the repo.

The inventory deliberately covers all registry packages in the desktop lock,
including other-platform/build dependencies; it does not assert that all are
linked into the Mac executable. The three local packages are identified
separately: PhotoCatalog and the desktop app get no invented license, and the
modified vendored XMP crate plus its embedded notices are collected separately.
Two historical winapi import-library crates omit notice/VCS files; their
metadata directs consumers to the locked winapi parent, whose actual
checksum-bound license texts are carried with explicit family attribution.

Frontend coverage includes all non-development packages and the full installed
IBM Plex font set. Build/test-only npm packages are explicitly listed as
non-runtime rather than being confused with bundled frontend code. Any later
promotion of a package into production changes the lockfile and requires a new
inventory. A production font/notice whose local bytes differ from its locked
registry archive is rejected.

Native inputs cover the observed Homebrew components and their installed
library basenames/aliases. The final executable's recursive closure is still
authoritative: additional linked components require additional notice coverage.
Do not call the historical fifteen-path readiness list complete: its load
commands also reference SharpYUV and JPEG XL CMS libraries. The installed JPEG
XL formula disables skcms and uses system LCMS, which is inventoried separately.

Additional material includes libjpeg-turbo's IJG README, AOM's patent grant,
VMAF's embedded LibSVM and Xiph notice-bearing source files, Rust's standard
library copyright inventory, and the SDK/XMP/Expat notices. The exact installed
LibRaw source archive and build recipe and the five locked MPL source archives
are supplied alongside the notice text, rather than substituting an unsupported
written source offer. Review the finished package's source availability and
library replacement/relinking conditions before distribution. The inventory's
status is **notice-text coverage**, not a legal certification or installer test.

Before using an inventory for the reference installation:

1. Check `missing` is empty and verify every manifest hash using the packaging
   utility. Bind the collector source and exact lockfiles in the build receipt.
2. Check the first actual Tauri executable's recursive closure against the
   native inventory; preserve the exact installed architectures and versions.
3. Carry every manifest member into the app, including the corresponding source
   archives/build recipe, font notice, and aggregate license files. Do not copy
   only the small JSON manifest.
4. Retain the actual installed package and worker validation required by
   `DESKTOP_PACKAGING.md`; notice coverage supplies none of those runtime claims.

Small source tests:

```sh
python3 -m unittest discover -s scripts -p test_collect_desktop_notices.py -v
```
