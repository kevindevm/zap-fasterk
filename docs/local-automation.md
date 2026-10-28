# Local command-line access

Enable Allow local command-line access in Settings, Privacy for each account
that should accept automation. The default is off. The app must already be
running. Account ids are the numeric account-folder ids, and chat ids must name
an existing conversation in that account. Restoring a backup disables this
computer-specific authority until explicitly enabled again.

The CLI uses a separate private fastframe-instance slot named
zap-fasterk-automation-v1 under runtime/automation-v1. Unix filesystem permissions
and Windows's random token authenticate the local user. The compatibility
legacy port never accepts these operations. This grants local programs running
as that user account access to the selected account; it is not remote access.
No network service, account system or telemetry is introduced.

Examples below use synthetic ids; substitute your chosen account/conversation.

```powershell
zap-fasterk chats --account 2
zap-fasterk contacts --account 2 --all
zap-fasterk messages fixture@s.whatsapp.net --account 2 --limit 10 --before 1700000000
zap-fasterk search "synthetic phrase" --account 2 --chat fixture@s.whatsapp.net
zap-fasterk watch --account 2 --since 1700000000 --interval 2
zap-fasterk send fixture@s.whatsapp.net "Synthetic text" --account 2 --request-id 0123456789abcdef0123456789abcdef
"Synthetic stdin text" | zap-fasterk send fixture@s.whatsapp.net - --account 2 --request-id 1123456789abcdef0123456789abcdef
zap-fasterk send fixture@s.whatsapp.net "Synthetic caption" --file ./synthetic.pdf --file ./synthetic.png --account 2 --request-id 2123456789abcdef0123456789abcdef
zap-fasterk automation read --account 2 --chat fixture@s.whatsapp.net --limit 10
zap-fasterk automation send --account 2 --chat fixture@s.whatsapp.net --request-id 3123456789abcdef0123456789abcdef --text-file ./synthetic.txt
zap-fasterk status --account 2 --chat fixture@s.whatsapp.net --request-id 2123456789abcdef0123456789abcdef
```

Read returns ascending rows from a bounded recent archive page, plus next_before.
Pass its timestamp and id with --before-time and --before-id to read older rows.
Watch polls archived arrival order once per second for 1 through 3600 seconds,
including late history. Resume with --after-row and --after-id from the returned
cursor. It reports locally archived arrivals, not a server stream, and never
requests older history or marks a conversation read. A deleted cursor refuses
continuation instead of silently skipping data; restart the watch deliberately.
Each archive request allows 1 through 25 rows. Replies above 12 KiB use
bounded 6 KiB byte fragments, keeping every private-channel response below its
16 KiB transport limit. The CLI assembles the complete JSON page before printing
any rows, preserving full text and captions even when one row exceeds the wire
limit. A page may contain up to 16 MiB of JSON. Up to 32 fragment sessions hold
only request identities, digests and authorization, never large reply buffers
or plaintext files. Sessions expire after 60 seconds and clear at shutdown.
Every fragment repeats the original bounded read and verifies its digest.
Changed, deleted, expired or newly private rows refuse the remainder rather
than mixing snapshots; lock, opt-out and privacy revocation remain permanent
for that transfer, including after unlocking. The CLI discards an incomplete
page and reports the refusal on stderr with a nonzero exit code. Attachment files, raw protobufs and encryption
keys are never exported. Phone-only and disappearing messages are withheld.
Locked chats are always refused, even when their folder is open in the app.
App lock refuses requests and scrubs pending replies, including hidden windows.
Chat and contact lists drain bounded pages automatically. Each stdout line is
one chat, contact, message, or send-result JSON object. Errors go to stderr and
return a nonzero exit code. Message rows include chat/id, sender/sender_name,
from_me, Unix timestamp, kind, full text or caption, and delivery status.
`contacts` returns address-book entries; `--all` also returns unsaved contacts.
`search` reads the archive across eligible conversations or the requested chat.
`watch` defaults to incoming messages timestamped from its start and continues
until interrupted. `--since` replays incoming archived messages from that time;
`--seconds` bounds the run. Arrival-order cursors drain each page before waiting,
including pages containing only withheld private messages. `--interval` accepts
1 through 60 seconds. No phone history or receipts are requested. The compatibility
`automation read/watch/send/status` commands remain available.

Each authorized request has a one-use permit. Account opt-out, account removal,
local or remotely received chat lock, and app lock permanently revoke pending
requests and replies. Unlocking or re-enabling access never revives an old request.
Collection requests capture the account conversation-privacy fingerprint. Any
subsequent lock or disappearing-timer change permanently revokes a buffered
collection reply, even if the privacy setting is later restored. Every row is
individually checked for privacy before output.
The transport rechecks authorization before returning a buffered response. Send
acceptance and revocation share a fence: a send accepted before revocation may
continue, while a request revoked before acceptance never enqueues.

Send accepts positional text, UTF-8 stdin with `-`, or `--text-file`, at most
4000 bytes, and a stable 32-digit hexadecimal request id. Keep that id before dispatch. It reserves the id in the
encrypted archive before enqueueing the normal supported WhatsApp send.
Repeated `--file` accepts up to eight readable files of 64 MiB each, sent as
documents. The first attachment carries the optional text as its caption.
Files are hashed before dispatch and checked again for size and content before
upload, so a changed source cannot silently send different content. The whole
batch reserves all child message ids before uploading. Status returns each
child id and delivery state; retrying the same batch never uploads or sends again.
Attachment preparation uses the normal serial, cancellable upload queue and
rechecks its permit before archiving and sending. A revoked attachment leaves
no outgoing row and its newly prepared cache file is removed. A crash or
preparation failure after reservation can leave Unconfirmed; no automated retry
is attempted.
Repeating that same id with the same account, chat and text returns the existing
operation instead of sending again; different content, paths, file hashes,
attachment order, or conversation under that id is refused.
A crash after reservation may leave Unconfirmed without a visible message row.
Status queries preserve this uncertainty. A timeout never means that a send did
not happen. Query the same id, check WhatsApp, and never create a new id blindly.
Normal sends preserve the existing safe pre-transmission rate-limit cooldown;
transport failures and uncertain acknowledgements never automatically retry.
Read/status cannot edit or retry an uncertain send. Offline or read-only
conversations refuse a new send. Sending requires the explicit opt-in, not just
ordinary chat-link handling. The CLI does not send typing or read receipts.

Synthetic fixtures additionally cover collection privacy fingerprint revocation,
private-row filtering with cursor progress, durable batch child identities, and
revocation between upload preparation and outgoing dispatch. They have been
written but still require the combined batch test gates. Existing fixtures cover bounded requests, redacted debug representations,
lock/cancellation response scrubbing, account preference defaults/roundtrip,
request-id reuse and changed-content refusal, archive expiry/chat-lock filtering,
command parsing, per-account dispatch, opt-out/local/remote lock revocation,
and late buffered-reply scrubbing. An offline fixture builds the official SDK
with a synthetic session store and intercepts transport starts through the
existing outgoing test hook: the first request reserves/archives/enqueues one
SDK-generated id, repeats enqueue nothing, and simulated uncertain completion
stays Unconfirmed with no transport retry. This proves local acceptance and
idempotency, not live WhatsApp server acceptance. Real user archive reads and
live sends were not used to
verify implementation. Private transport, attended CLI behavior and full gates
remain verification requirements.
