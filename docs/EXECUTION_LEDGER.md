# Epic sc-22835 execution ledger

Source of truth: https://app.shortcut.com/trefry/epic/22835

## Current delivery — 2026-09-14

S12 sc-22847 is In Progress; S13 sc-22848 is unstarted. Draft PR16 at
`f5917e19c01f648ed5af61e97f6a1b5bd7fa5de2` passed all four CI34804834306 jobs
and all three installer/archive/payload audits. Root verified 28 artifact hashes
in `sc-22847-ci-34804834306-z9sy0qyl/receipt.json`, SHA256
`2de9984ef36b72e6ab8ca0bf8afce306ed23ff5ca12f44372e0785995eee239d`.
PR description and exact head/body readback match. This qualifies the published
FS7 checkpoint and packaging evidence, not full desktop interaction. The Mac
observer binary was not uploaded; its reported hash is not independently verified.

Lightroom admission prerequisites remain isolated from the desktop PR:

- `bc759a8`: four fixed-layout allocation probes passed.
- `4c36a57`: two bounded error-formatting regressions and formatting passed.
- `af6b7b2`: saved artifact descriptors and IDs are validated on borrowed SQLite
  values before copying; existence-only reads use EXISTS. Four tests and formatting
  passed, including malformed-row pending/recheck, retry, offline raw custody and
  authority separation. Root verified 122 committed blobs and nine runtime
  artifacts. Receipt `sc-22847-saved-artifact-guards-giq85t1c/gate-initial-1789361046416382000/receipt.json`
  SHA256 `46070894fd747ccfcfb80c24d68eb93e3e9b9b04066b6cbfb2d0c13ad529b213`;
  independent final review SHA256
  `c1448f7257a327767d10800bd84f1b86a5595b7e5fbc91d8db9e02c4b8f02747` passed.

FS8 native/stage/prepared design v6 passed independent review; actual child
ownership, header/full decode and shared scheduler implementation is in progress.
Complete Source/core aggregate admission and migration wiring remain active,
including Windows path preparation before original Source admission. Remaining
filesystem, backup, Workbench, metadata editor and installed UI acceptance retain
the full tracked scope. Current local tests use synthetic files without RAID/GPU.

## Earlier checkpoints — preserved history


FS7 is independently accepted and integrated as
`f13ddb30bbb6eedf3158ac00a9e718f2870c49a3` from author
`b8a1cefa30ccd19ef45b6436041ce2273c642b57`. All19 committed/source blobs match
the reviewed checkpoint. Root verified61 final qualification artifacts;
independent review `sc-22847-fs7-final-independent-wf08wccl/review-final.json`
has SHA256 `b8313ad90b0ca01bc0b506877ded781662450e6a6f7a3b5866e8debc6872d52b`.
Coverage is113 distinct positives:105 focused, six configured C/F cases and two
compatibility tests, plus strict Clippy and formatting. Integrated Mac Tauri
release/locked compilation passed in16.44seconds; exec92743/PID34725 terminal0
and reaped. Its receipt is
`sc-22847-fs7-integration-1bdnfsrv/gate-initial-1789358718496710000/receipt.json`,
SHA256 `5bada1b51a2bbf388a2d893f4d7647cf835599158bac62fe191031d842a774de`.
This batch is ready for publication; newer CI is not yet qualified. Managed route
selection, FS8 native ownership, full Source/core admission and remaining S12
requirements remain open. Later paragraphs below preserve earlier checkpoints.

Latest continuation checkpoint: the isolated Lightroom migration branch now
includes `040dd7b4bdf09943acd705c5f68731e28dfe7af0`, which validates retained
identity storage, exact byte length and UTF-8 before allocation at all three
identified initial/recheck readers. Independent source review and six regression/
compatibility tests plus formatting passed. Root verified all122 committed blobs
and nine gate artifacts. Receipt `sc-22847-retained-identities-c1b54mxh/gate-initial-1789358190523259000/receipt.json`
has SHA256 `9761d043e6d0617e7b1789f8f09e88e15216633623daf041b6673d05f5c303be`.
This remains isolated from the desktop PR; aggregate Source/core memory admission
and production integration are unfinished.

FS7 local qualification now covers105 focused positives, six configured C/F
process cases and two compatibility cases, strict all-target Clippy and format.
The nine ordinary ignored entries are six configured behavior tests subsequently
passed plus three inert entrypoints, correcting earlier shorthand that called
all nine inert. Source v4 is frozen pending final independent acceptance.
FS8's required next inventory includes both native worker-output validation and
encoded-cache-hit decode; managed N owns decode/encode. No user originals or
catalogs were accessed by these synthetic gates.

The user selected **LensWorks** as the product name on 2026-09-13. Current
branding uses LensWorks; historical evidence below retains its original names.
Repository, bundle and persistent format identifiers remain compatible.
Shortcut epic E1 and S12 were updated; comment23342 records the decision.

S12 sc-22847 remains In Progress; S13 sc-22848 has not started. Public draft
PR16 head `81986e37e0119793682cdca6aa3559cc903c59b6` passed all four
CI34797188873 jobs and all three installer/archive/payload audits. Root verified
all41 evidence hashes in `sc-22847-ci-34797188873-rw8rygeh/receipt.json`, SHA256
`b8d83dd403dee42a455599be99b2df5ff84057548ac5efaf8e56be20c0d65f10`. The preceding `33d4a84` checkpoint passed all four
CI34794176560 jobs. All three provider archives, installers, native payloads,
notices and installed-worker evidence passed read-only audit. Root reverified
all56 evidence hashes in `sc-22847-ci-34794176560-p04ycpod/receipt.json`, SHA256
`97280aaadc9624be2f46728ccf85c348167e9142383ba659f2a28f96617f62d7`.
The published head includes the configured catalog/filesystem relay fixtures;
FS6 custody is now independently qualified and integrated locally at `9acecca`.
No whole desktop or GUI acceptance is claimed.

FS6 author `7174222b85844bccbc54f49d75b6f8bf97773455` is integrated as
`9aceccadf5627007f7f68a165fc5289fe3a4dcee`. Root verified all21 committed
blobs against reviewed source and all62 qualification artifacts. Independent
final review `sc-22847-fs6-ipc-review-j6xayps7/review-final-v7.json`, SHA256
`45476fc752a814c8fbdaf2af86b9986b9a8d5b178d8830a989af7acc7da33040`, passes
all six original findings. Qualified coverage is F32/relay25/C4, six configured
process fixtures and two compatibility cases; the changed queued fixture also
checks successful worker retirement and relay join explicitly. Strict all-target
Clippy and formatting pass. Integrated Mac Tauri release/locked check passed in
10.44 seconds with all21 source pins intact; session66336/PID36192 is terminal
and reaped. Receipt `sc-22847-fs6-integration-leosphhb/receipt.json` records log
SHA256 `4b6ec628b0a8070c2ad2218ca40d4c6969f6fc91333bc95c3e99f8a365faec49`.
The filesystem helper retains preview tier/relocation locks through the final SQL
drain; retries retain exact operation identity, stale operations cannot repeat
effects, and bounded status remains independent of blocked work. The managed
route is still unselected. FS7/FS8, actual native-descendant integration and full
S12 acceptance remain required; these tests did not touch originals or XMP.

LM Part C v30 is frozen for independent source review (114 pins), including the
runtime-unqualified v29 lint batch. Complete bounded seal/authority/descriptor
opening and shared core-private retained-record projection are implemented in
its isolated worktree. Nine new fixtures and carried affected tests await the
combined gate; aggregate memory qualification and production migration wiring
remain unfinished. Source receipt `foundation-source-v30.json`, SHA256
`edffd90ccd0bdf27a9d20e7c8456b8a6929e923f67e84e402516abf23268dd47`, under
`sc-22847-lightroom-migration-bridge-xksnotzh`. This is not part of the published
FS6 batch and no runtime parity is claimed yet.

Current public head `81986e37e0119793682cdca6aa3559cc903c59b6` includes the
qualified FS6 integration and this ledger. Fresh CI34797188873 is running;
comment23353 and exact PR body/head were read back. Its private audit directory
is `sc-22847-ci-34797188873-rw8rygeh`; prior successful CI receipts stay frozen.

FS7/FS8 plan-v3 is accepted in comment23354 after independent design review
`sc-22847-fs78-design-independent-d0rcl58x/review.json`, SHA256
`cc9eb46412417c7eb9557cd612ea4eca77d37d1bdd2543521ca283315aa6ef76`.
FS7 implementation starts on isolated `codex/sc-22847-preview-io`: bounded binary
transport followed by complete cache/relocation filesystem operations. FS8 keeps
staging/prepared work and managed G-owned native children as a required next
slice. Native wait must never block Stop while holding the sole live Child;
actual wait and IO joins precede cleanup/fence release. No production selection.

LM Part C v30 initial gate failed and stopped after five passing tests and one
Authority grammar mismatch. Root verified all10 evidence hashes; all114 source
pins remain unchanged. Receipt `source-v30-initial-20260914T015313Z-41219/receipt.json`
SHA256 `3ac75f41daacb8c3985acc02d56af76a301f7063321c09a890641cc3c3295466`
under `sc-22847-lightroom-migration-bridge-xksnotzh`. Session21101 is terminal and
reaped; no observed descendants remain. Independent source review8314d59 requires
buffered enum/path unit grammar and map-only Authority struct-variant corrections.
The author is applying both before the successor freeze; unrun tests remain unrun.
Measured macOS structs: InputSeal304, SelectedCapture104, SupplementPin176,
String24, ArtifactDescriptor496. The successful seal payload expression is
21,365,041 bytes excluding parser/transient/allocator owners; this is not an
aggregate opening/RSS qualification.

FS7/FS8 v4 makes kill-error ownership explicit; independent delta review
`sc-22847-fs78-design-independent-d0rcl58x/review-v4.json` SHA256
`3b01c6ad410137189cd40e8fa5173d3a18f0e8f7e65ba343c2e246013f862b6d`
passes without topology/scope changes. FS7 source implementation is active.

LM correction v31 is frozen at115 pins for independent review. It preserves
buffered-only numeric NativePath identifiers and empty-map unit enum payloads,
keeps direct parsing unchanged, and requires Authority variant bodies to be maps.
Two new public-oracle matrix functions supplement the original fixtures; no new
runtime claim yet. Source manifest SHA256
`5c2cbc990016d31ad09c00b797945f1cac9902f20fcd29cd79346212249452eb`.
Lifetime accounting review separately identifies parent-side chosen Artifact and
request clones before descriptor admission, whole Manifest/retained record/seal
overlap, validation BTreeSets and Source native-path/URI/open transients. These
must remain charged through rejection; the small descriptor-decoder bound does
not bound the parent constructor or total RSS.

LM v31 independent source review `review-v31.json` SHA256
`b34f7a8c613d044a4cc06f156fb4a212299680753d3dec3f5c19c30c7d7fdc88`
passes both corrective findings. Initial gate now passes15 actual functions plus
package formatting; root verified all20 evidence hashes,115 stationary pins and
exact counts. Session4966 is terminal/reaped. Receipt
`source-v31-initial-20260914T020507Z-45843/receipt.json` SHA256
`8fcb4176b7aba8089e0afebe8ec634142e7f1467b931941f753177119e3c8b83`.
Compiled Manifest box576 and Value inline192 bytes were measured; the tested
nested grammar cutoff agrees with the public decoder (123 accepted,124 rejected
in this fixture). Compatibility16 functions are now running separately; no result
or aggregate-memory qualification is implied yet.

LM v31 functional checkpoint is committed clean at
`50926345cdb3e0c476c129548fba9a1a9a89da59` (22 files). Root verified all115
committed blobs. Final independent source/runtime review SHA256
`c6b2be29ea049b4367c3bf85917f3638c70235937e06c6b87a71f8bbfae12caa`
passes30 functional tests (four actual-process) plus one inert helper and fmt;
root independently verified all40 runtime evidence hashes. Compatibility receipt
`source-v31-compatibility-20260914T020743Z-46819/receipt.json` SHA256
`f9948d55fc4b57476d845185434cf8734cd38bdf16a09d4514f8e0c547675d56`.
All native sessions are terminal/reaped. This functional source remains isolated
from PR16; aggregate memory/maximum observations, strict Clippy production wiring
and complete LM acceptance remain open. Actual consumer analysis additionally
finds pending/request retaining two Manifests; their overlap must stay charged.

At CI34797188873 / `81986e3`, macOS/Linux/contracts are successful; Windows
is still running. Download session11907 completed/reaped. macOS archive10330293580
(77,567,711 bytes), SHA256
`ca073ea5f1eebc9758867b7caeb606bfd1d6ccdba76bc09b61570bf5c248544f`,
and Linux10329789542 (51,540,951 bytes), SHA256
`ca4370d8615f0e296b9a0a6ac6dc2eb22c88b304ec683d92d86fe30f2c4e6ca5`,
match provider receipts. Both payload/installer/notice/worker-evidence readbacks
pass (Mac18 closure files/25 notices, Linux4/17; Mac observer receipt-only,
Linux included observer rehashed). This is not all-platform terminal acceptance.

Reference Mac launch smoke now verifies the actual new LensWorks package:
47 payload files and deep/strict code signature verified; native window/header/menu,
onboarding and catalog-picker open/cancel observed through computer use. No
catalog/photo was opened. The native picker restored the prior RAID test folder
and listed directory/file metadata. Command-Q reported App quit and known PID56948
was absent afterward; root is not its wait parent and makes no wait/reap claim.
No full catalog/editing/accessibility/color acceptance. Private fresh copy and
receipt `sc-22847-mac-gui-fs6-_d_97zwv/receipt.json`, SHA256
`30a1e81707260adb97384c5f29606465aa9bc8827225448b70e91997ec574cfd`.
Original CI artifacts remain unchanged. PR body/head read back exactly after
updating this limited native GUI evidence. Native/UI lane is released.

LM numerical observation plan-v3 is accepted for test-only implementation after
finite consumer review435cf4cc (16 committed pins). Five families cover opening,
parent Artifact construction, actual consumer overlap, retention batch break/resume
and transport overlap. A nondefault internal-capacity-probes feature gates all
allocator instrumentation behind cfg(test); default ordinary suite and production
builds keep their allocator. Source plan SHA256
`c656e16eb7476ae92e46b9af433ba683e6b16a6a31ff386dfe89fffa17738351`.
No native measurement or aggregate memory qualification yet.

Earlier PR16 checkpoint `e841bdb8a452c747bbf4e1eaf429c52f82c402b6` passed all four
CI34783536524 jobs (macOS, Linux, Windows, contracts). All three provider ZIP
digests and included installers, executables, native closure and notices passed
artifact audit. Root verified all 40 evidence hashes. Receipt
`sc-22847-ci-34783536524-2pvqkiwe/receipt.json`, SHA256
`169148975be76f7aa150a9d1ecd6952cb4c2914bfb66397c97fa21493f076d55`.
Linux/Windows observer executables were included and rehashed; Mac observer is
receipt-only. Known root/child reaping and temporary cleanup were verified;
GUI acceptance and absence of undiscovered descendants are not claimed.
This run includes additive catalog transport and the exclusive-create guard.
Shortcut comment23340 and In Progress state were read back exactly.

The LensWorks/helper batch is published at `383944d`. Fresh CI34790332966
finished with contracts passing and three platform failures. Windows stopped at
a test-only Unix mutation warning; macOS/Linux passed the Rust suite and built
LensWorks, then rejected ambiguous cached PhotoCatalog/LensWorks bundle outputs.
Both failures are repaired together: only the Unix fixture binding is mutable,
and disposable CI clears generated bundle output before bundling while preserving
compiled executables/dependencies. The focused discovery test, strict all-target
Clippy and formatting pass; thirteen staging/packaging tests pass on the pinned
Python runtime, including stale app/DEB/NSIS outputs and link refusal. The initial
default-Python import failure is preserved separately. These are local repair
results; fresh hosted qualification remains required. Comment23345 records the
preceding integrated gate and the XMP zero-test correction; it was read back.

The repairs are published at `872667e7375f64246e564e74ce3ce795fc38a316`.
CI34791575107 passed all four jobs, including macOS, Linux and Windows packaging
and installed-worker checks. All three provider archives and their included
payload, notices, installer and worker evidence passed read-only verification.
Receipt `sc-22847-ci-34791575107-5284b88m/receipt.json`, SHA256
`042a3d7efe02bf7b938906845b357b7af7d7c4167736726eb0b3c36b4a611d67`.
Mac observer remains receipt-only; GUI acceptance and undiscovered descendant
absence are not claimed. Comment23346 and its In Progress state were read back
exactly.

Source Part B's refined full-method admission plan is accepted in comment23347:
first qualify explicit public/private JSON grammar and shortest accepted forms,
then implement exact-capacity Manifest decoding and request-derived preflight
for all nine result kinds and ten Page collections. Candidate byte weights are
not qualified limits. Part C and combined lifetime admission remain required.
The first test-only milestone passes all four grammar functions and package
formatting at frozen v27 (104 pins). The tests confirm the private decoder's
sequence-tag acceptance widening, which the production B implementation must
repair. Receipt `sc-22847-lightroom-migration-bridge-xksnotzh/`
`source-v27-runtime-receipt.json`, SHA256
`cb69174946734ba46c9c5125b948db057f12523abc284bbf2d8181ecfcdf3297`;
root verified all four evidence hashes and actual test/gap output. No real files
or child workers were opened by this JSON-only gate.

FS6 plan v4 is accepted in comment23348 (SHA256
`0b5794e1dd8aadb958f26d3af1decfd00b06b90b89be5c3741732b865d12f928`).
The existing F will own preview/configuration descriptors through a session-bound
C adapter, retaining current, partial and retired root locks until verified final
native and SQL drain. Managed admission uses independent 16-root/2 MiB actual
owned-capacity limits, exact 256-byte serialized markers and the existing native
path cap. Real lock contention, lost results, restart/promotion and checked release
must be tested before acceptance. Store status has its own bounded control and
response capacity alongside bootstrap admission status. Both comments and S12's
In Progress state were read back; FS7/FS8 and production route selection remain
required.

