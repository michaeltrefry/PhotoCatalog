# Native storage CI fixtures (S5)

These are bounded correctness checks, not storage performance measurements or
full removable-volume lifecycle evidence. They modify only freshly created
synthetic temporary files. Source-ready implementation requires native hosted CI
execution before any platform PASS claim.

## Linux file bind mount

Run `python scripts/validate_linux_bind_mount.py` on the configured Linux build
host. The existing Ubuntu job invokes it after ordinary library tests. Cargo
selects the actual library test executable as the ordinary runner user. The
launcher verifies that the exact test exists, then uses noninteractive sudo and
`unshare --mount --propagation private` to run only that ignored test with a
60-second timeout. Missing privilege, unavailable statx mount IDs, failed mount,
missing test, or assertion failure is a failure, never a successful skip.

The test primes the real `ImportVolumes` parent-directory cache and mounts one
owned file onto another in the same temporary directory. It proves equal device
numbers, an unchanged parent object key, and distinct kernel mount IDs, checking
our statx result against the separate `/proc/self/fdinfo` descriptor interface.
The expected mounted path comes from the mount action itself. The observation
must report that exact file mount, its source-relative location, and invalidate
the parent cache. A neighboring file must retain its own mapping. Source bytes
remain intact; exact unmount reveals the original target bytes and restores its
parent mapping. A guard handles assertion/error cleanup, while private namespace
exit is a second isolation boundary. No existing mount is detached or rebound.

Removing the Linux mount-boundary conditional would incorrectly synthesize the
parent mount even though device numbers match; the explicit file-mount-path
assertion catches that regression. This tests the actual import cache component,
not image decoding or a complete CLI import, and does not prove UUID remount
persistence or arbitrary stacked/concurrently changing mount behavior.

Primary references: Linux [bind-mount semantics](https://man7.org/linux/man-pages/man2/mount.2.html),
[private mount namespaces](https://man7.org/linux/man-pages/man1/unshare.1.html),
and [descriptor mount IDs](https://man7.org/linux/man-pages/man5/proc_pid_fdinfo.5.html).

## Windows local GUID and junction

Run `./scripts/validate_windows_volume.ps1` with PowerShell 7 on the configured
Windows build host. The Windows job invokes the existing ignored
`inspect_explicit_volume_fixture` hook twice: on a newly created Unicode-named
file and on that same file through a newly created directory junction.
`Win32_Volume` through CIM supplies the expected local volume GUID independently
of the Rust adapter. `.NET Path.GetRelativePath` supplies the expected suffix
from the independently known drive root and source path. The hook requires both
values, a Windows GUID identity scheme, the expected canonical source, and a
reconstructed candidate path with the same native object identity. Thus missing
GUIDs, absent relative mappings, or a junction-relative suffix cannot pass.

The script checks exactly one executed test per invocation, unchanged fixture
SHA-256 and modification time, and no added fixture directory entries. It deletes
only its junction link before removing its owned temporary tree and restores the
prior test environment variables. All setup, expectation, and test failures
remain failures; no successful fallback or skip is allowed. The hook still
accepts coordinator-supplied optional expectations for other platforms, but this
Windows script always supplies every independent expectation.

This is native local-volume and same-volume junction mapping evidence after CI
passes. It does not claim VHD detach/remount, mounted-folder cross-volume mapping,
replacement-disk lifecycle, SMB/UNC availability, or network identity coverage.
The ordinary missing-path and pure ambiguity tests remain separate evidence.

Primary references: Microsoft's [Win32_Volume provider](https://learn.microsoft.com/en-us/previous-versions/windows/desktop/vdswmi/win32-volume),
[relative-path calculation](https://learn.microsoft.com/en-us/dotnet/api/system.io.path.getrelativepath?view=net-9.0),
and [PowerShell junction creation](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.management/new-item?view=powershell-7.5).

## File-mount regression discovered in CI

The first native Linux run (34390759693, head `9b593a8`) passed the ordinary suite but failed the bind-mount assertion: joining an empty relative suffix appended a separator to a regular-file locator. Both candidate reconstruction and filesystem-relative mapping now preserve the exact root/subpath when the suffix is empty. Portable tests compare `OsStr` bytes and read the resulting regular file, because normalized `Path` equality alone hides this defect. The original failed log is retained privately as `sc-22840-linux-ci-v1.log`. The repaired adapter passed the pinned standalone macOS fixture suite and Clippy; actual Linux execution on the repaired PR head remains required.
