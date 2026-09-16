# PhotoCatalog desktop packages

The desktop app lives in `desktop/src-tauri` and its executable is
`photocatalog-desktop`. Native preview and photo-export workers run through that
same installed executable, with `--preview-worker` and `--photo-export-worker`
handled **before** webview initialization. Worker paths must come from
`std::env::current_exe()`, not the checkout, current directory, or `PATH`.

`scripts/package_desktop.py` finalizes a macOS app or checks the native closure
of an extracted Linux/Windows install payload. It does not build, download,
install, publish, use signing credentials, or execute inspected binaries.
Reports say `PASS_DEPENDENCY_CLOSURE_ONLY`; installed launch/worker validation
is a separate gate. The script uses Python's standard library (Python 3.9+).

## macOS sequence

Use the repository's pinned Rust/native dependencies and the desktop's locked
frontend/Tauri dependencies. Build on the intended target architecture; an arm64
build is not a universal build. Reserve install-name space when linking, for
example by adding `-C link-arg=-Wl,-headerpad_max_install_names` to the desktop
build's existing `RUSTFLAGS` (do not discard existing flags).

From `desktop/`, after the frontend dependency install:

```sh
python3 ../scripts/desktop_tool.py --desktop . build
python3 ../scripts/desktop_tool.py --desktop . bundle -- --bundles app
```

The wrapper removes every ambient `APPLE_*` and `TAURI_SIGNING_*` variable and
sets `APPLE_SIGNING_IDENTITY=-`. Use it for local and CI commands: merely omitting
credentials from the command line does not prevent Tauri from discovering an
ambient Developer ID/notarization configuration. No credential values are logged.

Use the actual Cargo target directory from that build; it need not be the
repository's default. Then, from the repository root:

```sh
python3 scripts/package_desktop.py macos \
  --app /absolute/target/release/bundle/macos/PhotoCatalog.app \
  --output /absolute/new-private-package-directory \
  --notices /absolute/reviewed-notices/manifest.json \
  --dmg
```