The real catalog/filesystem relay is integrated as `f7c0b15` (author
`9539540a3fa04cf3870b7f8877fa52cca38745d8`). All nine integrated files initially
matched qualified v6 exactly. Twenty-one pure tests and six actual-process
fixtures pass, along with strict all-target Clippy, package formatting and the
exact CLI build. Actual SQLite contention verifies held write locks across F
marker operations, nine SQL closes before root release, and C fatal-74 retirement
before F termination after lost confirmation. Root verified all 43 final evidence
artifacts and nine committed blobs. Final author receipt
`sc-22847-filesystem-relay-y8j0veyc/final-receipt.json`, SHA256
`65fa85c02640604175040d7fa5c6baca61fed87a1d11906647c2f1b0377e545f`.
The integrated release/locked Tauri check also passes (8.86 seconds).

Integration found the configured-CLI fixtures needed a dedicated CI invocation:
the ordinary all-target test step runs before CLI build and supplies no helper
path. The six external-process tests are now explicitly ignored in ordinary test
runs and required by a separate post-build runner (six on Unix, five on Windows).
It supplies the exact built executable and rejects zero-test success; the two
inert subprocess entrypoints are never admitted as ordinary tests. Local runner
qualification passes: exact D release build, all six configured fixtures through
the new runner, and package formatting (session31048 terminal/reaped). Receipt
`sc-22847-filesystem-relay-y8j0veyc/root-ci-wiring-gate.json` preserves the four
source pins and all three logs. Independent review then caught Unix-only physical
IDs in a pure test helper; it now emits the proper native identity on Windows as
well. The ordinary desktop gate passes 21 tests with eight explicitly ignored
external/helper entrypoints and no configured executable; strict all-target Clippy
and formatting also pass (session68966 terminal/reaped). Independent four-file
source review passes with exact hashes. This fixture correction is checked
separately from unchanged actual-process test bodies. The relay and CI wiring are
published at `33d4a84f3e992e7d423ddf046e21b3450946e735`; fresh CI34794176560 is
running. Draft PR16 head/body and Shortcut comment23350 were read back exactly.
Production selection still requires the remaining custody work.

FS6 frozen v1 (20 files) requires a finite repair batch before native testing.
Root review identified stale operation replay after the latest receipt changes,
and ResourceLimit being downgraded on exact replay. Independent IPC review found
missing helper source-identity inputs, Stop canceling pending cleanup, unchecked
StoreStatus failures/query identities, and a status query that could wait after
the control transport stopped. The private store operation becomes a monotonic
U64 with a retained high-water fence, exact latest replay, cancellation gaps and
checked exhaustion. No persistent schema changes. Root review SHA256
`c70001ed915c21b1e5ae763a1b55bfb07c1f1aa05716872aa750f09df152b187`;
IPC review `d9c61fb211a9b13380ee7057a12c7bfd5606589afc68d55d79139f307c27bd45`.

LM Source Part B v28 passed independent review (107 pins, 11-file delta) and
the initial native gate: nine Manifest grammar/admission tests, four method
preflight tests and package formatting. Session71706 and all three commands were
reaped; source remained unchanged. Root verified all eight evidence hashes and
the actual thirteen-test result. Receipt
`sc-22847-lightroom-migration-bridge-xksnotzh/source-v28-runtime-receipt.json`,
SHA256 `375b0f74bca65280f7cf9db903d801bbd9ff5e7396517c05aa1174d151088f95`.
Larger private reader/lifecycle tests are next; Part C and aggregate memory are
still required. Shortcut comment23351 and In Progress state were read back exactly.

Part B is now a clean source checkpoint at
`a7682ba82e7fb09a9f29cbcfd31b412cb5428484`; root verified all107 committed blobs.
The larger gate also passed, totaling26 functional functions (9 actual-process,
17 in-process) plus one inert helper. Corrected runtime receipt SHA256
`f5573fffa02bbdcc9e29f58021a8ea72fae159a40b94409e4fa7d548e236c5bc` and independent
audit `6319685683f2568076061a3a57f1e01a997ee3267ec0ff71fdb015cac6fa33cf` distinguish
the nine actual-process functions from pure framing tests and the inert entrypoint.
Root verified21 larger-gate evidence hashes and the14 selected function results;
the initial13 remain the separately verified gate above. The full16MiB shortest
Issue fixture retained1,525,033 entries in109,802,376 exact vector bytes; foreign
path evidence retained8,387,806 native units. These measurements do not establish
aggregate allocation or RSS. Concrete Clippy repairs and the remaining production
integration/Part C are still required. Comment23352 was read back exactly.

At published33d4, CI34794176560 finished with all four jobs successful. All three
provider archives, payloads, installers and notices passed read-only verification.
macOS has18 closure files/25 notices and a receipt-only observer; Linux has4/17
and Windows4/71 with their observers included and rehashed. The frozen audit is
under `sc-22847-ci-34794176560-p04ycpod`; root reverified all56 evidence hashes.

Earlier successful checkpoints remain preserved: CI34779651605 at `ab921727`
qualifies reader/preview drain before transport and the exclusive-create guard;
CI34776023414 at `3bd92de` qualifies its preceding identity/close corrections.
Their original source, artifact and negative-attempt receipts remain available.

The complete custody design, call-site inventory and additive desktop transport
slice were accepted and read back in comment23326. The transport remains
unselected. Comment23328 records the complete Source reader/lock protocol,
including finite admission, all existing reader methods, consumption ownership,
and the explicit commit-winning reader-death rule. The migration foundation
checkpoint `41fdc91` passed 31 focused functions and independent review of
attempted writer-hold retirement; it does not qualify the whole migration bridge.
The raw-file executor, managed SQL roles and integration/installed parity remain
required. Preview drain repairs from comment23327 are locally qualified below;
no unverified worker exit may authorize resource release or Closed.

The additive transport is locally integrated as `b828834` + `222d897` (authors
`c7fd476` + `8094f514`); all 17 integrated source files match frozen v7. It passes
91 application functions, including 15 transport functions, 16 application bridge
functions, one Lightroom bridge function and one actual CLI transport process
test, plus strict all-target release Clippy and formatting. The actual transport
child test covers Status/Close before opening a catalog; native descendants and
installed GUI behavior are outside this test. Independent final review verified
the clean source and all 26 receipt artifacts:
`sc-22847-catalog-transport-independent-mwbdzs68/review-final.json`, SHA256
`c34fc35f4e418c4d43d4094fb2f9ee0a4e7eb979f406286178dbffe645bd6a21`.
Receipt: `sc-22847-catalog-desktop-transport-q7ofz57b/final-receipt.json`, SHA256
`6584882e25561716d006ca5d4dc1fb5859efd4d958927a059390d3eecda5ec56`.
Mac release/locked Tauri compilation for its hidden child dispatch passes;
session77548 is terminal/reaped. All 17 integrated source hashes still match.
Root integration receipt SHA256
`864469933168431f9e58aff7d37fc1f9631c828e7dd2334864891a1de910dfd5`.
Production State
still uses the existing engine; managed SQL roles and filesystem isolation are
the next required implementation, not completed by transport alone. The earlier
owned-worker PID fixture failure is retained and unexplained; the unchanged
binary passed three isolated observations and the unchanged fixture passed the
full final suite. Shortcut comment23332 and In Progress state were read back.

The SQLite exclusive-create guard is integrated as `aafd442` (author
`cc42d94`). Pinned SQLite 3.51.1 could retry a failed Unix exclusive temporary
open as read-only, then unlink an existing hardlink on close. The one-line guard
preserves exclusive creation. The supported SQLite filename fixture reproduces
the old behavior; the fixed existing-hardlink regression and fresh-file control
pass. Prior identity tests, strict all-target release Clippy and formatting pass.
All five source hashes and the complete two-patch upstream delta were verified;
no natural random-filename race or user-data corruption is claimed. Independent
final review: `sc-22847-sqlite-exclusive-independent-88sgcr8c/review-final.json`,
SHA256 `f2f835b09ada6a372ad1384d19389cfdf72be31ec58e10524cb335953be41237`.
Local receipt: `sc-22847-sqlite-exclusive-1uczt_3n/receipt.json`, SHA256
`20a814e86d0d980f3f2e009c6de0d00809f8c129ec750c23c32daa7a0d09eed6`.
Integrated release/locked Tauri compilation passed in 19.90 seconds;
session57536 is terminal/reaped and all five integrated hashes match.
Windows/Linux qualification passed in CI34783536524 at the published checkpoint.

The complete paired managed SQL session and filesystem bootstrap/restore plans
were accepted and read back in comment23334. They preserve all eight fixed
catalog/manifest connection roles, a private discovery role, explicit admission
confirmation, checked query cancellation, original creation semantics and bounded
restore documents. Implementation continues in isolated worktrees. Filesystem
ownership is a sibling of the catalog child; a failed bootstrap must not run SQL
cleanup against unconfirmed handles. Remaining backup, native worker, publication,
metadata and migration filesystem routes are still required.

The migration reader remains under qualification in its separate worktree.
Source v12 replaced a manually owned SQLite commit-hook context with a connection-
owned closure, retaining its lifetime if Catalog moves out of its lock wrapper.
The first v12 actual-owner attempt failed frame admission; that evidence remains.
Source v13 repaired private authority tagging for the full u128 range and opening
failure frames. Its six proxy functions pass, including actual Source ownership,
cancellation, drain and moved-Catalog death-before/after-commit behavior. The
three closed-reader functions passed on unchanged reader code. Independent final
review verifies 96 committed source pins and 11 evidence artifacts; focused receipt
SHA256 `634286acd0d9b54595d6c76fbbc246883f4bef236943802543d74b2ef98492b8`.
Source v14 passes three maximal-buffer fixtures and uses exact two-pass encoding.
Its object-Issue measurement is a specific representation, not the maximum of
all accepted sequence forms. The Source-specific borrowed Manifest decoder is
qualified at `8909ec4`: two Manifest parity, six budget, six proxy and three
closed-reader functions pass. Root verified 99 committed source pins and 12
evidence hashes in final receipt
`sc-22847-lightroom-migration-bridge-xksnotzh/source-v17-final-receipt.json`,
SHA256 `b1f5354e85aa9f97353f9458bcac0a061376268b87702021ef13b6088f5bce9e`.
Original retained bytes and their digest remain authoritative; decoded views do
not normalize the retained source. Earlier authoring/fixture failures remain
preserved. Numerical aggregate memory admission is still unqualified.

The finite Source opening guards are qualified in clean author commit `6aa0967`;
all 102 committed source pins were verified. Ten focused functions pass. The
large accepted sequence fixture retains 16,777,207 input bytes and 1,290,413
Issues with a 150,994,944-byte Vec capacity, producing 52,908,799 encoded bytes.
This disproves the prior 128 MiB typed allowance; no replacement aggregate
allowance is claimed. Failed compile, fixture and source-manifest attempts remain
preserved. Borrowed supplement selection is the next finite memory slice; other
preallocation and the numerical aggregate remain required. Comment23341 records
the qualified checkpoint.

Supplement projection Part A is clean checkpoint `b037b85`, with all 104 source
pins verified. Five functions pass, including original-parser parity, reserved
RawValue forms, cancellation and an actual reader fixture preserving source
bytes. The 8,388,604-byte synthetic baseline has 299,588 ignored members and nine
final projected nodes; this is not peak-allocation or RSS evidence. Source review
and the two test-only corrections are preserved. Runtime receipt SHA256
`052f802560ee2ec4f26aa19378a82d8458feefed6bff6e57dc4d9284434c6396`.
Whole-branch strict Clippy still fails in eighteen files unchanged from its
pre-Part-A baseline; no suppression or full migration acceptance is claimed.
Part B must account for every accepted representation, including shorter Some
values and serde map/sequence differences. The earlier large fixture is one exact
measurement, not a global maximum or replacement aggregate memory allowance.

The filesystem bootstrap/restore helper is qualified at author `e413c0a`, locally
integrated as `c10d546` plus `b21b13b`. Eighteen worker functions, one configured
CLI helper test, twelve marker tests, strict Clippy and formatting pass. Actual
fault children were reaped; the ordinary-harness helper is excluded. Independent
review SHA256
`38ce64f4352f340031310fba3ed131618ac3a927492284994bd64cb9620a28c5`
and integrated Tauri qualification are retained privately. These results do not
prove the real SQL/filesystem overlap or production route selection.

Managed SQL author `f46ba230` is integrated as `deae865`. The sole initialization
conflict preserves its shared initializer, the filesystem module and LensWorks
diagnostic text. Independent source review verifies 23 of 26 author files are
identical and the three remaining differences are expected branding and existing
filesystem integration. Review SHA256
`31774cff4f7fe55322295032632af0e8ed3896e10804194d6d6712e06298982e`.
Real CatalogFilesystem relay implementation continues under comment23343; FS6
store leases and remaining filesystem routes still prevent production selection.

LensWorks branding is commit `1513ded`: desktop/window/installer/dialog/CLI names
and new derivative CreatorTool change; imported XMP bytes and persistent/internal
identifiers remain compatible. Frontend build and 63 tests plus the Python
metadata derivative test passed. The initial Rust XMP selector selected zero
tests, so its exit zero is compile-only evidence. The corrected run at `deae865`
passes 64 actual XMP functions. Preserved corrective receipt
`sc-22847-lensworks-branding/xmp-correction.json`, SHA256
`7274f5b3276ac882e204bfcb2579ba799ad1eb14ae96f04aaafa38cd08bce1cb`.

Integrated `deae865` passes XMP64, session14 (one inert harness helper),
filesystem18 (one helper ignored), cancellation3 and discovery1, strict all-target
release Clippy, Mac Tauri release/locked compilation and formatting. All eight
gates are terminal/reaped. Receipt
`sc-22847-lensworks-sql-integration-jwt471d5/receipt.json`, SHA256
`49e81608cb1ff1f67b8526922b500960039133f79170cc3cf2489cc2beebda7c`.
Root reverified all eight logs and 26 integrated source hashes. This remains local
qualification; the new batch has no fresh hosted or installed acceptance yet.

Local `db023e1` (author `ef98f82e`) corrects preview cleanup and checked Quit.
Failed native wait retains the active child, scheduler/encoded reservations,
cache owner and complete catalog/import owner in Closing, with explicit retry.
Actor unwind retains the same complete owner if drain fails. Startup transport
errors are latched only after resource registration; partial requests are never
replayed. Native and global Workbench cancellation precede joins, while ordinary
Catalog Close preserves independent Workbench lifetime and direct status/cancel.
All 176 selected test functions pass (two ordinary-harness helper entrypoints are
ignored), plus strict all-target release Clippy and formatting. Actual worker
render/cancel, injected failed-wait/retry/panic and held-shutdown tests are
included. Mac Tauri release/locked compile passes; this is not installed GUI or
new hosted-platform qualification. Earlier export-status and decode-only test
failures and a syntax diagnostic are preserved, corrected and covered by the
final passing gate. No natural OS failure or historical preview cause is inferred.
Independent committed-source/20-artifact review:
`sc-22847-preview-drain-independent-2eq50unj/review-final.json`, SHA256
`fc155b555f96a61f669602b656a572fbb78e7f1f9b8c9cbb866cdcef59729ebd`.
Final core/Tauri receipt: `sc-22847-preview-drain-ehze1aof/receipt-final.json`, SHA256
`07ce0bb6b451877872a0b708aaaa20572a705360df229a62f3a469e9524cf054`.
All local test and compile sessions are terminal/reaped; full process/filesystem
custody conversion remains required.

Local `298b03c` also binds the sealed Lightroom reader's actual opened SQLite
object to its retained Source before any PRAGMA and on every verification.
Old checks failed all three new regressions, including accepting a byte-identical
substitute during directory ABA. Fixed full reader group20 functions, strict
all-target release Clippy and formatting pass. Independent committed-source and
nine-artifact readback:
`sc-22847-migration-reader-identity-independent-iwv0u6d5/review-final.json`, SHA256
`fd3890d2a98adf84e9588d8ca2aac02dfb3998b55b21e1b260c0fa73a810ee08`.
All local native owners drained. This remains an identity-only prerequisite;
shared raw descriptor/failed-open lifetime and external filesystem isolation are
still required. No new installed/RAID/GUI or three-platform runtime claim.
Local reviewed batch now adds Close/lease repair `716b2ad` and actual-opened
SQLite identity `63c7796` (author `91610d13`). All 37 frozen identity files and
committed blobs match, including the upstream ignored lockfile; the full code tree
matches the qualified author tree (only this ledger differs). Three substantive
identity/lock tests plus their subprocess helper, 20 relink tests, five notice
tests, strict all-target release Clippy and formatting passed. The host-filtered
locked desktop dependency graph resolves exactly one local SQLite links package.
Unrestricted offline metadata failed only on an uncached Android dependency; no
all-platform qualification is inferred. Independent review:
`sc-22847-sqlite-identity-independent-c78ncxhr/review-final.json`, SHA256
`ad4d492da6feea7f77e621a464a52ed61cd6e93f6c7b6469786ecedf22f9ba79`.
This is a narrow identity prerequisite. Shared connection/raw-descriptor custody
remains open; the accepted catalog-session helper and filesystem executor plan
is being implemented in isolated worktrees. Hosted identity/Close qualification
is provided by CI34776023414 above.

The earlier reviewed batch
includes copy/relink lifecycle repair `5eca77f`, export actor `8ea4d55`, serialized
Lightroom inspection workbench `9deedea`, and Windows boundary repair `e239268`.
The full export frontend is integrated as `c74e9aa` plus `eb0ef3c`. Independent
review verified the repaired completed-review settings binding and the combined
App merge. All 50 integrated frontend tests and TypeScript/Vite build pass. The
initial 32 and repair 18 private evidence hashes match. Review:
`sc-22847-export-interface-independent-4zfru_0x/review-repaired.json`. Integrated
receipt: `sc-22847-export-frontend-integrated-rbkjseev/receipt.json`. The earlier
batch qualified Mac/Linux; Windows observer repair is now qualified by
CI34773173909 as recorded below. No actual export GUI claim yet.

