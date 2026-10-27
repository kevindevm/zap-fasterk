---
title: Starred and pinned messages
permalink: /starred-pinned-messages/
description: Save starred messages, navigate pins, and understand synchronization limits.
---

# Starred and pinned messages

This integration adapts [LisandroNahuelH's starred-message PR #236](https://github.com/crmne/zapfast/pull/236)
and [pinned-message PR #234](https://github.com/crmne/zapfast/pull/234), with
fork-specific account isolation, durable ordering, privacy and recovery checks.

Choose **Star** in a message's menu to save it in your account's starred list.
The star button above the chat list opens that list. **Load more** retrieves the
next page, so the list is not restricted to its first 200 messages. Click a row
to open its chat at that message. Choose **Unstar** to remove it from the list. Stars made before your copy of
Zap Faster followed them are read from the phone once, the next time it
connects.

Star changes are saved locally before synchronization and survive restarts and
connection failures. A pending change appears locally while Zap Faster waits to
sync it. Starred messages belong to the linked account that made the change.
Locked chats are excluded from the cross-chat list. Deleting or revoking a message,
or clearing its chat, removes its saved mark and preview. Screen privacy also
applies to this list and the pinned-message banner.

Choose **Pin message** to pin a message for **24 hours**, **7 days**, or **30 days**.
These durations come from whatsapp-rust's `PinDuration` API. The banner under the
chat header opens a pinned message; clicking its pin icon opens the next pin.
**Unpin message** removes its pin. A notice in the conversation records when
someone pinned the message. Notices remain after unpinning or expiry until the
referenced message is deleted.

Zap Faster permits up to three active message pins and does not replace a fourth
silently. WhatsApp decides whether the account may pin in a particular group and
can refuse the request. The client only adds a pin after a confirmed send or a
remote pin update. A timed-out request may have reached WhatsApp: check the phone
before retrying. Pin requests are not blindly resent after uncertain transport
failures. Read-only channels do not offer pin actions.

Expiry removes the banner and the bubble's pin mark while the chat is open,
without waiting for another message. Expiry is local display maintenance; it
does not send an additional unpin request to WhatsApp.

## Synchronization limits

Stars share the `regular_low` app-state write scheduler with private read and
archive state. The archive keeps pending intent separately from confirmed state,
so an older acknowledgement cannot discard a newer click. Remote star mutations
use their original action timestamp. History's star flags have no mutation time
and only seed unknown records; replay cannot undo newer confirmed changes.

Pins use their original sender timestamps with millisecond precision. Older
history cannot undo a newer unpin, including events in the same second. Remote
updates and history recovery do not produce success toasts.

The pinned whatsapp-rust revision replays records during app-state snapshot
recovery, but does not reconcile records absent from a snapshot. Zap Faster cannot
prove that a previously stored star is still present when only an absent snapshot
record supplies the evidence. Complete recovery of that case requires a protocol
library change. This client does not advertise complete mobile synchronization
parity or verified live group pin permissions from compilation alone.

## Attended acceptance

Use synthetic conversations and a test account on Windows, Linux, and macOS:

1. Star a message here and verify the phone's star. Unstar on the phone, reconnect,
   and verify the list and bubble mark here. Repeat rapid star/unstar clicks,
   disconnect before acknowledgement, and restart with a pending change.
2. Put over 200 synthetic messages across several chats in the starred list.
   Load every page and open a message outside the initially loaded chat page.
3. Switch accounts while a change is pending. Its result must affect only its
   account, without a toast, composer change, read receipt, or dialog on the
   account on screen. Lock the app and a chat, then verify previews disappear.
4. Pin and unpin with each duration from both devices. Check groups where only
   admins may change the pin, and confirm a refused pin does not appear locally.
   Reach the three-pin cap and verify an additional pin is refused here.
5. Leave a short-duration synthetic pin fixture open across its expiry. The banner
   and bubble mark disappear while the timeline notice remains. Delete, revoke,
   and clear messages during outstanding requests and verify no late callback
   restores a preview or mark.
6. Replay older history after a newer unstar or unpin, including synthetic
   same-second pin/unpin events. Confirm the newer state remains in place.

Offline demos `starred`, `star-menu`, and `pinned` exercise the list, its menu,
banner navigation, marks, and timeline notices without opening personal chats.
