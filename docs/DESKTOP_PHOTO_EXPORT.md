# Desktop photo export

The catalog-owned export controller remains mounted when the dialog is hidden or
catalog indexes are preparing. Opening the dialog does not recover, run, retry,
restore, or resume saved work. Every publication-capable action is explicit.

## Workflow and command coverage

- Host Options supplies plan budgets, execution limits, page bounds and token
  limits. Every worker limit can be overridden explicitly with canonical decimal
  strings and the host's consistency checks; the backend remains authoritative.
- Recover is an acknowledged action before Run and may publish accepted intents.
  Incomplete recovery requires another explicit bounded step. A changed execution
  configuration requires matching recovery again.
- Begin creates a durable building job. Frozen targets capture the selected
  logical variant's edit revision and either omit metadata or capture resolved
  metadata revision with an optional explicit retained base-model ID.
- Native destination selection and Destinations produce bounded named pages.
  The review retains the exact output settings, budgets and target identities.
  Each Append uses the reviewed native destination, explicit overwrite choice,
  frozen metadata and current expected job total. Targets can span library pages.
  Errors/collisions never trigger silent renaming, skipping or overwrite.
- Seal checks the exact saved total. Run uses explicit attempt/time budgets;
  finishing an operation does not mean every output succeeded. Job and Item pages
  show durable outcomes, including failures. Continue is another explicit Run.
- Status polling is independent of action replies and dialog visibility. Active
  Cancel and Yield use exact operation/job identities. Inactive job Cancel uses
  the cached cancel operation for admission and completion even though its RPC
  returns a final Job. Draining remains nonterminal until the backend reaps work.
- Jobs, Items, DestinationRows and PlanChunk page independently with bounded
  continuation, including empty pages with a next cursor. Plan inspection exposes
  exact stored text without parsing/re-emitting it, plus bounded typed source,
  metadata, destination and receipt summaries. RetrySeal and Restore require an
  explicit acknowledgement of the inspected exact authority.
- Profile validates a native ICC selection and returns an immutable session
  token. ProfileRelease and ResultRelease are explicit. Saved plans own their
  bytes after append and do not depend on a session token. Paths exposes one
  bounded preparation step and reports remaining/unbound paths.

## Admission and uncertain replies

App flushes pending edits before admission. Its action gate ends once the
independent controller observes admission; completion is awaited outside the
gate. Status/cancel/yield and Close remain available while work runs. Backend
write holds, unknown controller status and pending admission block new edits.
Terminal refresh preserves the current EditQueue and pending draft, and only
refreshes the matching selected row in the same catalog.

Begin and Seal are direct queued writes. Stop waiting releases local waiting and
the App gate, but leaves a separate catalog-owned write hold. Job/Jobs reads have
higher actor priority and **cannot establish settlement**. Only the original
successful acknowledgement or closing/reopening the catalog releases that hold.
The direct request's response observer does not use the local abort signal, so a
late acknowledgement survives the bridge's post-invoke abort check. Abandoned
responses never select a job or continue writes in a changed dialog/catalog.
Transport failures are conservative: inspect saved work and close/reopen if the
acknowledgement never arrives. Requests are never replayed automatically.

Target preparation has cancellable local waiting as well as native request
cancellation. Cancel/hide releases its gate even if a reply remains held. Late
reads cannot install a frozen review. Native pickers and all action continuations
are guarded by catalog lifetime and dialog/attempt generation.

## Validation boundary

Frontend tests cover decimal bounds, the full execution-limit mapping and local
wait/late-response behavior. A private actual-App fixture exercises synthetic
backend replies, including held/lost acknowledgements, multi-page append, mixed
outcomes, authority/receipt inspection and catalog replacement. It does not
qualify native rendering, real filesystem destinations, installed dialogs,
memory at scale, or three-platform packaging. Those integrated checks remain
separate acceptance gates.