CI34765013818 is terminal: macOS, Linux and evidence contracts passed. Downloaded
Mac/Linux qualification receipts and installer hashes match; installed synthetic
preview/export workers reaped successfully. These receipts do not claim GUI testing.
Windows passed 434 library tests and failed two before installed qualification.
Root's independently reviewed repair checks native sharing-refusal codes and keeps
only the Windows SQLite manifest representable while retaining native thumbnail and
relocation data paths. Invalid Windows manifest paths reject before root creation;
Unix native manifest coverage remains. Actual Windows validation is still required.
Evidence: private `sc-22847-ci-34765013818-o2vc1r2w/readback.json` and
`sc-22847-windows-boundary-independent-p_1yrz8c/review-format-final.json`.

The combined local batch passed 54 focused Rust tests, strict all-target Clippy and
formatting, plus the separate Tauri picker-purpose mapping test. One macOS native
cache test returns at filesystem EILSEQ92; this function count is not physical
nonUnicode workflow coverage. Picker mapping is not actual native-dialog validation.
The six frozen source hashes match the committed tree. Receipt:
`sc-22847-export-lightroom-windows-integrated-eenm_abb/receipt.json`, SHA256
`bdb2bb0760a4cd80bcf0ae68b512517a2ae273f1e3a7749a23f9f5c8b3c63a89`.

Copy/relink retry and stale-callback repair passed independent actual-hook/gate
verification (nine source and 32 evidence hashes), 43 frontend tests and build.
The full export actor passed independent review and 35 distinct focused tests,
including synthetic PNG export through the application bridge, explicit recovery,
foreground preview preemption, and child drain before ownership release. Strict
Clippy and formatting pass. Its frontend is integrated above; actual installed GUI acceptance remains open.

The serialized Lightroom workbench passed 62 executed tests, strict Clippy and
formatting; independent review verified 12 source and 19 evidence files. It provides
owned discovery/capture/inspection, exact result chunks, explicit family decisions
and selection sealing. The complete desktop bridge `a6dad4e` passed independent review and 29 distinct
focused functions plus strict Clippy, formatting and TypeScript. Reviewed local desktop integration is `ab7a6e1`/
`ff2bf93`/`d2c53dc`, byte-identical to the isolated reviewed UI tree apart from this
ledger. This bridge is in the current PR16 checkpoint but remains absent from v6. Root controller
`0f84f56` passed 54 frontend functions and synthetic actual-hook lifecycle checks:
same-generation reads, hidden panel/catalog changes, stale callbacks, lost starts,
exact Cancel, and ambiguous or typed Close replies. Final delta keeps an accepted
Close pending through Closing until Closed even when its reply is an error. Final
TypeScript/build passes; independent final review verified four source and 36
evidence hashes. Review SHA256:
`199b9aada077f4d1d934095128a84d7d409e2fedf684e1ffaa547ac19ab5e7cc`. This is
not installed GUI proof. The full panel is integrated as `2fabffd`: all 11 actions,
13 queries and five review collections, with App-owned input/status lifetime and
explicit preparation gaps. Independent final review passed after repairing global
ID continuation, linear UTF-8 chunking and BOM preservation. All 63 frontend tests,
TypeScript/Vite and 18 actual-App synthetic snapshots pass. Eleven source/dependency
hashes match the reviewed tree. Review SHA256:
`2de77e3f54b60dd634046f19fd42d8d2d45e15c67569a39bf3267111acd497df`.

The complete seven-command migration PLAN/WIREv4 now supersedes the v3 transfer
mechanism. Exact documents/source delta were recorded in Shortcut comment23315 and
read back (64,147 characters). Independent v4 readback passed, SHA256
`898e6976ceaf7a7c891d6f2d9f6a9ab75c13a54788ad6a9d8510ea183c252b26`.
Pinned XNU source shows that malformed descriptor receipt under FD exhaustion can
release GUI POSIX locks before quarantine exists. V4 removes GUI descriptor receipt:
one owned helper holds the physical import lock and sole SQL executor; GUI owns
process/reap and exact cancellable Writers grants. Source/reader lifetime, alias
poisoning, separate bootstrap grants and complete seven-command/two-repair scope
remain mandatory. Unqualified transfer/fence/spawn drafts were preserved before
removal. The first source compile succeeded; six fixture tests hit macOS temporary
path symlinks before intended assertions. Fixture correction and full implementation
remain in progress. No canonical user migration occurred.

The old v1 app was closed normally. Installed v5 opens the selected16 TEST catalog:
preparation reached Catalog ready, and a known existing 2017 CR2 displayed both a
thumbnail and a large Develop preview. No editing controls were changed. Evidence:
`sc-22847-installed-gui-v5-125d2K/receipt.json`. The 2014-02-21 folder exposes a
separate stale-source issue: its catalog lists 93 photos, but its current directory
contains two renamed DNG files and no `IMG_6997.CR2`. Bounded DNG headers identify
one as converted from `IMG_7058.CR2`; they do not identify the missing IMG_6997 or
other originals. No guessed association or relink was performed. The user question
about conversion/removal is pending. V5 is the immutable `4e40972` checkpoint and
does not contain this newer export/workbench batch.

CI34768145412 at exact `d62fe14` is terminal: Mac/Linux/contracts passed. Windows
passed code/recovery/native-path/RAW/frontend/Tauri checks, then failed the final
observer executable association. Downloaded Windows evidence proves workers passed
and known processes were reaped; Rust's `\\?\` canonical prefix differed from the
Python input spelling. Reviewed fix `67975e0` compares actual file identity, keeps
pre/post SHA pins and literal cleanup verification, and rejects a separate file
with the same bytes. Nine local test functions pass; the Windows verbatim case
requires its native host. Observer tests now run in each platform matrix after
psutil setup. Independent source review SHA256:
`e9e267fd9a9478041babcccb2ce76bf2348594640df3e7c56006b1221f46f342`.
No claim that the failed workflow is green; next batched push must qualify the fix.
Mac/Linux downloaded installer/executable/receipt readback SHA256:
`5a6ade05fd436060a8115e54674a890616841ddf60c05f6184e32ccffe79a4ff`.
The downloaded Mac artifact does not include the observer at the Linux package
path; root independently verified the Linux observer only.

A controlled-metadata prerequisite found a retained-blob allocation gap. Reviewed
`616aa18` admits raw and compressed lengths in one SQL read before allocating the
compressed Rust vector. Exact output length and digest checks remain. All 15
focused functions, strict all-target Clippy and formatting pass; session61932
exited0. Receipt SHA256:
`4cad10fed7cac6a6374cf689140e81f28ea5fdff218e6034973b99cd2d3a0dae`.
Independent review SHA256:
`331e3ebe4097a9d6f7a020b52b7012976144d45a2b7593e6aaaf9bed80cdec9e`.
The prepare/commit seam is now integrated as `0537b29`. It prepares XMP off the
writer, carries early full image identity and an exact shared catalog-session pin,
then revalidates under the commit transaction. Four new tests cover all edit forms,
variant isolation, stale identity/revision, session mismatch and callback rollback.
All 40 affected functions, strict Clippy and formatting passed; the initial empty
organization filter is excluded and its corrected two-test run is retained. Final
independent review SHA256:
`5d923bf9df072136f0ebcaee7c9a91743e742ac227a03d379daf71491de0903c`.
This seam does not establish bounded writer duration or the desktop editor/sidecar
workflow; that complete workflow is being planned from the existing core surface.

The immutable Mac v6 package contains exact `d62fe14` (tree
`14ffdf0ad510912a8bf4f2e26fdd193c68374427`). All eight bounded build/package/worker
gates passed. Root verified 95 package file hashes, 65 evidence hashes, source/tree
identity, and all individually known children reaped. Private package:
`sc-22847-mac-checkpoint-v6/PhotoCatalog.app`; receipt:
`sc-22847-mac-v6-build-2l4msxyj/receipt.json`, SHA256
`1d341ea586ae1cff0a9108357a284ec4960cdd5a18291203ad4e4898abe864f7`.
Root readback SHA256:
`ef5f0d8f3bcc6cc7bda51d1fe617c94bdab6d72fd7f5d4de9fa04d6d9a1b952c`.
This is arm64 development/ad-hoc packaging, not notarized. V6 GUI acceptance waits
for the user to unlock the Mac. V5 PID20724 and executable remained unchanged;
no app switch occurred. Full controlled XMP
editing, sidecar exports, settings/accessibility/platform acceptance and S13 remain
open. Shortcut comments23311 and23312 were read back with S12 still In Progress.

The next integrated code head `2fabffd17e80f0bb3f60550e549ff1d39f1d5773`
passes 63 frontend tests, TypeScript/Vite, six bridge tests, four prepared-edit
regressions, Tauri compile plus the existing export-picker mapping test, and
formatting. This does not validate the new Lightroom pickers. Native
session64501 exited0 and was reaped. All 11 panel/dependency pins remain exact.
Receipt `sc-22847-lightroom-ui-integrated-rzlpj9m6/receipt.json`, SHA256
`c487c47a365d668d1a64fd392a12df4dedf2259a437f85b13803782c6bbfd08e`.
Independent exact-head integration review passed with no findings, SHA256
`e1552e3321ce00601ee3c5cc8be4bb694edd2c23dd5a02930e419754bfe3a378`.
This is combined source validation; installed GUI and fresh hosted CI remain open.

CI34773173909 exposed a close/reopen race at application_lightroom_bridge.rs394:
cached Closed preceded coordinator join and release of the process inspection lease.
Reviewed local repair `716b2ad` keeps Closing until both complete. Its deterministic
regression failed on old code, then seven bridge tests, the complete synthetic
capture/resume/close/reopen integration, strict Clippy and formatting passed.
Final receipt SHA256:
`6525e1d2fbb115d0a5207077f461f29867d08f5c530abeae81e2215b95285ab3`;
independent source review:
`f2c27c32fdfc49dc86a0f2eb2cfc501dbe226f4caca781cce279e76ebf18abf9`.
Native baseline62974 and repaired76727 are terminal/reaped. No retry push yet.
Downloaded Linux installer, executable, observer and worker receipts at0046341
verified; readback SHA256:
`e36b3c99c5db1986138109f5ee991357e84cccaa3f1ff40547a3fd6ea54ba2e1`.
Windows installer/executable and qualified worker receipts also verified, including
accepted verbatim path spelling and both known workers reaped. Windows readback:
`5525afe7400d99af0d711110ebbc741d8788084c858c841994e1b8817b6b8ef0`.
Its observer executable is not in the downloaded Windows payload; no independent
observer hash readback is claimed there. The failed Mac job produces no new Mac
installer qualification. These checks do not test GUI behavior.

New custody findings qualify the earlier integration PASS. Same-process photo
export destination snapshot opens/closes can release a live Workbench Source lock
if a destination aliases its SQLite object; the proposed metadata filesystem paths
share that defect. Review addendum SHA256:
`11a711cad6ce858384237d216e04ec288c24638157a0176a50e185e94c701892`.
A separately owned filesystem executor with the parent SQL transaction/permit held
through child drain is being planned. RelinkWorkerHandle::open_with also opens and
closes three raw catalog descriptors outside SQLite's deferred-close ownership;
existing export/relink callers and new metadata are affected. Unix HAS_MOVED alone
does not compare the expected held inode. The safe shared connection admission
replacement is a required unresolved prerequisite, not permission to remove identity
checks or assume startup is quiescent. Assessment SHA256:
`814a2783399c3cb8a16c7cffb52eb20e2ad98d1783da6511564ce5ed87a71e14`.
Metadata plan v3 remains unimplemented pending that design, including all four
edit forms, sidecars, durable receipts and explicit evidence reconciliation. The
complete final documents were recorded/read back in comment23322 (55,179
characters); schema13 is reserved. Qualified design review SHA256:
`7626b0a77316d6633d83c34640a2b70b8f4d3583b3349d15596aaba56599f768`.
The shared actual-opened identity slice is recorded/read back in comment23321:
one custom opcode in the pinned bundled SQLite returns actual Unix dev/inode
without another descriptor; Windows keeps its supported native-handle check.
This addresses migration LM-F4 and does not by itself solve GUI wrong-object
cleanup. Its isolated implementation and the complete custody design are active.
The migration lock-name finding was retracted after verifying the existing CLI
uses `.lightroom-import.lock`; no desktop-only rename is planned. These findings
are tracked on S12 comment23317; S12/epic remain In Progress, S13 unstarted.

CI34797188873 at81986e3 is terminal success in all four jobs. Root reverified
all41 receipt artifact hashes after the third archive download and full payload
audit; Windows artifact10331355869 is49,698,135 bytes, SHA256
`c826108989e6776de7c0c4f516695b96513451f42e853a7a8a6f1b8c8a988599`.
Final receipt `sc-22847-ci-34797188873-rw8rygeh/receipt.json`, SHA256
`b8d83dd403dee42a455599be99b2df5ff84057548ac5efaf8e56be20c0d65f10`, is
frozen. Included closure files/notices are Mac18/25, Linux4/17, Windows4/71.
The Mac observer remains receipt-only; the separate GUI smoke retains its
limited scope. PR16 exact body/head and Shortcut comment23357 were read back.

FS7 source v1 is frozen for independent review at19 changed/new files, manifest
`sc-22847-preview-io-82489lu8/source-v1.json`, SHA256
`cc24a0680b2af4d9b570e9d0c686567168688516891d51ac7e5a5169a6fee330`.
No native/source qualification is claimed. Review identified legacy pending-name
admission and lost-reply cleanup cases for a bounded correction batch. FS8 and
production route selection remain next required work.

LM test-only capacity v32 review verified122 pins and found two fixture evidence
gaps: cancellation must immediately follow an observed full send, and bounded
escaped/native-path cases must supplement the maximum-roster observation.
Artifact `sc-22847-capacity-v32-review-0ijkm_bd/review-v32.json`, SHA256
`185277f498d4193bd4ca6e226daa1db8c796e371e3710103e317c841d2dea48d`.
Author corrected only two test files in v33; root read both deltas and granted
the initial three exact synthetic tests under the existing serialized native
lane. Frozen v33 manifest SHA256
`0d9e0090f77211fae1f73ee45133bc862aaeda34bf29503df28d84e6ec8fbbec`.
No production/default allocator, protocol or original/RAID operation changes.

FS7 v1 independent source review returned two concrete fixes: retain exact
operation identity through undelivered failures and retire a reconciled known
failure so later reads can recover; preserve legacy pending-name cleanup with
borrowed SQL admission before materialization. Review artifact
`sc-22847-fs7-independent-4ls1kctl/review-source-v1.json`, SHA256
`eec1bda04b920983d457898b5cd1ab482e60bfdde1649cf52f8db7bbdd7256b0`.
No speculative active-step defect was asserted; actual current stream failures
and pre-admission failures were distinguished. Author is applying both findings.

LM v33 initial gate passed allocator and exact-key BTree observations, then
stopped on an invalid maximum-roster fixture (16384 selected plus one inherited
exclusion exceeded the existing combined cap). Root verified seven artifacts
in `source-v33-initial-20260914T030217Z-59508/receipt.json`, SHA256
`c311c2425e26bae4316eea7981cec177fd26468bcc5e22c9661b531bf01cb15e`.
Session53109 is terminal/reaped with no observed descendants. Actual Rust1.98
arm64 node layouts104/200 and280/376, alignment8, combine with the pinned
occupancy/transient proof to bound three validation sets at2,545,584 requested
node bytes. String owners, stack, allocator overhead and RSS are separate.
Independent partial review `sc-22847-capacity-v32-review-0ijkm_bd/review-v33-partial.json`,
SHA256 `ab817dc6c3fd870013f363051fe357a5eee343889ce68e74c0190d01b9453299`.

The v35 test-only correction passes the affected seal test, including exact
16384 selected/0 excluded and16383/1 boundaries and six escaped/native-path
8/16MiB grammar cases. The physical SQLite fixture remains small and unchanged.
Root verified five artifacts in
`source-v35-initial-20260914T030846Z-65270/receipt.json`, SHA256
`827a92a728211647d5433eec239446b020d83a24e247091bc45302711414c8d4`.
Session44531 is terminal/reaped. All122 source pins remain stationary at manifest
`db27cca9d562d658504304486ed1e716ecbd62d49d6a540373a3eb1ec150a353`.
The91,987,768-byte process peak includes deliberate fixture/expected/decoded
copies and is not a production aggregate. Remaining five numerical probes plus
formatting are now authorized serially; the two unchanged passing probes are
not repeated. Whole LM wiring, aggregate reservation and strict Clippy remain open.

The complete test-only LM observation milestone now has eight distinct positive
functions plus formatting, with failed fixture runs retained. v35 consumers
stopped at an expected-size fixture assertion; only one probe ran, no later
four/fmt. Receipt `source-v35-compatibility-20260914T031131Z-66430/receipt.json`,
SHA256 `d9d0056033677b0846ae3d20a6969594a587b8ddf5c0a9511c1fa5336eab78db`,
has five root-verified artifacts. The descriptor fixture correction changes
only decimal byte97 to120 to meet its existing near64KiB assertion.

v36 descriptor and actual custody-consumer probes pass. Accepted descriptors
are1351/57277 bytes; overlapping pending/request scopes and three chunk/Verify
observations are recorded. Retention then failed because its fixture expected
four large records in one saved-record page despite the existing8MiB cap.
Root verified seven artifacts in
`source-v36-compatibility-20260914T031939Z-82927/receipt.json`, SHA256
`73e4d9f8678659ad46c925e8f76e6f20c036f65fb9c157e838b7a1b5b6341f28`.
No production cap or retention behavior was changed by either fixture repair.