Tauri's [separate bundle command](https://v2.tauri.app/reference/cli/) operates on
an already-built executable. Its [macOS framework configuration](https://v2.tauri.app/distribute/macos-application-bundle/)
copies specified libraries; the native linker paths still require attention.
For this workflow leave `bundle.macOS.frameworks` empty for the image-library
closure. The script discovers these libraries itself. Do not subsequently ask
Tauri to rebuild the finalized app while generating a DMG.

The finalizer resolves recursive `otool` dependencies, including inherited
`@rpath` paths anchored to their declaring loader. It copies the input app into
a **new** output directory, copies non-system dylibs into `Contents/Frameworks`,
rewrites executable edges to `@executable_path/../Frameworks`, rewrites library
edges to `@loader_path`, and removes previous rpaths. System libraries under
`/usr/lib` and `/System/Library` remain system dependencies. Custom third-party
framework bundles require explicit support and currently fail this finalizer;
they are not flattened into fake dylibs.

Every copied library must support every architecture in the main executable.
The finalizer reads each file's `vtool -show-build` output and raises the copied
app's `LSMinimumSystemVersion` (and any per-architecture overrides) to the maximum
native deployment requirement before signing. The audit rejects an understated
minimum. SDK/linker versions are not deployment requirements, and no Mach-O
minimum is rewritten to manufacture compatibility. Source configuration remains
unchanged. The report records both the source declaration and effective floor;
for the initial reference Homebrew closure this is macOS 26.0, not the scaffold's
prospective 12.0 declaration. Missing or ambiguous
libraries, basename collisions, insufficient install-name space, escaping
symlinks, external final load paths, and missing license coverage fail.

The finalizer signs dylibs and the executable, then the app, using an ad-hoc
identity and verifies the result. This is local validation signing, **not**
Developer ID signing, notarization, or Gatekeeper distribution qualification.
It does not preserve a preexisting release-signing/notarization identity; use
this on the local Tauri app before release signing. A release-signing pipeline
must preserve required entitlements and sign the finalized content separately.
A DMG is created from the finished `.app` plus an Applications link and verified
with `hdiutil`; an existing installer is never patched. The input app is left
unchanged. Failed output directories remain for inspection and cannot be reused.

## Licenses and notices

A reviewed manifest binds **verbatim** notice files by SHA-256. An example entry:

```json
{
  "protocol": 1,
  "components": [
    {
      "component": "libraw",
      "libraries": ["libraw.25.dylib"],
      "files": [
        {"path": "libraw/COPYRIGHT", "sha256": "<exact file SHA-256>"},
        {"path": "libraw/LICENSE.LGPL", "sha256": "<exact file SHA-256>"}
      ]
    }
  ]
}
```

Paths resolve relative to the manifest. Supply one entry per component, all
relevant license/notice files, and its actual copied library basenames. The
complete manifest must also include `adobe-dng-sdk`, `xmp-toolkit`,
`rust-dependencies`, and `frontend-dependencies` entries even though those need
not have dynamic library names. The latter inventories must cover the resolved
Cargo and frontend lockfiles, including statically linked LCMS, bundled SQLite,
C/C++ dependencies and generated frontend assets. A heading or a dependency
name is not a license inventory. The script checks coverage and exact supplied
bytes; the manifest reviewer remains responsible for completeness, version
association, redistribution terms and any required source/offers. It never
invents a license or silently substitutes one.

The observed macOS CLI closure during readiness contained LibRaw, libavif,
WebP, JPEG XL and its threads library, libjpeg, OpenMP, LCMS, dav1d, aom,
highway, Brotli decoder/common/encoder, and libvmaf. This is a useful starting
inventory, not a hardcoded promise about a future desktop build. Use that
build's discovered closure. Native development packages alone are not the
license inventory.

macOS notices are copied into `Contents/Resources/THIRD_PARTY_NOTICES` before
signing. On Linux/Windows, place the manifest and its files in the Tauri
resources/install payload and pass that **installed** manifest to the audit;
a manifest pointing to texts outside the payload fails.

## Linux and Windows extracted-payload checks

Generate the platform's Tauri artifacts, then extract/install them in a fresh
test location under the platform's integration procedure. This utility does
not unpack or install an artifact. Run on the extracted payload using GNU
`readelf` on Linux or LLVM `llvm-readobj` on Windows. Do not use `ldd` on input
artifacts: the audit never executes them.

```sh
python3 scripts/package_desktop.py audit --platform linux \
  --root /absolute/extracted/AppDir \
  --executable usr/bin/photocatalog-desktop \
  --policy /absolute/reviewed-linux-policy.json \
  --notices /absolute/extracted/AppDir/usr/share/photocatalog/notices/manifest.json \
  --report /absolute/new-closure.json
```

Use `--platform windows`, the installed `.exe` relative path and a Windows
policy for NSIS/MSI payloads. Both native normal and delay DLL imports are
checked by LLVM's `--coff-imports` output (there is no separate delay-import flag). The policies are explicit deployment contracts, for example:

```json
{
  "protocol": 1,
  "platform": "linux",
  "library_directories": ["usr/lib"],
  "system_dependencies": {
    "libc.so.6": "glibc from the declared minimum supported Linux distribution"
  }
}
```

List exact native names, with a nonempty package/OS contract for every system
exception. Do not label a build-only image library a system dependency merely
to make the audit pass. On Linux every packaged non-system dependency must be
reachable through that object's effective `$ORIGIN` search path. When DT_RUNPATH
is present it supersedes DT_RPATH, including an empty RUNPATH; an empty entry
is rejected because it depends on the working directory. Absolute build paths,
relative CWD paths and escaping paths fail. This deliberately stricter local
layout avoids depending on inherited ELF RPATH or the developer's loader cache.
`library_directories` identifies payload directories for policy review; it does
not manufacture an ELF search path. Build/package the libraries with local
paths, or declare and install the actual supported distro package. In
particular, the existing Rust CI's checkout `.deps/jxl/lib` and
`LD_LIBRARY_PATH` must not leak into an installable app.

On Windows `library_directories` must be the executable's directory. Bundled
DLLs, including any required MSVC runtime or WebView2Loader, must be there and
match its machine type. `x64-windows-static-md` is not proof of a zero-DLL
closure. Enumerate exact Windows system/API-set imports in the reviewed policy;
review the installer configuration for its VC/UCRT and WebView2 prerequisite
strategy. Test on a machine without the developer's vcpkg or Visual Studio
paths. Tauri's [WebView2 installation options](https://v2.tauri.app/distribute/windows-installer/)
are separate from native image-library closure.

For Linux AppImage, ensure private image libraries really reach the AppDir;
[Tauri's custom AppImage files](https://v2.tauri.app/distribute/appimage/) are one
supported inclusion mechanism. Test the declared oldest WebKitGTK 4.1 distro
baseline. DEB/RPM dependency declarations and installation must match any
system exceptions. The closure report records exceptions rather than claiming
they were installed or ABI-tested.

## Required platform acceptance after closure checks

Run these gates on each of macOS, Windows and Linux, preserving artifact and
installed executable hashes, platform/architecture, commands, logs and cleanup:

1. Install/mount/extract into a new location outside the checkout. Check the
   package's notices, architecture/OS floor and actual installer dependencies.
   Do not inherit `DYLD_*`, `LD_*`, checkout paths, vcpkg paths or Homebrew paths.
   Use a fresh temporary working directory and a supported clean-machine image
   for the platform prerequisite test; merely clearing environment variables
   does not hide already-installed system libraries.
2. Launch the installed PhotoCatalog webview as an ordinary user. Confirm it
   writes catalog/cache/config/staging under user-controlled locations, not the
   signed bundle, Program Files, or the checkout. Close and reopen it.
3. Using temporary **synthetic** originals, run preview regeneration and a
   photo export through the public service path. Observe child processes using
   the installed executable and the early worker dispatch; no second webview,
   checkout executable or loader override. Cover representative RAW/DNG,
   AVIF/WebP/JXL paths, XMP retention and ICC export as supported by the fixture
   suite. Keep real-original/RAID admission separate.
4. Preserve timeout/cancel/restart and worker-reaping evidence. Exercise a
   moved/relinked synthetic original and restored catalog preview regeneration.
   Removal/update must not delete catalogs, backups or external originals.

The Python synthetic tests qualify dependency discovery, failure handling,
license pinning and output policy only. They do not qualify native relocation,
Tauri installation, rendering, signing distribution or any platform's GUI.
Those actual package/install tests remain required S12 acceptance evidence.

Run the script checks without native builds:

```sh
python3 -m unittest discover -s scripts -p test_package_desktop.py -v
```

## Hosted three-platform qualification

The existing `test` matrix runs the frontend tests/build, a locked Tauri build,
then packaging and installed-worker observation in the same OS lane. It reuses
that lane's DNG SDK, native libraries and Cargo cache. The finite job deadline
includes package preparation; failures retain their output instead of publishing
a partial installer. Artifact upload is CI evidence, not a release publication.

`desktop_ci_stage.py` prepares Linux and Windows native inputs **before** Tauri
bundling. `desktop_ci_package.py` supplies a generated bundle config and reads the
resulting installed payload. These commands are intended for disposable hosted
runners; the Windows command actually runs the per-user NSIS installer into a new
temporary directory. Never aim it at an existing installation.

* macOS uses the reviewed finalizer, mounts the resulting DMG read-only, copies
  the app outside the checkout, then repeats closure/signature/deployment-floor
  inspection. Ambient Apple/updater signing credentials are removed from both
  Tauri invocations. This is an ad-hoc checkpoint, without a release signature or
  notarization claim.
* Linux qualifies a DEB. The SDK's JPEG XL shared libraries ship under
  `/usr/lib/photocatalog-desktop/native`, with `$ORIGIN` paths. Other native
  SONAMEs must resolve to a unique installed Debian package; exact versions
  become declared minimum dependencies. The DEB is extracted outside the
  checkout and its actual loader paths are checked. It is not an AppImage test.
* Windows walks regular **and delay** imports. Vcpkg libraries and MSVC runtime
  DLLs are copied beside the executable; the latter must come from the installed
  Visual Studio `VC/Redist/MSVC/.../x64/*.CRT` directory. Only an explicit set of
  Windows OS imports/API contracts may remain external. An unknown import fails.
  The NSIS payload is installed into a fresh directory before inspection.
  WebView2 uses the configured bootstrapper; its actual GUI startup is a distinct
  acceptance check.

The public service probe `examples/desktop_worker_smoke.rs` creates a small PNG,
imports it into a temporary catalog, commits an exposure edit, and requests a
real preview and PNG export from the **installed desktop executable**. It checks
child metrics, output dimensions/edited pixels, unchanged original bytes and
cleanup. `installed_worker_smoke.py` reuses `edit_campaign.invoke` unchanged for
bounded process-tree observation (120 seconds, 1 GiB per process, 2 GiB group,
64 MiB free reserve). Loader/signing/developer environment variables are removed;
PATH is reduced to system directories and the working directory is outside the
checkout. The Linux observation tool gets a private explicit SDK RPATH; that
observer is not distributed and is not substituted for the installed executable.
The Windows observer gets its own directory with the installed runtime DLLs.
Linux/Windows executable and native bytes must match the staged hashes after
installer extraction/installation. The installed tree is retained with CI
evidence even when qualification fails.

Notices are collected from each platform's actual sources. The collector's
`--native-input` accepts only an explicit Linux/Windows manifest with bounded,
contained, checksum-verified files. Linux includes the SDK JPEG XL and nested
third-party notices; Debian manages notices for its own libraries. Windows
includes installed vcpkg copyright/SPDX records and checksum-verified LibRaw
source archives plus the exact port/patch/build files; MSVC runtime notices come
from that Visual Studio installation. No macOS Homebrew receipt is used as
Windows or Linux evidence. A missing license/source input fails preparation;
new license choices or redistribution restrictions require review before a
release. The application itself has no invented license grant.

Relevant primary references: [Tauri bundle configuration](https://v2.tauri.app/reference/config/),
[Tauri Windows installer](https://v2.tauri.app/distribute/windows-installer/),
[Microsoft runtime redistribution](https://learn.microsoft.com/en-us/cpp/windows/redistributing-visual-cpp-files?view=msvc-170),
and [Microsoft application-local deployment](https://learn.microsoft.com/en-us/cpp/windows/choosing-a-deployment-method?view=msvc-170).

A successful `qualification.json` means installed dependency closure and actual
worker dispatch passed on that particular CI platform. It does **not** certify
interactive GUI behavior, all desktop workflows, signing/notarization, older OS
compatibility, or S12 as a whole. Those acceptance observations remain required.
