# Architecture

## Workspace

- `crates/app`: CLI, first-run/explicit setup, per-instance client construction.
- `crates/tui`: dashboard, onboarding, picker, input/state/rendering, async orchestration.
- `crates/gitlab`: REST client, DTOs, pagination, reads and confirmed write actions.
- `crates/config`: validation, legacy/current TOML parsing, atomic bootstrap and updates.
- `crates/storage`: per-URL OS credentials, environment override, atomic generic JSON storage.

## Identity

The product, executable and Cargo packages are Invocra / `invocra`.
The config/cache directory is `~/.config/invocra`, the OS keyring service is
`invocra`, and the token environment variable is `INVOCRA_GITLAB_TOKEN`.
The 85×13 text composition (ASCII scene and Terminal Gothic glyphs encoded as Unicode Braille)
is embedded in the TUI crate and shared by the setup header and empty log pane. It is centered
without wrapping using the terminal's default foreground and background colors. Smaller areas
show the 28×4 Gothic wordmark alone, then a plain title if even the wordmark cannot fit.
The public source repository is https://github.com/dsszs3333/invocra;
all workspace packages inherit that URL in Cargo metadata.

## Startup and persistence

A missing configuration creates a bundled sample and opens onboarding. Existing configurations
never restart onboarding just because a token is missing: the CLI prints a re-login hint.
`setup` explicitly opens the wizard and merges the chosen instances; `a` does so from the dashboard.
Groups resolve on refresh; explicitly configured projects win duplicate IDs.

Only the active instance has a live `AppState`. Switching tabs persists its snapshots and ref
pins, then loads the target cache. Cache filenames use SHA-256 of the trimmed URL without trailing
slashes; the URL inside the file must match before restoring data. Unbound legacy caches are
left untouched and require manual recovery. Variables, logs, MR lists and transient UI state
are not persisted. A cache is a last-known snapshot, not a full offline GitLab client.

Tokens are keyed per URL; a legacy global credential is never adopted for an arbitrary server.
`INVOCRA_GITLAB_TOKEN` overrides keyring lookup for every configured instance. macOS uses Keychain,
Linux uses a kernel keyring (not persistent across reboot), Windows uses Credential Manager.

## Event loop and requests

Dashboard and onboarding each own a terminal session and one blocking input reader. Both close
the input queue, signal stop and join that reader on normal/error exits before another session starts.
Async network tasks send events to the current loop. An instance-session generation rejects results
from a previous visit to a tab. MR list/head results also carry the panel generation and list scope.

Ordinary background snapshots and viewed pipelines share a semaphore of eight project fetches.
Interactive reads and the open pipeline bypass this semaphore so queued background projects
cannot block them; all requests still share the HTTP budget. Auto-refresh
runs every three seconds for active pipelines and skips in-flight requests. Per-project generations
reject stale ref results; replacement reads are cancelled, and a slower full refresh cannot revert
a newer project request. UI clocks redraw every second; log follow polls every 3–15 seconds,
backing off when the trace is unchanged. Closed/replaced log requests are cancelled, including
requests waiting for budget. Background snapshots reuse project metadata until an explicit refresh.
Latest-pipeline snapshots carry a runtime-only detail timestamp (not restored from the disk cache).
Hidden projects poll pipeline status at the normal interval and reuse jobs/bridges for up to 30s
only when pipeline ID and active status are unchanged and the prior snapshot has no error.
New IDs, status transitions, terminal results and explicit refreshes fetch full details. Hidden
inspected views refresh as a whole every 30s. Opening a different project/view refreshes stale
details after 300 ms of stable selection, using open-pipeline priority and reusing project metadata.
Details younger than three seconds are reused. Navigation cancels its previous fetch and debounce;
pending manual requests are preserved. Full refreshes stream per-project snapshots, rejecting older
results and allowing already-loaded active projects to poll before the final report reconciles
membership and errors. A full refresh additionally limits concurrency to two projects when a local
request budget is set, or eight without one. The `g` scan still requests a full refresh.
Manual mode disables periodic status/log polling and selection-triggered detail refreshes;
initial loads and explicit actions still run.
Cache writes are debounced to the UI tick and flushed on exit.

State holds the latest project snapshot separately from a viewed history/downstream/MR pipeline.
Actions and logs use that viewed pipeline's actual project ID. Mutation confirmations freeze their
target; merging an MR includes its reviewed source SHA so a later push causes GitLab to reject it.
Write requests are not automatically retried or cancelled when switching tabs.

## HTTP and limits

API requests have a 20-second timeout, do not follow redirects and retry a GET once on HTTP 429,
honouring `Retry-After` (seconds or HTTP date), capped at 24 hours. Writes have no automatic retry.
All same-instance HTTP requests, including pagination and artifact requests before cross-origin
redirects, share an in-memory limiter across client clones. It spaces requests evenly and gives
queued interactive requests priority over automatic polling. Automatic polling has two classes:
open pipeline and ordinary background. When both wait, at most two open-pipeline requests run
before a background request. FIFO within each class lets all background projects make progress.
Navigation cancels/replaces automatic requests whose class changed, leaving manual requests alone.
Cancellation releases a queue slot.
A 429 pauses new requests for the whole instance; missing/invalid Retry-After uses exponential
backoff from 1 to 60 seconds. Successful responses after the cooldown clear the 429 hint.
Server `Date` headers anchor absolute reset/retry dates to avoid local clock skew; reset waits
longer than 24 hours are ignored as invalid. Without a valid Date, the local clock is used.
Valid RateLimit-Remaining and Unix RateLimit-Reset headers pace
both automatic classes over the remaining window, reserving 10% of the first observed remainder
(at least one request) for interactive traffic. Local spacing and 429 pauses remain hard limits,
including for interactive requests; an unlimited local budget still respects server quota.
Admission deducts quota immediately. A request guard tracks in-flight sends through response,
transport failure and cancellation; response counters conservatively subtract other in-flight
sends and can only lower the estimate within a window. Newer reset timestamps replace the window;
older responses cannot restore quota, and an expired monotonic deadline cannot be revived by a
late response. Missing/invalid headers preserve the estimate until expiry. GitLab-origin artifact
requests participate; external CDN redirects do not report into the limiter.
The request timeout starts at send time, not while waiting for a slot.

`requests_per_minute` (positive integer or `"unlimited"`) and `auto_refresh` inherit from `[settings]`
and can be independently overridden in `[[instance]]`; built-in defaults are 80/minute and true.
Settings apply on startup. Local limits are per process; server user/IP quotas can include other
clients. The TUI shows effective settings, source, selected-project update age and pause status;
a temporary 429 hint points to the config file without interrupting input and adds a footer row
only while visible. Cooldowns are in memory and reset on process restart.
Artifact downloads stream to a temporary file and atomically replace the destination on success;
they have a 20-second connect/read timeout without a total download timeout. Up to five redirects
are allowed; another origin must use HTTPS and never receives the GitLab token.

Paginated lists (including jobs and bridges) require advancing `x-next-page` and fail after 1000
pages instead of returning an incomplete success. MR lists intentionally show the latest 50 open
items; pipeline history shows 20. Logs keep at most 2 MiB. API permissions still control available
actions: read-only tokens cannot mutate, and CI/CD variables require Maintainer access.

The dashboard has five panels: projects, branches, pipeline/jobs, log, and merge requests.
Overlays provide variables, pipeline history, environments, tags, actions and confirmations.
Real GitLab/CDN integration, OS credential persistence and release publishing are separate
validation steps; unit tests use synthetic credentials and local HTTP mocks.