v37 checks SQL has four complete records, reads keys1–3 on the first bounded
saved page, key4 after the exact durable sequence cursor, then exhaustion.
That affected test, both actual-process probes and package fmt pass; prior five
successful functions are source-equivalent and not rerun. Root verified eight
artifacts in `source-v37-compatibility-20260914T032444Z-83731/receipt.json`, SHA256
`9709390451f677619c9810a66ae6da1b7e88451600901c25b71a9217464ccde1`.
All122 live/before/after pins remain exact at v37 manifest SHA256
`d0997034c4d669e5dbe2fa655a76b5803345240aead5dce79aadade966ea9b29`.
SQL/raw PIDs88273/88274 exercised64 queries with two caller Manifests and one
raw reader; pipe88430 observed Full then canceled. Known helpers report checked
wait/reap, all runners are terminal, and no observed same-identity descendants
remain. Unlabeled child peak observations101899/108473 bytes stay unordered,
not assigned to roles without independent evidence. Numerical substitution
and full independent closeout are pending; no production aggregate/RSS claim.

FS7 v2 freezes the two review repairs across19 files, manifest SHA256
`fe822e56723153fd22123c03b890e579d4065131deda46e55002e22e30283186`.
Its fixed optional failure receipt binds exact admitted operation/step/request;
transport uncertainty cannot erase the pending request. Legacy pending-name
cleanup uses borrowed SQL metadata admission and the complete packet cap.
Five added regressions bring new FS7 tests to17. Independent repair review is
active, and the private native harness is being strengthened for interruption
cleanup before the105-function initial suite/Clippy/fmt. Source remains frozen;
no FS7 native claim. Shortcut comment23358 and In Progress state were read back.

LM test-only observation milestone is independently qualified and committed clean
as `837ddec0ff70cc3d0fcdec6abb2291ece074f72e` (18-file delta). Root verified
all122 committed blobs. Committed receipt SHA256
`474cea884e701ae9470c49867e1c4f3bde9fb2d07f2718a61ab156b492e49ea0`;
cumulative readback `source-v37-cumulative-readback.json`, SHA256
`80c34a8d82186cdfe2873fec2c22f03db46aba8ac163d6a84a6841c19dbe5caa`,
verifies31 artifacts. Independent final `review-v37-final.json`, SHA256
`07a50f4bff5957f2b5d121ef24e365b6c8fbc60d12018003962c51aff4614ab1`,
confirms eight distinct functions (six in-process, two actual-process), formatting,
source equivalence of carried passes and known helper retirement. No push or
integration into the desktop branch yet.

The corrected inventory `PART-C-NUMERICAL-SUBSTITUTION-v37-v2.md`, SHA256
`affdd72d48eedf4749be22aa7e20bef41c4c9711f771653af83bace44112c742`,
preserves the original report and distinguishes seal dynamic21,364,737 bytes
from complete21,365,041 including its304-byte root. The576-byte Manifest box
exists only while the wire Value owns it; public returned values have separate
inline root placement. These report corrections change no source or runtime
qualification. Remaining Policy/Evidence and constructor/path/protocol owner
terms are being derived independently before aggregate admission/enforcement.

FS7 v2 repair source passes independent review `review-source-v2.json`, SHA256
`5ccf5fab849a77fe281bf21cd9b1cbcff80994d4148f9a8d0686f71ce7c991ee`.
Root strengthened only the private gate harness: v3 adds bounded owned-process
cleanup and terminal evidence; v4 uses current PID/start-matched process groups
rather than stale historical group values. A pure row regression covers moved
groups and PID reuse. All source v2 and prior harness artifacts remain intact.
Runner-v4 receipt SHA256
`eab96308bf57a230f52de26986c0188ad6c54a2e51beee73d45c752a8d41836b`.
Initial native gate is running with standard caps, discovery then105 positive
functions/9 inert ignored, Clippy and formatting; later configured phases are
not yet granted. No FS7 runtime qualification is claimed at this checkpoint.

## Earlier integration checkpoints

The entries below preserve historical evidence and pending-state descriptions at
those checkpoints. Current source, CI and user-app status are stated above.

Cache native-path repair `1f51107` is integrated as `4a51c9c`. It preserves legacy
TEXT rows, stored previews and both relocation phases while writing versioned
native BLOB paths under cache schema5. All54 focused local test functions and
strict Clippy/fmt pass; three physical nonUTF cases remain unqualified on this
Mac due to filesystem EILSEQ92. Independent review verified7 source and14 evidence
files: `sc-22847-ci-native-path-independent-rhr25ssc/review.json`.
CI34762593643 is terminal: contracts passed, all three platform jobs failed.
Windows reached the same obsolete version1 assertion; its log is retained with
the Mac/Linux logs. That assertion and the premature-restore fixture are repaired;
fresh hosted validation and installed Windows execution remain pending.

Latest local integration includes detached export execution `18b6e9e` as `bbf1a67`,
immutable Lightroom selection review/sealing `51a4fa0` as `b28cf67`, copy admission
repair `3742163`, and output settings controls `2140721`. Export's48 distinct focused
tests and strict Clippy/fmt pass; native service7 counts test functions, including
one macOS EILSEQ92 early exit, not seven physical workflows. Root verified12 source
and21 evidence hashes; independent final review is private
`sc-22847-export-execution-independent-s5jqietk/review-final.json`.
Selection passed11 new +17 reader +1 family tests and strict Clippy/fmt; root verified
six source and20 evidence hashes, including the approved final rustdoc-only delta.
It retains exact explicit approval bytes and all-family decisions, rejects observed
source drift, and publishes a new immutable seal at a documented final-CAS decision
point. Live SQLite SHM cache semantics and process-scoped lock ownership are explicit;
the later workbench serializes inspection access. No canonical migration ran.

Copy admission browser checks reproduce an accepted Run with an unresolved reply
leaving Cancel unavailable. Independent status polling now settles UI admission,
keeps late replies from replacing state, preserves stable identical terminal objects,
and fences pre-admission/pre-error reads while retaining uncertainty until fresh
status. Independent review found and verified repairs for both terminal-refresh and
lost-error write-hold races. Evidence: private `sc-22847-copy-admission-ui-vvfj1300`.
Output controls cover the complete current format/depth/size/profile/alpha surface;
43 frontend tests/build and synthetic actual-component browser checks pass. Controls
and controller/bridge integration remain unfinished, with no actual export UI claim.

Draft PR16 remote head is `2698e8690d3daa47d73f7e4f6e5fcff6ede7081b` in the PUBLIC
repository. CI34762593643 contracts passed; all three platforms failed.
The platform jobs expose an obsolete XMP version1
fixture (new native-path version3); Linux additionally exposes a restore fixture
that already rolled back automatically and a real Unicode-only preview-cache path
restriction. Private logs: `sc-22847-ci-34762593643-hlby48a6`. The bounded fix preserves
legacy cache rows/journals through native-path BLOB compatibility in cache schema5;
implementation is integrated above and hosted proof remains pending. This is not a main
catalog schema change. Full export actor/UI and serialized Lightroom workbench
are active next slices; S12 remains In Progress and S13 has not started.


Mac v5 is an immutable local checkpoint of `4e409726c0cce5434e51e10ed730da77cba4da61`.
Integrated worker/capture tests, strict Clippy/fmt, Tauri build/bundle and installed
preview, edited-PNG export and Lightroom capture passed. Installed capture used a
small synthetic committed-WAL fixture: original main/WAL/SHM/auxiliary hashes stayed
unchanged, logical rating3 was recovered, and cancel/Drop/owner EOF all reaped.
Root verified65 private evidence files and95 package/image-root files, including46
app files. Receipt: private `sc-22847-mac-v5-build-mw_p9xb3/receipt.json`.
The app/DMG under `sc-22847-mac-checkpoint-v5` require arm64 macOS26, use ad-hoc
signing and are not notarized. The old app49373 exited normally; bounded actual
user-catalog GUI preview verification is recorded above. Full acceptance remains open.

CI repair `c96f063` is integrated as `7b84a86`: the three-command frontend step now
explicitly selects Bash, so Windows stops at a failed install/test/build command.
Four local command-exit fixtures and independent source review pass. Previous
Windows advancement past this step is not proof that its frontend tests passed;
terminal logs remain required. No jobs, runner matrix or additional workflows were added.


Latest S12 integration: export path compatibility `1da8212` is integrated as
`ed547a4`, backup lifecycle repairs are `58e3c78`, and owned Lightroom capture
`ee83b33` is integrated as `cb10f73`. All three received independent source review.
Export wire formats now retain exact native path units in plans, snapshots, seals,
receipts and worker completion while preserving legacy raw plan bytes and authority
hashes. Sixty focused test functions and strict Clippy pass. Four physical nonUTF
fixtures report the exact macOS EILSEQ92 filename limitation before publication;
they do not establish physical nonUTF export on this Mac. Five representable-path
worker/service workflows ran. Root verified11 integrated source and23 evidence
hashes in private `sc-22847-export-native-path-qnt6co3y`.

Backup browser checks reproduce then repair stale destination/picker/restore replies
crossing catalog or panel scopes, and unusable Cancel behind an unresolved start
acknowledgement. Independent status polling now reconciles admission and owns status;
exact-operation cancellation is separate from review admission. Same-catalog reopen
also releases an obsolete pending restore-review callback. Actual-component synthetic
browser evidence is `sc-22847-backup-ui-gilg1rzz`;38 frontend tests and build pass.
CI34760079137 at90e4041 is terminal. All platforms reported obsolete frontend
lifecycle-key expectations;58e3c78 checks stable ownership through indexing, distinct
sibling keys, and replacement across catalog sessions. Windows passed the earlier
original-observation regression and its native library/CLI stage, then failed in
installed-worker observation. The digest-verified Windows artifact proves a psutil
WindowsPath TypeError before the first telemetry sample; temporary-directory cleanup
then masked it with WinError32. Repair9aa8564 is integrated as8bdbcda: all five affected
qualification helper calls use os.fspath, and caller cwd/environment restore precedes
temporary-directory deletion. Fifty Python tests pass; original-helper regressions
fail as expected. Root independently reviewed all8 source and22 evidence hashes in
private sc-22847-windows-worker-pfoyscb1. This proves the observer defect, not installed
Windows worker execution; fresh integrated terminal CI remains required.

Owned Lightroom capture adds bounded create-new request/result transport, pre-source
request digest admission, child-only original handles and POSIX byte locks, and an
EOF ownership lease with explicit cancel/Drop kill and reap. Hidden CLI/Tauri dispatch
runs before app initialization; the legacy capture-worker protocol remains available.
Five focused and24 Lightroom integration tests plus strict Clippy/fmt pass. Private
`sc-22847-lightroom-capture-worker-hdne55hk` retains source, failures and receipt.
This is a prerequisite: installed helper execution passed on Mac v5; the full
desktop Lightroom workbench and photo-export/metadata-write workflows remain open.
No actual user catalog was recaptured or migrated, and no original was changed.


S1–S11 are Done. S10 PR #14 merged as
`287d3382538b10717bf943553c73fc29a300ffd6`; final and merged CI passed.
The selected16 migration remains a scratch TEST, with no canonical migration
or Adobe renderer parity claim. The user has opened this TEST in the first local
Mac checkpoint app. Read-only diagnosis verified a selected original exists at
its recorded path while its migrated physical asset remains pending.

S11 PR #15 merged as `e43997aad29bafc90f079fa2954df9f6db084266`, identical
to independently reviewed `6105679`. Final PR CI `34742822480` and merged CI
`34743768864` passed all four jobs (macOS, Windows, Linux, evidence contracts).
Shortcut sc-22846 was read back Done; comment23286 records acceptance and limits.
Its whole-database backup, verified new-destination restore, interruption safety,
XMP/migration retention, restored-job holds, relink and preview regeneration are
covered by [Catalog backup and restore](CATALOG_BACKUP.md). This does not claim a
measured backup of the73GB TEST catalog.

S12 sc-22847 is In Progress on `codex/sc-22847-desktop`. React/TypeScript/Vite
and Tauri implement the first grid/editor integration; PhotoCatalog remains the
working name. Actual filesystem folders across all years share one catalog.
The UI-independent Rust application actor owns typed bounded commands, precise
paths/revisions, foreground priority, preview scheduling and worker lifecycle.
Draft PR #16 remains open on the
PUBLIC repository. Run `34751855171` finished: contracts passed; Linux failed
late JPEG XL discovery after Python setup replaced PKG_CONFIG_PATH; Windows failed
notice collection; macOS package/installed-worker checks passed but evidence upload
failed. Linux environment repair `d242842` is local; remaining platform failures are
repaired by `be54877` integrated as `e762c1d`, with 37 Python tests and independent review.
Run `34754042585` finished with contracts passing and three platform failures:
macOS exposed a scheduling race in the backup cancellation test; Linux and Windows
reached installed payload verification, which rejected Tauri's package marker patch.
Captured installer bytes on both platforms prove the only change is the pinned
first marker UNK→DEB/NSS. The local repair predicts that exact whole-file digest
before bundling and still rejects every unrelated byte change. Eleven staging
tests, the deterministic SQLite cancellation regression, strict Clippy and independent
source review pass. Private evidence is `sc-22847-ci-marker-backup-gate-v2` and
`sc-22847-ci-34754042585`. Run34755832775 at04d6913 passed macOS,
Linux and evidence contracts. Windows built and inspected the NSIS payload but
failed before worker launch because its environment snapshot uses SYSTEMROOT,
while the smoke launcher looked up SystemRoot. Repair7fd2d81 normalizes Windows
environment keys and uses exact Windows path syntax;45 Python tests and independent
source review pass. Fresh Windows installed-worker qualification remains pending.
Run34758374938 atc53fbe5 is terminal: contracts passed; all platforms failed the
schema4 organization fixture because it omitted the shared schema12 teardown.
The one-line test-only repair233569f passed all23 organization tests and independent
SQL/source review. Windows additionally timed out in the original-observation test;
its unqualified channel error does not identify the failed boundary. Test diagnostics
now report the exact request or checkpoint and worker status without relaxing deadlines.
The focused local test passes; the Windows cause and installed-worker gate remain pending.
Local import `557c0ff` is integrated as
`eebc6e5` after 85 focused tests, strict Clippy and independent source review.
Root backup dispatch/UI, search/import controls and loading feedback `6e21ff0`
passed 24 native bridge/actor tests, strict Clippy, Tauri check, frontend build and
seven frontend tests with an independent source review.

The actual macOS checkpoint's dependency closure and installed preview/export
workers passed local validation. Its linked native libraries require macOS 26;
macOS 12 compatibility is not established. Signing-sanitized tooling now forces
ad-hoc local builds; two earlier unintended signing/notarization attempts are
recorded in Shortcut comment23288. No GitHub release was published.

User testing reached Catalog ready after initial indexing, but exposed pending
Lightroom originals being called unavailable. Initial physical preparation
`a1d5500` is integrated as `47551b4`: 46 distinct focused tests passed, including
variant-specific pixels, translated import recipe history, retained metadata/XMP,
missing originals, held-reader cancellation and seven crash cases. Exact v4 source
review passed independently. Preparation reads one selected original at a time;
its result establishes physical readiness without replacing recipes or paths.
Root integrated 14 bridge + 9 organization tests and strict Clippy pass. Mac v3
package at e762c1d passes frontend22/build, Tauri build, dependency closure and
installed preview/export worker checks. Actual RAW GUI validation is still pending; the Mac
was locked when Computer Use attempted it, and the user has an unlock request.
Mac v4 atc53fbe5 also passed build/bundle, dependency closure and installed synthetic
preview/edited-PNG export. Root verified46 installed file hashes and22 evidence
artifacts. The app/DMG under private `sc-22847-mac-checkpoint-v4` are immutable,
arm64 macOS26 debug checkpoints, ad-hoc signed and not notarized. The original user
app is unchanged; Computer Use still reports a locked Mac.

Schema11 durable zero-position collection projection `97e0394` is integrated as
`01af9b4`: 50 focused tests plus query-counter fixture and strict Clippy passed;
independent review verified all source/log hashes. Bounded persistent initialization
prevents a full membership scan before a positive-only first page. Root dispatcher
and bounded single-collection lookup passed integrated gates and source
review. Organization UI review identified transient-indexing unmount and false
end-of-page on errors; both are repaired in `5ab1560`, integrated as `77b58c0`, and independently
re-reviewed PASS with all 11 source files matching root exactly. Older recorded
drive paths remain unchanged; the new relink workflow has not been used on the user catalog.

Compatibility badges now distinguish translated, retained-only and untranslated
Lightroom settings without asserting Adobe appearance equivalence (`72b0e41`).
Metadata adapters15330a6/f4f4e57 expose selected-variant effective values,
conflict resolution, source/packet/observation/decision provenance, exact retained
bytes, immutable Lightroom graph history, actual source columns and Adobe settings
interpretation. Native-copy ancestry follows immutable same-asset predecessors.
Four metadata UI findings were repaired and independently re-reviewed.

Relink core5fc7b5d introduces schema12 with detached bounded candidate preparation,
explicit association acknowledgement where no historical digest exists, exact source
fences, cancellable apply/undo, pinned existing database workers, and hydration lineage.
Its58 focused tests and strictClippy passed independent exact-source review. Adapter
2214d55 plus sparse-scan repair8e38d9b owns status/cancel and worker drain, preserves
cached browsing during write holds, and bounds Rules scans by candidates examined.
The10k-row sparse-exclusion regression and native imported master/copy hydration/undo
passed. Legacy plans report counts unavailable but retain strict lineage-checked undo.

Desktopdaa9dc2 adds Metadata & XMP and Locate originals workflows. All36 frontend
tests/build and bounded independent reviews pass. Synthetic browser scenarios cover
history continuation, failed-read retry, opaque provenance, explicit whole-plan
confirmation, commit-winning cancellation, undo, and stale picker/cancel replies.
The post-apply image refresh binds catalog, request generation and selected-row identity.
Root61b45f8 validates selected-copy metadata revision and session isolation and the
schema12 downgrade fixture. The combined gate passes41 actor tests,15 native bridge
tests and strictClippy; independent integration review passes. Its held transaction
permits metadata reads and rejects conflict-resolution writes. Duplicate React keys
on the retained-column and settings-path siblings were found in the browser console
and repaired with distinct anchor-qualified keys; the narrow review and build pass. These are local source/fixture results, not installed user-catalog
or canonical migration acceptance. Computer Use rechecked the Mac: still locked;
the old checkpoint remains running and unmodified.

