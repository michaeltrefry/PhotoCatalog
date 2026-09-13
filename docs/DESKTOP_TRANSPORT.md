# Additive desktop transport

`application::desktop::DesktopBridge` is an explicitly constructed façade. Production CLI/Tauri startup and `commands::State` still select `application::Bridge`. This component is the transport prerequisite in S12 comment 23326; it does not establish managed SQLite admission, filesystem isolation, Source-lock custody, or completion of the metadata workflow.

The façade keeps the existing synchronous `submit`, `preview_bytes`, `shutdown` and pending/cancellation types. `try_shutdown` reports a failed drain and `status` reports transport readiness, pending count, unknown outcomes, child PID and drain error. Every outer Request variant has an exhaustive route: Lightroom stays in the independent local Bridge; catalog requests and global BackupInspect/Restore/Status/Cancel go to the child, including when no catalog is open. Closing a catalog does not close Lightroom. Shutting down the façade signals both owners before joining.

## Protocol and limits

The configured executable has a hidden `--catalog-desktop-worker` mode before normal application initialization. It runs the existing actor. No executable, program arguments or environment comes from an individual command. Startup transfers the full Config through a capped 4 MiB private JSON handshake, preserving NativePath, preview policy, service limits and application limits. Test-only closures are omitted. A version and source-derived protocol fingerprint bind both endpoints; the fingerprint is not a claim that separately packaged executables have identical file hashes. Packaging qualification must retain those hashes separately.

Owned stdin carries input, stderr carries framed completion/control, and stdout carries framed preview bytes. Each 48-byte little-endian header contains magic `PCDT`, version 1, kind, two reserved zero bytes, 16-byte session nonce, u64 request ID, u64 byte offset, u32 total and u32 payload length. Payloads are at most 16 KiB. Header checks precede payload allocation; assembly admits the complete declared size against its kind's cap before allocation. Existing JSON DTOs keep decimal-string and opaque authority semantics. No packet or preview byte data is base64 encoded.

Ordinary admission is capped by configured `Limits.queued`; a separate 16-slot control allowance serves status, cancel, Close and shutdown. The input writer interleaves control at frame boundaries. Child completion collection uses bounded nonblocking queues, so a held binary result or caller that does not receive its reply cannot block control collection. A bounded merged interval ledger rejects duplicate request IDs without retaining an unbounded per-request history. Cancellation before admission is remembered within the same bounded allowance.

Success replies keep `Limits.reply_bytes`. Existing small error replies have a separate 1024-byte allowance so a success budget of one byte still reports ResourceLimit without turning it into an unknown transport failure. Drain errors and the handshake have a 16 KiB control cap.

Preview bytes reserve the parent's aggregate `Limits.binary_bytes` before allocation. The exact catalog, ticket, session, length, contiguous offsets and BLAKE3 checksum must match. The child retains its original PreviewBytes until the transfer acknowledgement; the parent retains its independent bytes until the renderer drops PreviewBytes. An accepted catalog Close cancels only that catalog's outstanding byte requests atomically with admission. Partial, canceled and malformed transfer ownership is released through Drop; acknowledging a fully delivered transfer is distinct from releasing renderer memory.

## Failure and shutdown

No transport error replays a request or creates a replacement child. An unacknowledged operation remains unknown until verified process exit; callers explicitly reopen and inspect durable state. Pipe EOF alone is not Closed. A dedicated supervisor waits the exact owned child independently of callers receiving results. Only an affirmative OS wait releases process custody. A wait error preserves the Child and pipe/thread owners for explicit retry.

Shutdown requests carry numbered attempts. A checked engine drain failure returns a matching DrainError while keeping stdin and process ownership alive. An explicit retry addresses the same child. Old failure replies cannot replace a newer attempt. Dropping an already failed owner does not implicitly retry or force-exit it. No kill/force-exit is implemented for a child that could own native descendants. The checked engine shutdown integration is a prerequisite for final qualification; a legacy best-effort shutdown is not evidence of verified drain.

## Qualification boundary

The actual-process fixture is deliberately Status and Close-before-Open only: no catalog, cache, photo, decoder or user application. Fake fixtures separately exercise binary backpressure, partial/canceled/stale transfers, replay and early cancel, out-of-order/lost replies, numbered drain failures, and injected failed wait retaining ownership. These fixture claims do not qualify native descendants, production backend selection, the Windows/macOS/Linux installed package, or Source-lock isolation.
