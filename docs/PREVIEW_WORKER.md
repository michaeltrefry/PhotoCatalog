# Preview worker process boundary

The application supplies an absolute executable path to `WorkerProcess`. The CLI's internal `--preview-worker` entry runs the same compiled full-image renderer and selected production preview codecs. The application must admit a worker through the scheduler before launching it and retain that reservation until the child has exited and the owner has consumed its validated output. This low-level boundary does not itself select process concurrency or establish an RSS budget.

A bounded JSON request carries a lossless native source path, immutable preview keys and an encoded output allowance. One request renders at most two distinct tiers from one full-resolution source. The child checks the actual compiled renderer identity and original fingerprint before rendering and checks the original again before writing its completion receipt. Unimplemented nonzero pixel recipe revisions are rejected; general metadata revisions are not pixel recipe revisions.

The parent keeps the child's stdin pipe open. A start token admits full-image decoding. After decoding, the worker atomically publishes a small checkpoint marker and holds the decoded pixels until the owner's next poll sends one encode-admission token. EOF, errors, repeated tokens or unexpected bytes revoke the lease and terminate the entire process, including native codec threads. This also handles the kernel closing the pipe after an owner crash. Explicit cancellation closes the lease, kills the child and waits before releasing its process resources. A completed child is consumed only once.

Encoder scratch and the temporary encoded Vec belong to the worker working-memory allowance, separately from encoded staging; the output allowance is checked after encoding and before writing. The child writes exclusively created, flushed files inside its private staging directory, with a cumulative encoded allowance checked before each file write and a separate 64 KiB receipt limit. The owner rejects oversized files before allocating buffers, validates exact requested keys and hashes, parses dimensions before pixel allocation and fully decodes every output using the production cache decoder. JPEG completion requires its final end marker. A valid checksum alone cannot admit a truncated image. Catalog/manifest attachment occurs later in the service under catalog generation authority; the child never opens those databases.

A child holds an OS file lock while accessing staging. Startup recovery examines at most 128 directory entries per call and skips live child locks. After acquiring a lock it claims the directory with an atomic rename, keeps lock ownership through known-file cleanup and can resume an interrupted claim. The child revalidates its original absolute working-directory identity after acquiring its lock, preventing delayed startup into a recovered directory. Windows rename denial for an open working directory leaves the original staging untouched for a later recovery tick. Unknown files or links require inspection rather than a recursive cleanup. An interrupted child cannot leave a completed result that bypasses image validation. Recovery and output-size checks do not sandbox an arbitrary replacement executable or impose a filesystem quota on foreign code.

Local focused evidence uses a 17×11 PNG through the actual CLI child, plus a generated 36×16 linear DNG through the native SDK before cancellation and owner EOF at the decoded holding checkpoint. The checkpoint cases exercise the armed watchdog and actual admitted native/render work with decoded pixels retained; they do not claim a deterministic kill inside an uninterruptible codec call. Other cases cover EOF before admission, an actually killed child leaving a partial file, active-lock recovery, delayed startup after lock-file open, interrupted claimed-directory cleanup, an oversized sparse output and a correctly checksummed truncated JPEG. These are correctness tests, not throughput or memory measurements. The ordinary import command is still being connected to the service; these worker checks do not complete S6 or establish its browsing budgets.

Per-request decode admission uses `DecodeLimits`: encoded source bytes are
length-checked before allocation; raster dimensions are checked before full pixel
decode; PSD plane/float sizes, AVIF dimensions before `NextImage`, LibRaw's
uncropped sensor and processing sizes before `unpack`, and DNG main/stage/mask
surfaces are checked before pixel allocation. SDK scratch blocks and LibRaw's
allocation ceiling also receive the configured per-allocation limit. The float
conversion and orientation surfaces use the same dimensions and allocation
ceiling. These checks do not constitute an aggregate native-memory allocator or
an OS RSS limit. Peak memory plus margin remains required before the service's
worker reservation is frozen.

`decode_full` retains its existing default 100 MP output support and allocation
limits. `decode_full_limited` allows a caller to reduce admission limits, including
native intermediate dimensions; the intermediate ceiling can exceed 100 MP to
accommodate uncropped sensor margins. A refusal remains `ResourceLimit`, carried
through the actual child error receipt as `WorkerFailure.decode_status`, so the
service can retain a durable retryable job and the prior thumbnail. Increasing
admission permits retry of the same source identity. This change alone does not
complete that service persistence or establish a production memory default.