Adjustment-copy adapter80eb2a5 exposes all seven groups through immutable source
inspection, bounded named target/job/result pages, explicit append/seal/run/resume,
and scoped queue-bypass cancellation. One target runs per background opportunity;
the reviewed priority fix yields to selected previews and byte delivery too. Six
adapter, nine actor and seven core tests pass with strictClippy and independent review.
The actual held-relink/copy-cancel/Close test verifies drain before durable cancellation
and no automatic resume on reopen. Root integrated six copy tests, the original-location
test and strictClippy pass with exact source hashes unchanged.

Frontend5ad942f provides page-scoped target selection across one persistent batch,
source groups, review, progress and per-target outcomes. All38 frontend tests/build
pass; independent review fixes stale canceled-job state, catalog-owned refresh holds,
and lost Run reply recovery. Synthetic browser evidence covers seven groups, exact
large revisions, lost append replies, cross-page appends, explicit execution and hidden
cancellation. The actual App close-race fixture holds Close, completes copy, acknowledges
Close, then opens another catalog; all11 recipe sliders are enabled. Seven namespaced
React keys remove the observed sibling collisions, with narrow independent review and
no duplicate-key console output in the repeated App check. Private evidence is
`sc-22847-copy-ui-45uny_am`, `sc-22847-copy-close-ui-trxv5o70`,
`sc-22847-copy-independent-j_66flda` and `sc-22847-copy-integrated-pz_h936r`.

S12 remains incomplete. Lightroom discovery/capture/dry-run/import/reconciliation,
full export and controlled metadata-write workflows, backup
settings, preview-cache/color settings, accessibility, user visual review, and complete
installed three-platform workflows remain in the agreed foundation scope. Existing
organization, metadata, relink and adjustment-copy implementations still need integrated installed
acceptance; their local tests do not close S12.
Export native-path compatibility is integrated as recorded above. Detached cancellable
export execution and immutable Lightroom selection sealing are the next bounded
implementation prerequisites; their actor/UI workflows remain unfinished.
S13 remains the sole terminal integrated readiness/scale campaign after S12 is stable.

## S10 pre-merge checkpoint — 2026-09-13

This section records pre-merge evidence; Shortcut holds the live delivery state.

S10 sc-22845 is In Review. S1–S9 and the linked inspection repairs are Done;
S9 merged at `56aa3ca`. The selected TEST imports all 16 approved source catalogs
into one scratch catalog and preserves the existing filesystem folder hierarchy.
The 32 excluded catalog candidates remain excluded. Canonical-library migration
has not been authorized.

PR #14 remains draft at the qualified implementation `6c4e140`. All 643 optimized
Rust tests passed (five existing ignored), along with formatting, strict Clippy
and all four jobs in CI run 34733952105: macOS, Windows, Linux and benchmark
contracts. Original import `5b54d2b9`, projection fix `46f56423`, current-settings
repair `063d011a` and the schema-9 `7fda5ebd` upgrade/API evidence retain their own
source attribution.

The selected TEST completed import and both resumable repairs. Keyword recovery
replaced and verified 5,050 archived items: 156 dictionaries, including 140 named
keyword source records and 16 structural roots, plus 4,894 image memberships,
with 16 fresh capture reports. Current-settings recovery preserves 215,708 archived outcomes:
215,671 container rebindings and 37 unchanged. Rebinding is not translation;
215 current settings are translated with appearance gaps and 215,493 remain
retained-only. Imported candidates preserve source terms without selecting metadata
precedence. All 429 retained collection memberships point to explicitly unsupported
module collections (321 print and 108 unsaved-book memberships).

The final full post-keyword preservation audit passed in 491.067 seconds using
2,392 queries and 43,372,523,371 bytes of cumulative processed metadata. Its private
correctness owner sampled 1,204,813,824 bytes peak RSS under an explicitly admitted
2 GiB allowance and reaped the child cleanly. Earlier 1 GiB audit failures remain
preserved; this allowance is not evidence for the product's 4 GiB performance
contract. The audit covers the complete original/current/keyword custody and
association chain, including all repair archives and fresh reports. It does not
rehash every original or retained raw blob and makes no Adobe appearance claim.

The final public API probe and independent saved-result review passed. It read
all 140 named keyword source records and their paths, 16 structural roots,
156 dictionary predecessors and two membership predecessors, plus separate
master/virtual candidate, effective-value
and provenance observations. The native probe took 0.797 seconds; its owner took
1.181 seconds and sampled 274,104,320 bytes peak RSS within the 512 MiB limit.
The main database stamp and catalog checkpoints were unchanged; the admitted
Catalog::open WAL/SHM lifecycle completed normally. These two images use different
physical assets, so this is not an actual same-file sibling or rendering test.
Earlier schema-9 API evidence covers current/history/before-settings routes and
literal folder paging; final-source fixtures cover shared-file independence and
validated native rendering.

S10 acceptance evidence is complete for the approved selected TEST: final native
reconciliation, full preservation audit and representative API readback all passed
with independent review. The story is In Review, not Done. Remaining delivery is
merge, terminal merged-head CI and tracker/task read-back. Canonical migration,
Adobe appearance equivalence and product performance qualification are not claimed.

The selected TEST has reached native Complete in one scratch catalog: all 16
capture reports, 22 source-walk stages and 171 companion artifacts reconciled.
Selected-source custody contains 14,025,415 records. The final progress counter
is 3,452,770 processing steps across repeated metadata passes, not unique photos.
Originals and source catalogs remain read-only. Native completion is separate
from the independent destination audit and the representative public-API check.

Repair `e615b076` supplied the complete composite key for two metadata mapping
queries and passed independent review plus a 20,000-row bounded-work regression.
The resumed worker completed both affected metadata stages. A subsequent external
RSS-probe timeout and a later native rating-conflict failure are preserved as
separate failed intervals, each with its durable checkpoint.

Repair `30f8cd40` retains conflicted ratings and labels as image-local metadata
candidates without selecting source precedence. Keywords accumulate a source-owned
set, with explicit unresolved and resource-limit results. Independent source review,
13 focused regressions, all 594 optimized all-target test instances, strict all-target
Clippy and formatting passed; five existing tests remain ignored. The original
interactive conflict guard and successful component receipts remain intact.

The same TEST resumed with that qualified successor. A bounded read-only check
proved recovery of the exact failing row: its rating candidate is retained, ambiguity
is still visible, prior rating sources and choices are unchanged, and the saved flag
and label receipts are identical. The checkpoint had advanced beyond 2,149,840
processed rows. This is a successful repair/resume observation, not terminal import
acceptance. Earlier native receipts remain attributed to their original source.

That interval later stopped at 2,208,017 processed rows when the XMP wrapper
treated an empty scalar as a null composite-construction argument during a label
edit's preservation check. The source label was empty and the incoming value was
Red. The checkpoint, original models and exact error are preserved. Repair
`94ebee0b` reproduced the error in a synthetic fixture, fixes the wrapper's empty
scalar handling, and preserves the existing composite and semantic guards.
Independent source review, 37 focused tests, all 599 optimized all-target test
instances, strict all-target Clippy and formatting passed, with five existing
tests ignored. Run 12 passed the exact failing row: the effective label is Red,
rating remains zero, all three organization receipts match the completed source
row, and both original model descriptors, projections and blob checksums are
unchanged. It then cooperatively stopped at 2,254,865 processed rows to serialize
native qualification of the preview-cache repair. The worker and owner exited
cleanly, and the saved checkpoint was read back.

Repair `e7bfb9c9` explicitly releases acquired preview manifest, tier and relocation
locks when ownership ends, including failed initialization. A deterministic
retained-handle test reproduced the lock-lifetime defect; its involvement in the
earlier Linux CI occurrence remains unproven. Independent source review, 72
targeted preview tests, all 604 optimized all-target test instances, strict
all-target Clippy, formatting and the release build passed, with five existing
tests ignored. Worker teardown and relocation ownership remain intact. Run 13
and run 14 continued the same TEST; input, policy, schema, successful receipts
and resource limits remained unchanged.

Final reconciliation exposed two query costs. Repair `5366ed23` counts completed
retained records through a covering total minus the scoped incomplete count,
without fetching every completed payload row. The original diagnostic exhausted
30 seconds during the first large counts; the candidate destination-query
sequence completed in 24.13 seconds. These private-runtime timings omit source
and late reconciliation work. The native successor then completed all 16 capture
reports, proving the count repair on the selected dataset.

Repair `5b54d2b9` replaces the final full-population DISTINCT with bounded indexed
capture-key seeks in one read snapshot. It preserves selected/excluded checks,
incomplete-row inclusion, the terminal empty seek and the final mapping-epoch CAS.
The actual diagnostic returned the same 16 keys in 17 seeks after the original
query hit its 30-second limit. Synthetic tests verify scope, pending/extra keys,
constant VM work across duplicated records and concurrent-insert snapshot behavior.
The resumed native worker completed its final step and exited cleanly; the
read-only checkpoint exactly matches the terminal Complete result. Earlier run
receipts retain their original source attribution.

The independent v16 metadata audit passed on the completed TEST in 350.776 seconds:
1,641 queries, 2,807,048,000 approximate VM steps and 856,309,760 bytes peak RSS.
All 16 reports, selected custody, 171 artifact members, 20 supplemental proofs,
native relationships and literal folder ancestry passed. This checks metadata and
the admitted custody chain; it does not independently rehash every payload or render
actual photos. Earlier time, VM and RSS stops remain preserved. The final verifier
repair projects only slot/state before grouping: SQLite's previous sorter included
the full metadata result. Typed-output comparisons and query-plan review qualified
the change. Final allowances are 900 seconds, 1 GiB RSS and 10 billion VM steps;
the data checks remain intact.

The resulting census exposed a product defect: all 215,708 current-develop receipts
were retained-only, with no translated native recipe. The coordinator always passed
an empty settings path, while sampled successfully parsed Lightroom payloads stored
their settings beneath the outer `s` assignment. A bounded sample across all 16
captures also contains different process versions and nested historical properties;
those must not be selected by searching for convenient parameter names. Task 23166
now explicitly tracks grammar-proven container selection, full importer/render/replay
regressions and a guarded resumable repair of existing current projections.

The pre-repair audit remains valid evidence of the old state, not editing acceptance.
The schema-8 repair now archives old receipts and reports, rejects adoption after
later user edits, and commits each replacement recipe/receipt/archive/cursor in one
transaction before fresh reconciliation. Independent review found and corrected an
unchanged oversized-identity row that could stall repair. All 620 distinct Rust test
instances passed across the optimized all-target run and corrected library rerun,
with five existing ignores. The initial CLI SQL type error and old schema-7 test
assertion remain recorded as failures preceding their fixes. Strict all-target
Clippy and package formatting passed. A quiescent APFS clone of the actual TEST
database preserves its completed progress and epoch 424799 before repair; the
source database remained unchanged. This recovery copy does not close S11.

Source `46f56423` passed all four hosted CI jobs in run 34720501392. Actual TEST
repair examined all 215,708 records, rebound 215,671 proven settings containers,
and preserved 37 unchanged entries. All predecessor receipts and 16 reports remain
archived. Reconciliation committed its first report, then stopped with SQLite
interruption; one bounded unchanged-worker resume reproduced the interruption.
Both workers were reaped without forced cleanup or an owner resource stop.
Read-only checkpoints confirm the same capture index 1, epoch 424799, archive
counts, and unchanged database content stamp after the failed resume.

The destination-only query trace passed in 11.619 seconds and approximately
34.286 million VM steps, but omitted source admission/counts and final operations;
it does not establish the native failure's cause. Operation-specific error context
now identifies source admission and each reconciliation query boundary, with SQL
and limits unchanged. Independent source review and 33 focused tests passed,
including original SQLite error identity and report/cursor rollback. The
instrumented attempt located the interruption at 30.002 seconds in supplemental
custody/projection reconciliation for capture index 1. Its checkpoint remained
unchanged. A bounded read-only comparison of all 14 supplemental items in that
capture showed the original lookup choosing the table-name index; selecting the
existing exact-source index returned identical results with much less work
(0.524 seconds / approximately 3.222 million VM steps versus 0.006 seconds in the
warm Python diagnostic). The query now selects that existing exact-source index
without changing schema, predicates, limits or checkpoint semantics. Seven
reconciliation, five supplemental and eleven importer regression tests passed; the native
VM-step test stayed at 38 steps with 8 and 4,096 unrelated files, while the old
index plan grew to 20,517 steps. Qualified source `063d011a` then completed actual
native reconciliation: repair 05 finished all 16 report positions in 116.259 seconds,
with 215,708 examined, 215,671 container-rebound and 37 unchanged records. The owner
reaped the native worker at exit zero with no cleanup events or resource stop.
Container-rebound counts are not translated-recipe counts. Source catalogs and
originals remain untouched.

The full post-repair v20 audit stopped cleanly at its 900-second observer deadline,
after base custody and native-relation checks but before finishing archived recipe
verification. Its failed receipt remains preserved: 1,649 queries, approximately
2.827 billion VM steps, 17,058,440,223 processed metadata bytes and 864,010,240 bytes
peak RSS. Completed report observations classify 215 current results as
translated-with-appearance-gaps and 215,493 as retained-only; they are not a full
repair-audit PASS. The finite 48 GiB cumulative processing allowance was based on
measured archive charges and is distinct from the unchanged 1 GiB memory limit.
A bounded sample of 32 archive rows from each selected revision confirmed exact
composite indexed lookups and unchanged database content. It identified redundant
JSON parsing in recipe validation. The reviewed successor reuses parsed values
without changing validation and adds bounded timing/progress telemetry. Its full
v22 audit passed in 853.453 seconds: 1,650 queries, 43,212,343,101 cumulative metadata
bytes, approximately 2.864 billion VM steps and 992,329,728 bytes peak RSS. All
215,708 archives, 16 predecessor/fresh reports, stored recipes and custody/relationship
checks passed; the worker exited cleanly. The exact current classifications are
215 translated-with-appearance-gaps and 215,493 retained-only. The reviewed walltime
allowance was 1,800 seconds; the audit finished within the earlier 900-second limit
on this run. Neither warm timing nor parsing improvements alone are asserted as
the complete explanation for the prior timeout.

Actual API case selection exposed a separate lookup defect: every selected capture
has ancillary entity records with a non-text local key. The prior public lookup
used collection-wide key availability, allowing unrelated records to block valid
image/history anchors. A bounded census confirms those rows have known source and
table keys, so a known mismatch can safely exclude them from a queried endpoint's
completeness proof. The correction must retain unknown possible matches, snapshot
and ambiguity checks, and use indexes to avoid repeatedly scanning the unavailable
population. Schema-9 adds seven partial indexes that retain unknown keys and
checks at most eight exact value/NULL branches per query. All 36 focused tests,
strict all-target Clippy, formatting and independent source review pass. The
20,000-row regression checks bounded VM work across every lookup shape; migration
failure rolls back both DDL and schema version. Source `7fda5ebd` passed 627 release
test instances and all four PR CI jobs. The actual TEST's quiescent schema-8 APFS
safety copy remains preserved. Its shipping CLI completed the index-only upgrade
in 147.409 seconds with 587,022,336 bytes sampled peak RSS. All seven index
definitions match; pre-existing schema objects, import/repair checkpoints, mapping
epoch and 16 report digests are unchanged. The schema-9 database grew by
228,605,952 bytes. This reviewed DDL-path and checkpoint evidence is separate from
the earlier full schema-8 audit; it is not a new complete payload hash.

The organization audit accounts for 4,894 retained keyword memberships and 429
retained collection memberships without native endpoints. A bounded dictionary
diagnostic checked all 566 dictionary ledger rows: 390 native collections, 20
retained collection types, and 156 retained keyword records. Exact retained-row
proofs establish 16 typed Null-name/Null-parent keyword boundaries. Their rejection
blocked 140 named keywords and the keyword memberships. A narrow boundary adapter
is independently reviewed and locally committed as `0ec76399`, with 25 focused
tests and strict Clippy passing. Atomic archived recovery of the completed TEST
is source-qualified and awaiting actual execution; it must preserve original custody, current-settings repair
archives, recipes and local choices, and publish fresh reconciliation reports.
The 20 retained collection records are Lightroom print, slideshow, book and
web-gallery constructs. An exact endpoint diagnostic verified all 429 memberships
in 1.787 seconds: every image has a native mapping and every collection belongs
to that unsupported roster. Its 9,090 queries processed 8,000,016 metadata bytes
without changing the database stamp. No unresolved or missing image endpoint
remains in this population; independent actual-result review passed. The affected
memberships use two print collections (321) and one unsaved book collection (108).

The schema-9 public API probe passed for three selected cases in 0.256 seconds:
a translated master, virtual-copy history and earlier settings, with matching
recipe identities and four literal filesystem folder pages. Independent review
verified the exact source, selection, process cleanup and unchanged main database
stamp. Empty WAL/SHM companions were removed on normal close. The history endpoint
remained addressable but its selected payload was not parsed. Exact retained-cell
inspection confirms a 14,760-byte Blob, which the text extractor preserves without
interpretation. All 16 root-table digests are now available for recovery admission.
A fresh schema-9 APFS safety copy passed exact checkpoint and epoch readback before
keyword recovery; it is not a full restore or payload-hash qualification. Independent
review verified the backup and retained-cell diagnostic. These bounded
observations do not claim actual image rendering or complete migration acceptance.
The atomic keyword recovery source is locally committed as `ee723c15` and has
passed independent review, 154 distinct associated tests across preserved runs
and focused corrections, strict Clippy and formatting. Tests cover exact master
and virtual candidate terms, effective local choices, source-parent guards,
reopen/resume, pending-operation fencing and atomic archive/ledger/report rollback.
An actual read-only predecessor pass bound all 5,050 dictionary/membership records
and 16 complete root proofs in 246 queries and 0.119 seconds. The recovery request
is held pending full release qualification; no keyword replacement has run yet.
Keyword recovery, full post-recovery audit and public readback, final acceptance review,
merge and terminal merged-head CI remain required for S10.
S11 backup/restore and S12 Tauri interface retain their S10 dependencies;
S13 integrated readiness follows both.
Fieldbook remains a design reference, not the product name.

