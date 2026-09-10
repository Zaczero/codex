# User verification cancellation (experimental)

Local UI clients can cancel a native user-verification RPC by sending
`userVerification/cancel` with `{requestId}` and the `experimentalApi` opt-in.
The result is an empty acknowledgment (`{}`). This API does not enable desktop
verification capability advertisement.

`requestId` is the original status, enroll, delete, or verify RPC's string or
integer ID on the same connection, not the server elicitation ID. Use fresh IDs
for each operation and a distinct ID for the cancel RPC. Unknown, finished,
unrelated, and other-connection requests are no-ops.

The acknowledgment confirms the cancellation signal without waiting for the OS
prompt to close. The original RPC completes independently, with
`cancelled/interrupted` when cancellation prevents completion. Cancellation
cannot roll back completed effects. It remains effective while a proof waits for
outbound queue capacity, but cannot retract a response already enqueued.

Canceling or resolving an elicitation does not itself stop a separate
`userVerification/verify` RPC. Clients must cancel that RPC separately and discard
late proofs after the approval is canceled or resolved. Only one native worker
runs per app-server; if an OS call remains active after cancellation or timeout,
subsequent local operations return `failed/providerError` until that worker exits.

# Thread removal

`thread/archive` and `thread/delete` reject attempts to remove a live internal
worker with JSON-RPC error `-32600`. The worker's owner controls its shutdown.
For example, a Guardian reviewer remains available to its parent conversation
after a client tries to archive or delete it.

After the owner releases the worker, its saved conversation can be archived or
deleted normally. Ordinary client-controlled threads keep their existing behavior.

# Working-directory changes

`thread/cwd/set` accepts `{threadId, cwd}` and reloads an idle, persistent local
root thread in an absolute destination directory. It returns a `ThreadResumeResponse`
with an empty `thread.turns` array and emits `thread/settings/updated` to subscribed
clients. The thread identity,
conversation history, name, ledger and session settings are retained. The destination's
project configuration, instructions and runtime services are loaded without starting
a model turn. The directory change is persisted before the response.

Clients may supply `developerInstructions` for the destination, as with thread
startup. Omission uses the reloaded project's instructions. The TUI supplies its
terminal-specific instructions when that feature is enabled.

The thread and its agents must be idle with no background terminals, and the
destination must have an explicit trust decision. Untrusted destinations keep
project-local configuration and hooks disabled. Invalid destination configuration leaves the source
runtime intact. If replacement startup fails, the server attempts to restore the
source runtime and reports the error. Existing subscriptions survive the reload.

The TUI uses this operation for `/cd`. `thread/settings/update` remains the partial
settings-update API; it does not reload all project configuration. Forking or creating
a managed worktree still creates a separate thread.

# Amazon Bedrock authentication

If `model_providers.amazon-bedrock.aws.credential_export` is configured, Bedrock setup and
Bedrock login return an error without changing configuration or saved credentials. Remove the
exporter configuration before selecting another credential source. `aws.credential_export` and
`aws.profile` cannot be configured together.