## Historical checkpoint before S9 closeout

S9 sc-22844 remains In Progress. All selected extraction and TEST finalization
are complete and independently reviewed: 16 chosen catalog families, 209,091
file references, 6,617 virtual copies, 68 enumerated path-overlap pairs, and 6,112
indexed source-evidence pages. The original files are unchanged within the
recorded inspection proofs; this is a dry run, not a migration.

The complete shared-ID list now contains all 8,271 pairs across 120 selected
revision pairs and 130 page calls, including 120 terminal empty pages. All 1,000
historical sampled rows reconcile and four old/new family comparisons match.
Root session 96292 exited 0 and was reaped after 468.75 seconds; result SHA
`38c6d2d176d746bcbd8e0e9f937eacab6019ff81a79036bf5f401e8e2cde7a08`,
index SHA `204f3049a3dd70bd426a4bc4825211a5174a33f4957625cf638fc7664c3096c5`.
Independent actual review passed: `5730310661ed6bcd0260f1da135832fe24daca892817441961cdd50eda33c01e`. The earlier 120-second query timeout remains
preserved as failed. The repair uses existing global indexes on both join sides;
zero-byte WAL and 32 KiB SHM were preserved before normal owned-plan reopening.
No extraction or family choices were repeated.

Bug sc-23153 is fixed locally: all 20 affected PSD references now pass, with
unchanged full source digests and all 40 raw/decoded baseline payloads matching.
Independent actual review `433a3b06e6905338879f3e6ff67a1e8c53e50c034c24537d0ab86d337930f131`
binds the separate supplemental evidence. Historical plan counts are preserved.
The final private readable report is complete, SHA
`99fda86fb63d7ce49bb87db685ff94771a97a390634498978b7564f60e0e819f`,
with independent review `6970985af414c57c2fe7874da8fd1f98ef5f5470781d3272cfd0f811f8c114c2`.
It is at `PRIVATE_RESULTS_ROOT/sc-22844-private-migration-review-1seycgbk/REVIEW.md`.
PR #11 is ready for its final delivery batch; current-head CI, merge and tracker
closeout remain required. S1–S8 and bugs sc-23102/sc-23122/sc-23137 are
Done; S10–S13 retain their recorded dependencies.

## Historical checkpoints

The entries below describe earlier states and are superseded by the current wave.

S9 sc-22844 remains In Progress. Corrected FULL and metadata-only PATHS are complete and independently audited. PATHS session16784 exited0/reaped after3,481 commands, next27152:1,006,834 references,301,117 available and705,717 missing at their stored paths. Result `9c53960818b6d52f7e2e6a8ea610c6186eb6cc2c3e52c9193a9814fd6b044232`; output `105b0e7ac5f36497888c55e7d687e026e3337147cd97295eb5a6cc2b137c91dc`; independent terminal review `2b681bb0a4cc4b4448b267641eb503e5db815386e6f8497a90d374f86180e434`. No failures/cleanup. Terminal audit source v2 corrects sparse native report-state counts,13 regressions and root source review passed; actual audit completed5.84s/72.6MB sampled/reaped0,49.96MB metadata. No packet phase, family choices or migration yet.

The sidecar metadata scan completed and was independently verified: session 36036 exited 0/reaped, 24.853 seconds and 48,431,104-byte peak, all 47 revisions and 4,027,336 literal stat calls. Measurement `12d07f0e51963781ae1e55fc38ae6d2af6a61c44589ec3fae7324bc1b9d70858`; actual independent review `f6875da60cec50a79c6714b57deaed28e645a7c7e0bb8df8d5ceb598d0645d40`; owner result `c97c447b2eaa30b577cdf6140511937bbc1d2d7596324e30deeba86ddbd62d22`. The 301,117 available original references total 15,160,542,029,800 bytes; 256 sidecar occurrences total 846,812 raw bytes plus the same separately decoded bytes. No photo/sidecar body or database reads. All inspection processes are reaped. Do not rerun completed FULL, PATHS or sidecar measurement.

sc-23137 is verified In Progress and blocks sc-22844. Source analysis exposes 30.32 TB of repeated whole-file hashing before parser reads; this is a derived read volume, not measured runtime. Hubble implements the focused single-pass source-proof fix off main9dbcc76 in `sc-23137-xmp-single-pass`; Hooke assesses explicit successor-native qualification without changing historical bindings. Packet funding remains preparation-only with unresolved embedded payload/projection allowances. No packet grant, extraction, family choices or migration. Exact continuation is in PAUSED_HANDOFF.md; S9 comment23138 and bug relation read back verified. PR11 remains draft at80d56a0/all4CIgreen; frozen execution is unchanged.

Current-catalog scope revalidated: user wants one current member per family, while all-47 external packet extraction is frozen runner sequencing. The private `sc-22844-current-catalog-proposal-9oqhobxn` presents all 48 candidates and 16 proposed choices from saved PATHS metadata; independent review passed (`b4ef4d4fae9267c7cb4a632662eeea510bc47b9c38358e54f50d320e106c5373`), user choice pending asynchronously. No selections or source mutations applied. S9 comment23139 records the explicit successor roster/attribution requirement.

sc-23137 candidate `f68f78e947d59af91d9ac51c55986f9cab62391e` is in draft PR13 and tracker In Review; final CI/source review/merge pending. 398 local tests passed at39012d5, final change only removes a needless return; strict Clippy/fmt pass. Receipt `2724dd0b82cc5dec5e35ff569a031a7e64ea103cba708a3b5ef30a99d74ac308`. Broader Unix ctime was rejected after concrete kernel precision evidence; current one-pass eligibility is held local APFS or Windows deny-write/full-ID proof, conservative elsewhere. Reference RAID APFS confirmed read-only. Native lane released, all local sessions reaped. S9 selected execution adapter implementation continues in isolated `sc-22844-selected-packets`; no actual choice or phase grant.

PR13 current head `aa21c2453f3a16cc4423124207ad6ad7ad30b1a7` is ready, source reviewed, final CI34653155165 running. Previous f68 CI34652404977 passed Mac/Windows/benchmark; Linux test-only unused import was corrected by aa21 and independently reviewed (`40971aa9...`). Preserved exact failed log and previous terminal JSON under sc-23137-single-pass-gate-v1. Conditional selected16 packet budget is now complete as a prospective policy: 1,140,624,680,187-byte initial minimum, independently reviewed `24a9a3a0...`; actual user selection/admission still pending. Three-catalog native fixture package df9833e3... is prepared but not executed. No actual packet phase or source-file writes.

## Historical checkpoints

Corrected FULL inspection is terminal and independently audited PASS. FULL29 (`17a9c510-8893-4988-a22f-4ca025dbab95`, session28679) exited0/reaped at command23671, no command failures/cleanup. Result SHA `a022b93193438067b3e48692750faf69caf3f69dd53d36ce948c924cf839bb9f`; `sc-22844-raid-full-terminal-review-v1/terminal-review.json` SHA `c2977071b1b4bd9e7c79cbd1ff2082d9cb89c683b066743b26796ebfcf1dc679` passed (31,652,232B metadata). FULL output SHA `7367db0a8cabaab85f1aeff9874357e93cc18e36ddec1c89b119b3251037d3ac` contains48 outcomes/47 requested full captures,16 families,19,872,102 retained source rows, no inspection errors. Do not continue FULL. The subsequent metadata-only paths phase is now running; no packet phase, family choice or migration has executed. Both old failed intervals and clean cadence transition remain preserved. This is terminal FULL provenance proof, not complete S9 acceptance.

Actual first-path sizing completed under the existing supervisor: `sc-22844-path-sizing-result-44ed1d98-f565-4ce5-add2-2dd93a4ae379/measurement.json`, SHA `a21718b9258f99e68119a67996f038b16a3d0ff4c456926646fd96e6c9392065`, MEASURED_SIZING_ONLY.47 revisions/1,006,834 pending paths,428,026,080 stored descriptor bytes,418,695,916 native pending-admission bytes (max741).6.267s,229,562,000 VM-step floor,63,356,928B process HWM and sampled group peak; owner/session29407 exited0/reaped, no errors. Plan93,308,780,544B,4096-byte pages, one freelist page, journal_mode delete, no WAL/SHM/rollback journal, before/after identities unchanged. Admission/request/owner/results/actual tool responses are under `sc-22844-path-sizing-admission-44ed1d98-f565-4ce5-add2-2dd93a4ae379`; request SHA `b2a0e7c55e6d01814d0a5ba5c785793fad57428d59a7dd68784f17dcabe6d28b`, owner-recipe SHA `173e38876317bbebe2078b4941b7a0505319af35cd05e40b8e8e83e937bb3fa8`, owner-result SHA `093187442763e846ee6bc88c6f3e05dd1b4fe8718e4998400cd8c97d7b51fee3`. Actual sizing independently passed: `sc-22844-path-sizing-actual-independent-bs5fwhg1/review.json`, SHA `c527810074c1b5e5c5812a30030af9aaad8806a836d1f7b074eb009ff8636292`. First paths funding/procedure is prepared in `sc-22844-first-paths-funding-9vjvdcnw`: funding SHA `d8240a8615a8bfaf9888f4a49a5cbf74a74c32528a8efa3a53f62759adbc4255`, prospective initial free minimum320,784,132,099B with unchanged headroom/reserve. Independent funding review passed: `sc-22844-first-paths-funding-independent-ey9zggtk/review.json`, SHA `3003c8f2a87c027b031887b7889d9fc140c97b271b544ca8505a8016db8115c5`; corrected PROCEDURE-v2 distinguishes requested WAL/FULL/fullfsync from the quiescent DELETE observation and states the actual locking/free-space monitor boundaries. Exact unsigned phase-boundary recipe is under `sc-22844-first-paths-admission-f56d09e7-e3bb-4d19-b43f-ac2252cd0ef1`, SHA `47901e6f1082c8edf7c2f19634df971d125ce80074da18e811cf5090f01e8aab`; both locks acquired, FULL29 current/journal23671 and measured plan identity unchanged, no pause, exact command000023671 absent. RAID3.725TB/local302GB available at preparation. First paths phase is now running: attempt `f56d09e7-e3bb-4d19-b43f-ac2252cd0ef1`, session16784, recipe SHA `5aeca619495e20c47cdfffb3407740146ad5358ae9d60125e3448db2d343eccc`, root admission `abe0638a32a628234b14804b2fb3308be43f2d5d57478f74aea0cd7ef9feaefe`. Actual launch response is saved unchanged in the admission directory. First revision checked12,316 paths in12.178s across13 nonempty native commands; this is a prefix observation, not phase completion or whole-library throughput. Frozen source is unchanged. The old FULL terminal auditor is FULL-only; a bounded phase-specific terminal review is being prepared, anchored to completed FULL provenance.

Bugs sc-23102 and sc-23122 are verified Done: PR #12 merged as `9dbcc76d4e9dfa3dbd626007b4e73698d2995a9e`, exact tree match to reviewed `af8c6a4c743a8849287e2f35ce5c1b9b51e5c532`. PR CI34638787643 and merged-main CI34639830635 both passed all4 jobs; live main and tracker read-back verified. Delivery receipt `sc-23122-orphan-handshake-gate-v1/delivery.json` SHA `702ca2f4808f68301bb3a40d186124b79bd48f74b78a54e97c413e632ca22824`. S9 integrated main at local `2dc7b105c6fbefd0d82a48e2e3d40bee452fd9f5`; no frozen runtime changed. Public PR11 head `80d56a0eec2beb1ac02a3bb60f09889854ce5e5e` passed all4 jobs in CI34641343286; receipt `sc-22844-delivery-80d56a0/ci-terminal.json` SHA `702ff9e17a0425af985535a58c99b993b37298d6df8b086458a011ced005ba99`. It remains draft/unmerged and S9 is In Progress.

Historical split preparation: Export-lock bug sc-23102 is now isolated unchanged on live-main6ff4487 in PR #12, head `ae59afcf25d7a8f8849ac7242351887b1a34a4da`, worktree `sc-23102-export-owner-release`. Exact transplant review `d9a2124269e0cd7e1d9ee054ca253064c0fb744d3ac08faf7f43853eba756e26` passed. Fresh CI34637538748 exposed an unchanged timing-dependent supervisor test: job103388995719 expected known orphan but received unproven ancestry. Exact failed log retained under `sc-23102-export-lock-gate-v1/ci-ae59afc-benchmark-failed.log`. New Bug sc-23122 is In Progress, blocks23102, and is being repaired in the same PR test file with explicit child-observation synchronization; no production runtime change or local native build is admitted. The original PR11 remains draft and the unchanged358ac3d CI success is historical evidence, not standalone delivery acceptance.

Historical public snapshot: head `358ac3d3b1aa2ae2865fbbced13601f6daa27bf4` passed all four CI `34625720876` jobs. Bug sc-23102 is In Review with deterministic before/after reproduction, three lifecycle tests, five export integrations, strict Clippy/formatting and independent review `7c2e2296…`. PR #11 remains draft/unmerged and S9 remains In Progress. Repository visibility was reverified PUBLIC. Source is stationary; this handoff update is local pending the next material delivery batch.

Cadence transition is reviewed and admitted: 3600s cooperative soft duration, unchanged4800s emergency/RSS/disk/source/user-pause guards, early pause at12MiB completed new-result metadata or18000 new commands, unchanged16MiB/20000 terminal caps. Package `sc-22844-cadence-source-v1-EvaR9L` has56 author tests plus3 independent boundary checks. Source review `d8b6eb9a024adcb2e55da28179b831439ba13cd91541adf12e7294cae9b49008`; actual clean transition `sc-22844-cadence-transition-independent-oekvvrrt/transition-pass.json`, SHA `d3ff4e36a91041b170461631065e7b49c0d0ec0906f2888d603285022396c921`. Root admission `sc-22844-canonical-continuations/81bb3b08-58ed-4b34-a623-ad5d7fa33c0c/root-admission.json`, SHA `16ead5ea88b441c422e631c22b3522be88098637c712ca21e99f6c4dba46631a`. Both old failed intervals remain failed; new pause/terminal auditors are `9b7689e89e461e8c232d7dc8b401b42842f59b58d0caef244ca2c720d1952993` / `da77b8f8604d37c76ed97b21875500a0c010343922f0b288298056f51881c6d6`. Actual quiescent accounting:92737536B local control allocation, RAID3.771TB/local316GB free independently checked. The96020296B category is an estimate, not quota; no funding number or resource limit increased.

Historical sizing source preparation: script v3 was ready as PREPARATION_ONLY under `sc-22844-paths-sizing-script-v3-i9xvYY`: source `7c62af68f0bae9197631b4f4c03a1d3c4835119ecb294f6c86079bbd4dd11674`, independent review `sc-22844-paths-sizing-independent-cj0csq7s/review-v3.json` SHA256 `0917e7216eac679186e5d8ec900b8ac210bda9838f1a5659d79ebee573702852`, 25 synthetic tests passing. V1 metadata-growth/gate-status defects were corrected; V2’s 100M VM proposal was insufficient for even the 503921 pending paths already observed in 15 reports, so V3 uses prospective 1B VM steps with the same 120s/256MiB/other bounds. No actual-fit guarantee or real plan read. Terminal FULL/review, exact 47-revision inputs and separate root admission are required. Use its README and existing frozen owner; never execute directly outside supervision. The sizing gate used `/Users/michael/PhotoCatalog-private-results/sc-22843-private-python-v1/env/bin/python` (verified psutil7.2.2, SQLite3.53.3); use this existing private environment for the owner/measurement, not the bare FULL-controller Python which lacks psutil. Source-only cadence compatibility review `sc-22844-paths-sizing-cadence-interface-gkrnjxk5/review.json` SHA `b0ad3a87d4f429ebb61a15fd44623795bb8e122da2988af3973c0c344dfceef7` passed4 tests: no package change required; old auditor pin provides extracted metadata/lock utilities, while actual new result/review/profile refs must be bound in the real request.

Final family-choice renderer is source-ready under `sc-22844-family-renderer-v1`: `render.py` SHA `4f2e4a4d2d2bcbd780ccbb8ea1803d5ed1d4231d6bae075fdbfdba0d0929b382`, five synthetic tests PASS, independent review `sc-22844-family-renderer-independent-7idhapzk/review.json` SHA `c6e77d3a13d3b911809e2bff362555ef46be515dc0bcba9f1e3ca719812e550e`. It renders exact final FULL and packets reports into private REVIEW.md plus lossless mapping.json, covering all16 families/48 candidates, separate image/master/virtual/file counts, UTC filesystem dates and explicitly unknown internal timestamp units. No actual reports rendered or choices made. Use its README after final packet qualification.

A bounded local observation after FULL21 recorded 16 successful native adds, 15 reports and 9739671 retained source rows, not photo counts or terminal acceptance. Receipt `sc-22844-native-progress-after21-zn4ro2z7/observation.json`, SHA256 `306757f8c5be7e5a654f28af87f83a023b7946a291c29ecea119b5a1058af2a2`. Final family choices must bind the latest packet-phase evidence digest; pre-selection zero conflicts do not establish absence of overlap, and internal change timestamps have unverified units.

Historical attempt 18 (`2bacecf2-06d6-4341-bb56-4f6a83c42a91`) exceeded the 512 MiB sampled Python limit at 539,197,440 bytes during saved-row replay. Session 91297 was reaped, with only discovery 10238 added. Result `2854b115c33d7765c0460ed89c33c0e8aae8e436368f8270eba8b1aa25afdddd` remains failed. The earlier diagnostic substituted file-sized reads for production cap-sized buffered reads, so its memory result did not establish production allocation behavior. The fixed-chunk reader passed 129 Python tests and independent review. Corrected diagnostic v3 replayed 10,221 records without native dispatch in 197.2 seconds at 253,100,032 bytes peak RSS; session 4846 and child 33755 were reaped. Independent recovery review `a11f1ab8…` preserved both failed intervals before attempt 19 admission. Older snapshots below are historical.

Historical S9 stop after seventeenth attempt
`e1ddf030-f29f-440c-84d1-dd0421b079a3` exceeded the 512 MiB Python sampled RSS
limit during saved-page JSON replay (537,378,816 bytes observed). Actual session
`88833` exited and was reaped; cleanup recorded no remaining observed processes.
Its result remains `failed_or_unknown`, SHA256
`2c71d45e486bfbd3472897565d93e9c9b87a4b702bc606cb4275b2697e2990d9`.
Journal next command is 10238; this attempt completed only fresh discovery 10237.
Do not resume from an older clean checkpoint or relabel this failure. A reader
correction now releases encoded bytes before constructing JSON objects, with
encoding/error behavior preserved. Read-only diagnostic session 9436 reached the
first unrecorded command after 10,221 saved records in 203 seconds, with
343,457,792 bytes peak RSS under the unchanged 512 MiB limit, no native commands,
unchanged failed evidence and reaped owner/child. Receipts are under
`sc-22844-readonly-replay-diagnostic-v2/execution-20260911-root-01`.
Independent actual recovery review and separate admission remain pending;
diagnostic qualification alone does not establish production or S9 acceptance. The clean
checkpoint and earlier active snapshots below are historical; check current control.

The public reader/protocol 2 correction passed all 126 Lightroom Python tests
and independent review (`723051297dcde7e0ba64d25046f9b46560e2ae9c167814770d67873b3aba499f`).
Its sampled test-root RSS was 143,966,208 bytes; the test process was reaped.
Public support binds the expanded helper roster and equivalence evidence, while
ordinary failed-predecessor rejection remains. The incident-specific recovery
controller and history auditors are separate private evidence, not a generic retry
feature. This local gate does not replace CI for the eventual new commit.

sc-22836–sc-22843 are verified Done. S8 PR #10 merged as `6ff4487`, with identical reviewed/merged trees and all four PR/main CI jobs passing; Shortcut closeout was read back. S9 legacy MAIN is complete: 48 outcomes, 19,872,102 retained source rows and 5,740 source-table descriptors, not distinct photos. Its schema-2 numeric relationship defect prevents treating derived links as S9 acceptance. Corrected schema-3 FULL is rebuilding all 48 members; all 47 requested full captures are independently verified complete. Historical draft PR #11 head `70f5284`: CI `34604737271` has terminal SUCCESS in all four jobs, including Windows. Earlier CI failures remain historical evidence. Actual FULL completion, paths, packets, family decisions and merged-state acceptance remain outstanding. Live Shortcut remains authoritative.

| Story | State | Work surface / artifact | Evidence | Next action |
| --- | --- | --- | --- | --- |
| sc-22836 | Done | [PR #1](https://github.com/michaeltrefry/PhotoCatalog/pull/1), merged 56f0b37 | Independent review PASS; 18 local tests; real CR2/JPEG source invariance; merged-main three-platform CI 34227667817 SUCCESS; Shortcut read-back | Complete |
| sc-22837 | Done | [PR #3](https://github.com/michaeltrefry/PhotoCatalog/pull/3), merged f353f3d; [decision](BACKEND_DECISION.md) | Reviewed head 7bb3930 and merge have identical trees; PR CI 34362927300 and main CI 34366395501 all four jobs SUCCESS; Shortcut Done read-back, comment 22896 | Complete |
| sc-22838 | Done | [PR #2](https://github.com/michaeltrefry/PhotoCatalog/pull/2) and corrective [PR #5](https://github.com/michaeltrefry/PhotoCatalog/pull/5), merged bef8b6c | Final renderer review; 115 tests; private camera/render comparisons; PR CI 34386168135 and main CI 34390636314 all four jobs SUCCESS; Shortcut read-back | Complete |
| sc-22839 | Done | [PR #4](https://github.com/michaeltrefry/PhotoCatalog/pull/4), merged be85c16 | SDK normalization repaired; independent RDF/opaque XMP preservation checks; PR CI 34380874912 and main CI 34382746592 all four jobs SUCCESS; Shortcut Done read-back | Complete |
| sc-22840 | Done | [PR #6](https://github.com/michaeltrefry/PhotoCatalog/pull/6), merged 60fc33c | Controlled APFS detach/remount/reorganization/replacement/undo; Linux bind mount and Windows volume GUID/junction proof; PR CI 34394610454 and main CI 34395883109 all four jobs SUCCESS; Shortcut Done read-back | Complete |
| sc-22841 | Done | [PR #8](https://github.com/michaeltrefry/PhotoCatalog/pull/8), merged789a39d | Reviewed preview defaults and full30-file memory/quality, layout, navigation and integrated10M evidence; PR CI34430709720 and main CI34432335579 SUCCESS; Shortcut Done read-back | Complete |
| sc-22842 | Done | [PR #7](https://github.com/michaeltrefry/PhotoCatalog/pull/7), mergede68d375 | [Mac qualification report](ORGANIZATION_PERFORMANCE_RESULTS.md); reviewed scale/actual-overlap evidence; main CI34418548263 SUCCESS; Shortcut Done read-back | Complete |
| sc-22843 | Done | [PR #9](https://github.com/michaeltrefry/PhotoCatalog/pull/9), merged f6d19dc; corrective [PR #10](https://github.com/michaeltrefry/PhotoCatalog/pull/10), merged 6ff4487; [qualification report](EDIT_QUALIFICATION_RESULTS.md) | Reviewed 533 cases/249 numerical configurations PASS; PR CI34538492535 and main CI34550127979 all four jobs SUCCESS; exact tree and closeout evidence verified; Shortcut Done read-back | Complete |
| sc-22844 | In Progress | [Draft PR #11](https://github.com/michaeltrefry/PhotoCatalog/pull/11); corrected schema-3 FULL continuing | 47 captures verified. FULL26 clean at 13956; failures17/18 retained. All four CI jobs pass at `358ac3d` in `34625720876` | Terminal corrected FULL, separately sized paths/packets, family decisions, merge and merged-state acceptance |
| sc-23102 | In Review | Export-service lock lifetime correction in PR #11 | Deterministic before/after, 3 lifecycle +5 integration tests, Clippy/fmt, independent review and all four exact-head CI jobs pass | Merge with S9 delivery and verify merged-state closeout |
| sc-22845 | To Do | Lightroom migration | Prerequisites sc-22840/sc-22842/sc-22843/sc-22844 | Dependency-bound |
| sc-22846 | To Do | Backup and restore | Prerequisites sc-22843/sc-22845 | Dependency-bound |
| sc-22847 | To Do | Desktop UI | Prerequisites sc-22840/sc-22841/sc-22842/sc-22843/sc-22845/sc-22846 | Dependency-bound |
| sc-22848 | To Do | Integrated readiness | Prerequisite sc-22847 | Terminal proof only after integration |

## Bounded JSON read correction

Production requested the configured JSON cap even for small files. The earlier diagnostic substituted file-sized reads and therefore did not reproduce that allocation behavior. The corrected reader uses at most 64 KiB per read for admitted caps, joins fixed chunks, and releases the input buffers before decoding objects. It preserves byte encoding, JSON errors and cap overflow behavior. Public source and tests passed independent review `d1e2f62b62ea57ff0ed31c3ffa180f94b2d2b194e34c0e8f12ef6fd9dfdc6a95`; all 129 Lightroom Python tests passed. The initial synthetic standalone-helper fixture error and rejected bytearray prototype remain retained evidence.

Private helper `209b8a54…` and profile `5ccb6aea…` bind equivalence review `9f1134f6…`. The corrected diagnostic retains production buffered read sizes, the real JSON implementation, and frozen hashing/page processing. Its receipts are under `sc-22844-readonly-replay-diagnostic-v3/execution-root-01`. Synthetic small-file temporary allocation improved substantially; near-page chunk joining used more memory than the original reader, so the result is not a blanket memory or throughput claim. Controller `0ac64b39…` and auditors `0f8e97c6…` / `4a62e753…` passed independent source review and preserve both failed intervals. No failed attempt is relabeled as a clean checkpoint. Production attempt 19 crossed replay and reached a reviewed clean checkpoint; terminal FULL and S9 acceptance remain pending.

## Export-service ownership correction (sc-23102)

Linux CI at `58e7a1a` exposed an export owner-drop/reopen failure. A deterministic local test reproduced it by retaining a duplicate of the actual service lock descriptor after dropping the service. `ExportService` now creates an acquired-only explicit-unlock guard immediately after successful acquisition. Its release follows existing worker reaping and preview-pause cleanup; failed acquisition cannot unlock another owner. Constructor errors and unwind also release ownership. Three lock lifecycle regressions and all five existing export-service integration tests passed locally, and independent source review found no blocker. Exact-head three-platform CI remains required. The change does not alter render pixels, codecs or hashing; prior S8 numerical measurements retain their original source identity.

## Resource and authorization ledger

- User authorized epic delivery and ordinary PR/CI/merge. The user changed michaeltrefry/PhotoCatalog to public because private-repository Actions consumed the monthly allowance; preserve public visibility and batch validated changes before CI. The user released the reference Mac on 2026-09-09.
- Originals and Lightroom sources on the RAID remain read-only. Private fixtures and evidence remain outside Git. The selected TEST migration writes only its authorized scratch destination; no canonical migration or original mutation has occurred.
- User authorized a dedicated RAID scratch folder for S9: `/Volumes/MichaelJon/PhotoCatalog-Scratch/sc-22844-schema3-20260911`. Inspection output is under `inspection/schema3-run`; SQLite temporary files use `sqlite-temp`. Local control/evidence remains under `/Users/michael/PhotoCatalog-private-results/sc-22844-raid-full-control-v1`. Scratch authorization does not permit writes to source catalogs or photos.
- All S2 measurement sessions are complete. Local builds/renders were paused during timing. The owned passive observer PID 30547/session 93348 was stopped afterward and exited successfully; its final receipt at 2026-09-09T14:00:27Z records SIGTERM and 6,472 samples. No outstanding S2 process needs resuming.
- S4 and S6 use isolated worktrees. Parent owns catalog schema/model integration; packet extraction and explicit export own separate modules. S6 owns preview adapters/store/scheduler; its rebuildable manifest has separate schema ownership. Heavy Cargo and timed measurements are serialized.
- Bound Cargo builds to four jobs and serialize heavy measurement/render lanes. Native Windows/Linux behavior requires hosted CI; local Mac tests alone do not establish it.
- Preserve user ZIPs and unrelated root-worktree files. No CodeGraph tools/index or root CODEGRAPH.md was available.

## S2 evidence and closeout

The frozen original harness SHA-256 is `167b13d524c0f21a18426bb8b21acc6792f275de7884edca1cf995c6ff874c89`; supplemental driver is `1c509cf8a06ce2dba734ef71ca4aecd6b4a50d1b9eac513ab9cc9ee2a8e6f5eb`. Both files remain unchanged. The frozen original native binary remains separately retained at `sc-22837-frozen-v2-native/catalog_probe`, SHA-256 `cfbe4dcce0f3f176af369e1c685f40bce45d5ae77c259603ab6d78948b6084a6`.

All private paths below are under `/Users/michael/PhotoCatalog-private-results/`:

| Evidence | Status / review |
| --- | --- |
| `sc-22837-final-v2` | Complete original campaign; 1,026 distributions, all load proofs, paired hashes, 48 plans and recovery independently reconciled. Default 256 MiB failures retained. |
| `sc-22837-production-profiles` | Complete predeclared profiles; 2,475 distributions reviewed. DuckDB 1024 MiB numerical PASS; no original SQLite profile qualifies. |
| `sc-22837-query-work-v3` | Independent full-record/counter/provenance review PASS. SQLite correction constant 2,410/2,413 VM steps; DuckDB scans grow. Failed diagnostics retained. |
| `sc-22837-query-work-pristine-v3/derivation.json` | Six verified APFS standalone clones; main-file hashes and original main/companion state retained unchanged. No WAL/SHM removal. |
| `sc-22837-sqlite-page-correction-v1` | Complete once, all scales PASS. Review reconciles 432 children and 495 distributions. 10M worst warm/fresh/write p95 0.695/1.587/20.813 ms; warm RSS 325 MiB. |
| `sc-22837-native-query-work-v1` | Review PASS: bundled SQLite 3.51.1, 36 queries/7,200 full records; same constant candidate VM work, hashes and source preservation. |
| `sc-22837-native-runtime-v1` | Complete once, all scales PASS; reviewed 6 children, 15 distributions/2,400 samples. 10M browse/rating/edit p95 0.129/86.948/87.238 ms. Two edit samples exceed 100 ms; p99 105.234/max 116.937 ms retained. |
| `sc-22837-measurement-20260909-host.jsonl` | Complete ordinary desktop CPU/RAM/GPU/I/O observation stream, with explicit final shutdown. |

Native runtime source is 1ac4737 (code introduced at ef53d25), binary SHA-256 `ab76f6d00358f4ad45e4f833bfd2e124b73cd74b9733d6fece6d02d101f3c8f8`. Its build reference binds Rust 1.98.0, lockfile, source and binary. Later documentation/comment changes do not change the measured execution path. Five native query-work tests plus new configuration/preservation/browse-only tests are included in the 40-test Rust suite. The 39 Python tests include real candidate child/recovery dispatch and full DuckDB diagnostic setup.

Full original/profile/corrected tables are in [baseline results](BASELINE_BENCHMARK_RESULTS.md) and [profile results](PRODUCTION_PROFILE_RESULTS.md). The [decision](BACKEND_DECISION.md) records the selected configuration, original adverse evidence, native contention differences, cold-cache limitations, and integration scope. No S2 result is evidence for RAW/preview/UI performance. Do not repeat a completed campaign to improve a result.

## S3 retained evidence

PR #2 reviewed head 2d30647 and merged a0bf374 have identical trees. Private corpus `sc-22838-e6a4545-corpus/receipt.json` and `verification.json` retain 22 fixtures/44 deterministic renders, numeric and visual color/HDR checks, and camera/format provenance. Two DNG fixtures derive from JPEG/TIFF and are explicitly not native-camera evidence. AVIF orientation, PSD transparency/missing-composite, and DNG crop/profile/spatial calibration repairs passed review and the corpus. This backend integration does not change the renderer; do not repeat the private corpus without a relevant change or unresolved concern.

## Foundation risk register

Incomplete previews after interruption; duplicate asset creation on retry; source changes during extraction; generated fixtures falsely standing in for real CR2 compatibility; private images or metadata being committed; unbounded directory/file reads; platform differences in path and file publication semantics. Validate these within sc-22836's scope before closeout.

## S7 qualification checkpoint — 2026-09-09

The [organization report](ORGANIZATION_PERFORMANCE_RESULTS.md) records the final
v4 query and actual-overlap transition PASS at runtime source `0cfc1fd`. The v1/v2
failures and rejected v3 mixed summary remain retained. Worst warm/fresh page p95
was 60.295/61.871 ms; browse RSS peaked at 480.781 MiB. All 200 saves overlapped
background activity at every scale, with p95 12.788/12.161/11.543 ms. Final CI and
PR/merged-head verification remain required; this checkpoint is not a Done claim.

## S8 implementation checkpoint — 2026-09-10

The Rust core implements versioned complete basic recipes, independent variants,
persistent undo/redo, bounded copy-adjustment jobs, and variant-aware retained and
interactive previews. Full-original JPEG8, PNG8/16 and TIFF8/16/float32 export uses
immutable plans, selected metadata, explicit overwrite approval, a process lease,
read-back seals and guarded durable publication. Actual process tests demonstrate
cancel/reap, preview preemption, undo ABA rejection and owner restart. An orphan
seal is never published just because it exists: a resumed worker rerenders the
original and requires full byte equality before reuse.

The focused gate at `7a97b8b` passed 120 library tests and 14 integration tests
(organization3, actual export service5, publication2, edited previews4).
Strict all-target Clippy passed at `8f8bc58`. Preserved logs include the earlier
error-context assertion, private fixture-helper compile error and lint failures.
These results establish focused correctness, not S8 performance or platform
qualification.

Publication now performs full verification outside the catalog writer, retains
held-file identities/change stamps for short guarded namespace steps, and commits
intent before capture/link. Tests cover interrupted and repeated restoration,
actual crash after publication, cancellation and later offline originals. Warm
preview records identify the actual checksum-validated prepared input consumed;
corruption is an explicit cache miss. The retained identity/change-stamp guard now spans native export decode; its
regression rewrites the same inode before decode and restores bytes/mtime afterward.
The inconsistent decode is rejected while the stable positive control passes.
The prospective S8 targets remain unchanged. The serial qualification harness
must freeze exact source/cohort/oracles/settings/resources before execution.

## S9 current FULL checkpoint — 2026-09-11

Delivery head `70f5284` passed all four jobs in CI `34604737271`; the saved
terminal receipt is `sc-22844-canonical-runtime-profile-v1/ci-70f5284-terminal.json`.
PR #11 remains draft and unmerged.

Legacy MAIN output is `sc-22844-current-families-v6/reports/main-review.json`
(SHA256 `5942e4dfea9ab05c1677bb7d1844ef68431e9ef080eb795878f781fd97026b44`).
The source remains preserved; corrected FULL creates a fresh schema-3 plan rather
than rewriting the old relationship evidence. No family is automatically chosen.

Execution binds native source `ccae8ad` (binary SHA256
`eb541139f7f7888ee5eec58975ad9f36008c65500ee553fa4030d4507c47602f`),
and base driver SHA256 `af2c5e1dcb939713f8296998cc908e87b8e6658a4208b6e6cb12fe3b60132288`,
using new controller SHA256
`28c84319a05f10bfc4d7eb82efbf3d5ab147ea6f9a452fb482ed160b6cd21590`
and canonical hash profile `sc-22844-canonical-runtime-profile-v1/profile.json`,
SHA256 `243ccfff25670add20fa9f4c1b5511e426da637f99637dc17456b312eeff0d2b`.
The profile installs a reviewed byte-equivalent hashing helper: effective Python
execution changed, while native and base driver files/bindings remain preserved.
Prior slices used controller `969eeaad…`; their artifacts are not relabelled.

The independently reviewed first slice completed all 47 requested captures.
Second attempt `c6bf42d7-cea0-4ed1-baf5-415cfe0e7c12` is cleanly paused/reaped,
with 599 successful new commands and next command 655. Its local result SHA256 is
`df98edc1966c18aab0c071ead6e30ac7dbf0208fd8d7c41108d3823dbab98529`;
independent review `sc-22844-raid-full-second-slice-review-v1/paused-review.json`
has SHA256 `d268a04b3567081bb0c0a8f4835f94c779e9a3e94bc0e8e3d467e12d855d2fb0`.
A bounded observation at next command 8857 recorded 12 successful native adds,
11 native reports and 7,297,390 retained rows, with no observed native failures
or stage warnings. These are native progress facts, not completed Python
inspections: the driver can catch inspection errors before returning final FULL.
Observation `sc-22844-native-progress-observation-2VBJ8X3X/observation.json` has
SHA256 `6524fd768e3c420269aa4b6ee88e1bde40f626353dfd5edd45f9a34f93387683`.
All-48 FULL outcome qualification remains pending.

Fifteenth FULL attempt `bd23129e-1876-442c-b4a8-a0b6576b935e`, session `91609`,
paused cleanly and was reaped at next command 9528 after 711 successful new
commands from 8817 and no failures. Result SHA256
`26d7c7313f0abe76653ba85a765bbdf0771ed973d075263f68906df9a74e607f`;
PASS review `sc-22844-raid-full-fifteenth-slice-review-v1/paused-review.json`, SHA256
`4784b3b3e11f78fb5ee39d762e1686f0b66aa905116f8f92ed96aaba0a810b85`.
This is a clean checkpoint, not FULL completion.

Sixteenth slice `d74d191c-661d-481b-9f35-11a2897df6fa`, session `51299`,
is active from command 9528 with the same reviewed canonical hash profile.
Its recipe SHA256 is
`e32d27c2fe995490044795e23ad235256ffbe3e44424f7855d79af30deb65844`.
This snapshot can become stale while execution continues. Before resuming or
starting competing native work, read the actual `current.json` in
`sc-22844-raid-full-control-v1` and that attempt's result and terminal tool receipt;
do not treat this document as current process ownership proof.

Funding and limits are unchanged: zero additional capture copies backed by all-47
completion, full remaining plan/page/temp/metadata allowances without partial
credit, separate headroom and 32 GiB operating reserve; 600-second cooperative
slices, 4,800-second sampled emergency stop, 512 MiB Python / 1 GiB native /
1.5 GiB combined sampled RSS limits. Exact scratch environment/identity checks
and fresh inventory/free-space admission remain required. Recent recorded free
space was 3,812,635,889,664 bytes against the unchanged 474,549,846,911-byte
minimum; this observation does not replace fresh admission.

The exact old-to-profile transition PASS is
`sc-22844-canonical-transition-review-gtGRGbi4/transition-pass.json`, SHA256
`92443376288e43ffc5e5367adb50e957a016fa183fd4c366d62fcbb824558c25`.
The first canonical-profile attempt (thirteenth slice) recorded authorization before dispatch (`execution-profile.json`,
SHA256 `fe890c4aef5cd2bb03b5e92490b2b074786b7dd971f8e1f3eae0bd69b4810ce1`)
and actual child installation (`execution-profile-consumed.json`, SHA256
`2490f16a3daf7766f0b2f6ca426265ccefb7d10ee8b2a09841bcf565fa771240`).
These receipts augment the base source identity; they do not assert a runtime gain.

The actual local native/controller transition gate passed in 12 seconds: 52 commands,
one total capture, eight hash comparisons, five negative cases and all 11 CLI
children reaped. Receipt `sc-22844-canonical-transition-smoke-execution-v2/smoke-result.json`
has SHA256 `2c97975bfaadcf77d3390509391fbd479bfc0cb6215088878e4cfe086dabcce8`.
All 116 Lightroom Python tests passed in 5.18 seconds; root tool receipts are under
`sc-22844-canonical-runtime-profile-v1`. These gates establish transition correctness,
not real-corpus speed or all-48 FULL completion.

The profile-aware metadata auditor `sc-22844-checkpoint-audit-profile-source-v1/audit_pause.py`
has SHA256 `92cc53471f3457be1448977f456328f786db6c99026b6c5773e5f1a8739bba7d`.
Its independent review and 20 synthetic checks passed; review SHA256
`d2d6f223465434598876f737ec6114cbdf7d138d5031556edd8658e95482b36a`.
The previous e7-only auditor remains preserved for historical checkpoints.
The canonical preparer source `bd91429a4cfe4e14c0aa26ed6100a7708b86c11cf6a4e6034e08a039c763c8fb`
passed independent review `2e38b2d6994f6c4a9ff383b7145923d3b182017f35046596c913ba427a1fe27d`;
actual read-only draft and externally granted final preparation both passed.

The terminal FULL auditor `sc-22844-terminal-audit-source-OKkmuQol/audit_terminal.py`
(SHA256 `1ef8cb5d860489973828acaa4181e80ee7ed7f5be4d20132854a6474fe56ebe5`)
is source-reviewed, with 13 author and eight independent synthetic checks passing.
Independent review `sc-22844-terminal-audit-independent-aohcguni/review.json` has
SHA256 `7028802a2ac35b3f9531bc518bf8954df0dcaff9e6399d32e75149da6293159a`.
This is readiness to audit an actual terminal result, not a FULL completion claim.

For subsequent clean pauses, preserve the actual result/review, current control,
journal, exact owned pause and source/funding bindings. The private
`sc-22844-canonical-continuation-preparer-source-v2` prepares packages only,
requiring an independent review for the initial profile transition and a root grant
for each launch; root reviews and launches.
Later continuations must retain the same profile and use its profile-aware audit.
No re-adoption, automatic retry, family choice or migration is authorized by a prepared recipe. Paths/packets require successful FULL output
and separately reviewed funding through the existing phase controller; no new
controller code is required. See `LIGHTROOM_PHASE_CONTROL.md`.

## Historical S9 inspection resumption checkpoint — 2026-09-10

The following records the earlier failure/adoption state, superseded by the current
MAIN/FULL checkpoint above; its failed and successful artifacts remain preserved.

The seventh v5 inspection attempt stopped after macOS reused the PID of a
completed inspector for a later command. All 198 observed PIDs are absent and
the wrapper was reaped. The failed result remains preserved; user Lightroom
catalogs are unchanged. Commands 5020–5848 succeeded, and read-only row command
5849 was interrupted. Fourteen completed members retain 9,256,288 rows; the
fifteenth retains 483,383 rows with 24 successful readback pages. Its failed
25th page does not establish progress.

The supervisor now identifies owned processes using native birth seconds and
microseconds. Independent source reviews passed this correction and protocol3
checkpoint `cda4a65`, which admits the exact failed attempt into a separate v6
inspection namespace while retaining v5/v4/v2 provenance. Eight supervisor
fixtures and 60 Lightroom contracts passed.
Receipt: `sc-22844-pid-recovery-gate-cda4a65-v1/result.json` under private results.
The actual v6 init/adopt phases subsequently exited successfully. Independent
review reconciled 13 typed tables, 1,774 descriptors, 9,739,671 retained rows,
15 captures and 14 completed member outcomes. The active member retains 24
successful pages through cursor 9,280,288; failed command 5849 adds no progress.
Known processes are absent and v6 main has not started. Evidence is
`sc-22844-generation-request-v6-cda4a65/independent-adoption-review.json`
under private results. Adoption finished before the tiny editing smoke began.
The next main request is held: its existing growth allowance plus protected S8
qualification funding exceeds currently available space. Continuous protection
of that funding is required before further inspection growth. Auxiliary,
referenced-path, packet and current family selection work remains outstanding.

## S8 qualification preparation checkpoint

At `45df945`, the full locked all-target Rust gate passed 370 tests across 33
suites, with three intentional ignored fixtures. Strict Clippy and package
formatting passed. The initial broad gate exposed 16 dirty-binding trigger
failures; conditional inserts fixed repeated UPSERT/REPLACE conflict behavior.
The failures remain retained. The independent Adobe DNG sensor-neutral test
passed; it does not establish real-camera Adobe appearance parity.

The probe compiled at `a5ac9d6`. At `6bdb95f`, all 42 Python checker contracts
passed, including actual subprocess cleanup, evidence failures and the fixed
per-configuration statistics. Independent statistics review passed. Receipts:
`sc-22843-core-and-probe-gate-v1/receipt.json` and
`sc-22843-supervisor-statistics-v1/receipt.json` under private results.
The expanded checker gate at `d38a526` passed 80 tests. Nine actual preview/export
process tests and strict all-target Clippy passed, including worker high-water
receipts. Fifteen memory/cleanup checker tests passed at `3539509`. These gates
exercise checker contracts; they are not actual corpus or timing qualification. No large editing campaign has started. Final aggregate, durable
metadata/service and 100MP independent checks, binding, campaign, review and
three-platform delivery remain required.

The first tiny synthetic end-to-end smoke failed because image 0.25.9 silently
missed the recognized TIFF ICC tag. Commit `9483835` reads that tag through the
existing TIFF dependency and rejects malformed profiles. The native regression
reproduced the wrong gamma before the fix; the media suite then passed 19 tests
with one intentional external-fixture ignore. Independent source review passed.
Tiny smoke v2 passed all four analytic fixtures with 14 recipes each using the
unchanged independent oracle and tolerances. Both smoke attempts are retained
under `sc-22843-tiny-smoke-v1` / `sc-22843-tiny-smoke-v2` in private results.
This is correctness evidence only; performance and large-image qualification
remain pending.

Tiny production-path smoke v3 passed two references, warm previews (2+100) and
first-original lifecycle on TIFF (2+20), and retained nine failures. Six metadata
cases exposed invalid RDF in the controlled fixture; `3473220` preserves its
intended nested qualifier in valid explicit RDF syntax. Twenty-two JPEG exports
completed, but the checker incorrectly expected no XMP despite the service's
regenerated technical metadata. `26d38cc` corrects that expectation while keeping
direct-codec omission checks strict. All 98 Python contracts and the focused
native RDF regression passed; independent source review passed. Two tiny overlap
cases did not retain a live worker long enough. No overlap or performance result
is awarded, and a fresh corrective smoke remains required. Failed v3 artifacts
are preserved under `sc-22843-service-smoke-v3` in private results.

Corrected read-only verification of v3's 22 retained JPEG outputs passed, with
the original inputs and failed receipts unchanged. Fresh smoke v4 then passed
seven cases: the two references, warm preview, first-original delivery, the
22-output export including cleanup, and PNG8/16 selected-metadata preservation.
Six failures remain retained. `48a2d17` fixes TIFF readback through the held file
descriptor by providing the Python decoder a display name; the regression failed
at all three precisions before the fix, then all 18 readback contracts passed.
Independent source review passed. These results do not yet qualify actual TIFF
derivative preservation.

JPEG with a large selected XMP packet exposed a product preservation defect:
the pinned SDK's extended packet loses the named RDF subject. The strict checker
and importer reject the resulting subject mismatch. Encoder transport correction
`afaa4ca` preserves the named subject and recomputes the extension digest;
seven native export tests pass, including full export/reimport preservation and
rejection of a changed extension subject. Stale-renderer publication guards
`1ded7b6` pass 17 native tests while retaining finalization of already-installed
outputs. Both changes passed independent source review. Two tiny overlap cases
still lack a sufficiently long-lived natural worker. All v4 known processes are
absent; no performance award follows from this correctness smoke. Full campaign
preparation still awaits final source binding and an explicit phase admission.

Bounded outer supervision and host-log funding are implemented in `ac373d1` and
`a649807`, with a separate preparation limit. Final review found late zero-exit
acceptance, repeated zombie accounting, and unresolved ancestry reporting;
`809e434` fixes all three. The integrated Python gate passes 122 tests with no
skips, and independent exact-source review passes. The full native gate passes
377 tests with three intentional skips; strict Clippy and both debug and release
builds pass. These results qualify the source checks, not the unstarted full
campaign. The next service smoke must use the corrected native binaries and
final supervisor helpers; v5 remains an unexecuted source proposal.

S9 supplementary funding control `90a0f1a` passed 12 synthetic tests and parent
independent source review. It preserves the frozen inspector and adopted evidence
while checking free space before new commands and at outer observation points.
It is not a filesystem quota. Its prepared main request remains held and must
bind the final S8 funding amount before any execution; no further large plan copy
is needed solely for this control change.

PR13 final aa21c245 passed all four CI34653155165 jobs and merged as b411ab5b; reviewed merge tree04d6706e verified. Merged-main CI34653984699 running. Selected runner152f899 passed150 Python tests; independent review identified a terminal selected-path identity reconciliation gap, now being corrected. Integrated source9aa5d0a builds a separate release native under sc-22844-selected-native-build-oil8ij91 (session69457). Tiny native proof launcher is in preparation; no actual fixture native or real packet command has launched. User selection remains pending.

Selected source correction dccada8 independently reviewed PASS b89f06ab; integrated c1cebd54 passes153 tests with pinned Python3.14 (default-runtime failed attempt retained), gate409f3cc3. Native9aa release build completed/reaped0 (67.17s), build46516238/nativee85cd0bf. Local tiny prior native proof launched by Hubble under admission243d4341, stops before selected dispatch. Renderer selected extension9 tests/final reviewd6261039 passed; all48 candidates remain visible with packet scope separate from native choice. Real selection and packet admission still pending.

Merged-main CI34653984699 now all4 SUCCESS at b411ab5b. Tiny proof session1880 reaped1 after successful MAIN/FULL3/PATHS3 due solely to launcher expected `available` versus actual `available_packets_uninspected`; failed result retained. Direct native comparison continuation authorized on quiescent tiny-plan copies, without recapture or retrying failed owners.

sc-23137 | Done (read back; comment23145) | PR13 merged b411ab5b, PR/main all4 CIgreen; qualified9aa/e85 S9 native | direct108-command comparison and actual33-command selected pause/resume/replay PASS, independent aedce5f7; all sessions reaped | no remaining bug work.
sc-22844 | In Progress | draftPR11 published003770e, all4 CI34654732967 SUCCESS; local closeout docs ahead | selected2/excluded1 fixture final8ac44056, sources/prerequisites/replay preserved; native-generated renderer proof47348bfe | pending actual user catalog selection, then production-specific profile/funding/admission, selected XMP, final dry run and delivery. Real FULL/PATHS/sidecar scans remain completed; no real packet phase or migration started.

User clarification: proposed16 source catalogs are approved for TEST/dry-run use; intended destination is ONE consolidated PhotoCatalog library with nested folders such as year/month/date. Epic E7/E10 and S10 updated/read back, S9 comment23147. Current Rust physical-folder hierarchy and recursive queries verified directly. Approval does not authorize final migration, backup ingestion or filesystem reorganization. Source message bca24810 retained privately; next test manifest/profile must bind this limited scope.

User follow-up confirms the folder display should mirror the filesystem, with every year in the same catalog. E7 and S10 now state that directly.

sc-22844 | In Progress | selected16 actual XMP test launched session77218, attempt6ae59d32-9b77-4c9a-afe3-e2a30bd1fe91 | recipe3cfa42ac, independent packagee22173d7/nativeaa208bb8, root admission1899f3cf, fresh RAID3.724TB >1.141TB prospective minimum | await actual pause/terminal and independent audit; originals read-only, one future consolidated destination; no migration. First root preflight used prior-result instead of prior-recipe descriptor and failed before writes/launch, preserved then corrected. Post-packets finalization procedure is sc-22844-post-packets-finalization-ljhtrxp7/PROCEDURE.md; no finalization launched.

sc-22844 | In Progress | first selected slice clean pause result e418cc8d,467 successes/125795refs, sampledcombined414466048B, session77218reaped0 | audit50439189 + independentc6dc1306; sourcefinalizerv2 3e9c1730 reviewedf25fe739/11tests | exact continuationaaef3b7c session19583 recipe917ea78f launched afterroot40aa71aa andreview8b8f7f00; currentrunactive, no migration.

sc-22844 | In Progress | second selected slice clean pause a13dae5c:378 successes, cumulative191569/209091refs,6 finished catalog checks, sampledcombined416137216B; session19583reaped0 | auditc00058ab + independent62067c5f + continuationa9121b01 | third attemptb030a554/session25675 launched recipe98109cf5 after fresh locked admission; same scope, no migration.

sc-22844 | In Progress | selected PACKETS complete:209091refs/16checks,1044successfulcommands/3slices, next28196; session25675terminal0/reaped | resultevidence8a4ecb9c/outputd849361a/auditeb2acd60; independent actual review pending | final TEST choices/report not yet run;48657missing +5467availablepacketgaps require explicit finalreport; no migration.

sc-22844 | In Progress | selected terminal independent2981479b PASS;6112 appendixrecords capacityprepared | finalizerv2 active session96719 requestda3dec86/outerb29cafd3/admissionb2588c1b | TEST derived-plan choices only, no migration; wait actualterminal and independentfinalreportreview.

sc-22844 | In Progress | TESTfinalizerterminal0/reaped session96719,157 successfulcalls,16choices,68pathpairs,6112indexedpages | FINALIZED_TEST_WITH_NAMED_GAPS:8271globalIDpairs/sample1000 | paginatedendpointfix underway on isolatedbranch; independentfinalizerreview pending; exact5467gapclassification pinned a12d97e5; no migration.
