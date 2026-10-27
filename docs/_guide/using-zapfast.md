---
title: Using Zap Faster
description: Send messages and use attachments, interactive messages, voice messages, and keyboard shortcuts.
redirect_from:
  - /using-fastsapp/
nav_order: 3
---

This guide includes unreleased development features. The public download is
still 0.30.0. The history recovery, account backup, native calling, custom font,
Messages appearance and portable-preference additions below require a development
build; device acceptance is still in progress.

## Writing

Undo and redo in the message field apply to its current chat and account.
Switching chats or accounts, closing a chat, or unlinking clears the editor's
undo history. Saved drafts remain available when you return to their chat.

Enter sends and Shift+Enter adds a line. Turn off **Enter sends** in Settings
to make Enter add a line and send with Ctrl+Enter (Command+Enter on macOS).
`*bold*`, `_italic_`, `~strike~`, and ```` ```monospace ```` ```` format
like WhatsApp, and a message of nothing but emoji shows large.
Mentions in a group are written with `@`; the smiley opens emoji
(searchable), GIFs, and stickers, including the stickers used on the
phone.

Right-click a message to reply, react with any emoji, edit, forward, delete, or
check when it was sent, delivered, and read. The reaction row has a **+** that
opens the full emoji picker. Hover over a reaction to see who added it.
Editing uses the composer. Press Escape to cancel.

The message menu also stars messages for the cross-chat list and pins messages
to the chat header for 24 hours, 7 days, or 30 days. See the
[starred and pinned message guide]({{ site.baseurl }}/starred-pinned-messages/) for sync behavior
and recovery limits.

Forwarding opens a recipient picker. Select up to five writable chats, as on
WhatsApp; a message labelled as forwarded many times goes to one chat at a
time, replacing the previous pick. **Review recipients** lists the selection
for you to confirm, and **Back** returns to the picker without losing it. If a
destination becomes locked or read-only, Zap Faster returns to selection
instead of sending a partial batch. Switching accounts discards the open
forwarding selection. In a large window the dialog takes more of the window, so
more recipients fit at once; it still stays inside a narrow or short one, and
searching or changing the selection does not collapse the list.

Outgoing text can be edited for 15 minutes after its original send time.
Zap Faster validates that limit again before sending and keeps the original
message until WhatsApp accepts the edit. Failed corrections return to the
originating account without replacing a newer draft or reply. Expired
corrections become ordinary unsent drafts and require an explicit send.
Deleting or revoking the original discards its queued corrections. Edits to
the same message are sent in arrival order; unrelated messages remain
independent.

Right-click a group message and choose **Reply privately** to open a direct
conversation with its sender while quoting the group message. The quote
survives closing the recipient chat or switching accounts. Locked or
inaccessible source groups hide their quote previews and cannot be used for
sending until authorized again. Deleting the recipient chat discards its
private-reply recovery, including refusals arriving after deletion.

Chats with unsent text or attachments appear at the top of the chat list,
above pinned chats. Saved text shows after **Draft:** in the chat row; a file
without a caption shows its filename, and a pasted image shows **Image**.
The open chat's text stays in the composer. Sending or removing the draft
restores the chat's usual position. Drafts still respect search, filters,
the archive, and locked-chat privacy.

Attachments, captions, selected mentions, and reply quotes stay with their
chat when you switch chats, close a conversation, or switch accounts.
Return to that chat in that account to resume. Text is saved in the encrypted
archive and survives restarting Zap Faster. Attachment drafts stay in memory
while Zap Faster runs and are discarded when you quit. Clearing or deleting
a chat, or unlinking its account, discards its attachment drafts. A refused
attachment send returns its files, caption and reply quote to the originating
account's chat draft, even if another account is on screen.

Zap Faster also shows when your message was delivered and read when
you click its time or ticks, as on WhatsApp.

Double-click beside a message, or on its edge, to reply to it. A double-click
on its text still selects the word.

Deleting a message asks which copies to remove. **Delete for everyone**
revokes it through WhatsApp. In **0.19.0**, **Delete for me** removes only this
computer's copy, and individual deletions do not sync to or from your phone.

Zap Faster syncs **Delete for me** with your phone and linked
devices; other people keep their copy. This needs a connection, and the local
copy disappears only after WhatsApp accepts the deletion. Phone deletions sync
here too, and history replay cannot restore deleted messages. Interrupted
requests remain saved for that account and retry when it reconnects, including
when an acknowledgement was lost. Unlinking clears those requests. Confirmed
deletions whose local cleanup failed are repaired on restart or reconnect
without another network request. Neither deletion can be undone.

With several messages selected, right-click any selected message to delete
the complete selection. The confirmation shows its count and offers the
available deletion scopes. Messages stay visible until WhatsApp accepts each
request; a failed deletion reports an error and preserves the message.

**Delete chat** and **Clear chat** already sync in 0.19.0, as described under
[Chats](#chats).

A message you send is placed at the time WhatsApp's server acknowledged it, so
a reply no longer sorts above it when this computer's clock runs ahead. Until
the server answers, and for a poll, the message keeps this computer's time.

Zap Faster preserves edited replies and the phone's order for
messages sent within the same second when loading history. If an earlier
build missed a message or put nearby messages in the wrong order, right-click
a message just after the affected area and choose **Reload earlier messages**.
This asks your phone for up to 50 messages before that point and merges them
into the archive. Edited history snapshots restore the reply even when the
original version was never stored on this computer. Keep your phone online.
Availability depends on what the phone sends; messages deleted for you stay
deleted. This action does not require unlinking your account or clearing the
archive.

## Sending while reconnecting

Locally prepared messages wait in account order while WhatsApp reconnects or
returns a sending limit. **Waiting to send** offers cancellation. Prepared
waiting messages survive a restart in that account's encrypted archive. A later
text cannot overtake an attachment already being prepared. Reconnecting does not
cancel an unclaimed prepared message.

Once transmission starts, a timeout or interrupted connection can leave delivery
uncertain. **Send unconfirmed** means the message may have reached WhatsApp; Zap
Faster never resends it automatically. Later receipts still update its status.
Copy or delete it locally after checking whether it arrived. Sends are retried
only after a known pre-transmission refusal, never from an error's text.

## Selecting messages

Choose **Select messages** in the chat's menu (the three dots at the top),
**Select** in a message's menu, or Ctrl-click (Command-click on macOS) a
message. As in WhatsApp Web, every message then gets a check box on the left.
Click anywhere on a message's row, its box included, to add or remove it.
Shift-click adds every message up to the one you click, and a drag adds every
message it passes, scrolling when you hold the pointer at the top or bottom
edge. A drag that starts beside the bubbles, off the text, starts a selection
too. A drag over the text outside a selection still selects the text to copy.
**Forward…** sends the selected messages together, in their original
order. Unticking the last message keeps the selection open; Escape or the
close button ends it. A batch goes out one message at a time, each starting
once the one before it reached WhatsApp, so a picture cannot overtake the
text that came before it. Deleted or unsupported messages, phone-only content,
polls, and interactive messages cannot be selected or forwarded, so they have
no box. If a selected message is deleted, it leaves the selection automatically.
Keyboard focus outlines the box; screen readers identify its message
by sender, time, and a short summary.

## Stickers

The sticker tab works like WhatsApp's: a row of tabs holds **Recent**
(the clock), **Favorites** (the star), each of your packs, and **+** for
adding more. Zap Faster adds **Received** (the speech bubble) after Favorites.
Click a sticker to send it. Animated stickers play on hover.

Click a downloaded sticker in a conversation to see it on its own. Animated
stickers play in the viewer. **Add to favorites** or **Remove from favorites**
uses the same favorites that sync with your phone. Click the sticker or the
backdrop, or press Escape, to close the viewer.


**Recent** holds the stickers you sent, not the ones you received.
Right-click one to take it out of Recent here and on your phone. A sticker
you sent long ago may be gone from WhatsApp's servers: it joins Recent once
you click it in its chat, which asks your phone for it again.

**Received** holds the stickers people sent you that are already
downloaded, newest first, each once. Stickers already in Recent or
Favorites, and stickers from locked chats, stay out of it.

Right-click a sticker in a chat or the picker to add it to your
**Favorites**. Favorites stay in sync with your phone: a sticker you favorite
or unfavorite on either side follows on the other.

Type in the search field to find stickers by emoji (😂), by a word that names
an emoji ("laugh", "duck"), or by pack name. Stickers carry the emojis they
express in their metadata, as WhatsApp's own stickers do.

Under **+**, paste a `signal.art` link from
[signalstickers.org](https://signalstickers.org) (or click **Find packs**), or
open a `.wastickers` file, to import a pack. Signal packs keep each sticker's
emoji. Open a pack's tab and use its delete button to remove it; Zap Faster asks
first, since that deletes the pack's files. Packs are stored as WebP files on
your computer.

A WhatsApp sticker pack someone shares in a chat shows its name, publisher,
and size. Click **View stickers** to download and look at it, and **Add to my
stickers** to keep it as a pack here. To share one of your packs, open its tab
and click the send arrow beside its name: it goes to the open chat as a
WhatsApp sticker pack of up to 60 stickers, with each sticker's emojis.

You can also make packs of your own: type a name under **Make your own** and
click **Create pack**. Right-click any sticker and choose one of your packs in
the menu to add it; a check mark shows the packs it is already in, and
choosing a checked one takes it back out. A pack keeps its own copy of each
sticker, named by the sticker's content, so the same picture is added once
however many chats it came from. Deleting a pack removes its copies and leaves
your favorites and other packs alone.

To make a sticker from a picture, click **Make a sticker from a picture…**
under **+** and choose a PNG, JPEG, WebP, or GIF. Drag the square to choose
the part you want and use **Size** to resize it. A picture with a transparent
background keeps it unless you turn that off, and then the background becomes
white. Type the emojis that describe it, for search here and for WhatsApp's
sticker suggestions, then **Send** it to the open chat or **Add to favorites**.
The sticker is a 512 × 512 WebP under WhatsApp's 100 KB limit.

## Attachments

Paste a picture or copied files, drop files on the window, or select them with
the paperclip.
Pasted or dropped attachments are staged before the composer is drawn, so the
configured send shortcut can send them immediately. Paste handling leaves
other text fields, popups, recording controls and video controls in charge
of their own keyboard input.
They stay above the composer until you send them, with the typed text as a
caption. To reply with an attachment, start a reply and then attach the file.
When sending several files, the caption and reply quote belong to the first
one. Press Escape or click a file's close button to remove it.

With exactly one supported photo (JPEG, PNG, or WebP) staged, select
the **View once** icon (a 1 inside a dashed circle) beside Send. It lights up
when enabled, and sends the photo as a WhatsApp view-once message. You can still
add a caption. The sent message appears as a view-once placeholder in ZapFast;
the recipient opens it on their phone. This option is not available for
documents or batches of attachments. If you add more attachments, or a refused
send returns to a draft with other attachments, remove the extras or turn off
**View once** before sending. ZapFast keeps this choice until you switch it off,
send, or clear the draft.

Pictures and videos sent as a WhatsApp album appear together in a grid. Only
items with the same protocol album identity, sender and local calendar date
are grouped. **Download all** uses the normal per-file download limit and
shows individual failures; **Save all…** copies downloaded files to a folder
without replacing existing files. Open an item to view it individually.

Videos go as playable video messages only when they are MP4. WhatsApp does
not deliver a video message in another format, and Zap Faster does not
convert videos, so a QuickTime `.mov`, WebM, MKV, or AVI file goes as a
document instead and arrives as the original file. Audio works the same way:
MP3, M4A, AAC, AMR, and OGG go as audio messages, other formats as documents.

Staged attachments, captions, selected mentions and reply quotes remain with
their chat and account when you switch or close conversations. Text drafts
persist in the encrypted archive; attachment drafts stay in memory until
Zap Faster exits. Clearing or deleting the chat, or unlinking its account,
discards its drafts. A refused attachment send returns to its originating
chat without filling another account's composer.

Pinned chats stay first. Other chats with unsent text or attachments appear
below the pins once you leave their conversation. Opening a draft, typing,
or clearing it keeps its draft priority until you leave again. Within each
group, chats keep their usual latest-message order. The Favorites chip keeps
the phone's favorites order below pins.

Click a picture above the composer to crop it before you send it. The picture
opens over the window with the region to keep marked, and you can drag that
region by its edges and corners or move it whole. **Turn left** and **Turn
right** turn the picture a quarter at a time, and **Reset** puts it back the way
it started. Press Enter or click the check to keep the crop, or Escape or the
close button to leave the picture as it was.

Keeping a crop does not change the file you picked. Zap Faster writes the cropped
picture beside its other media and sends that one. Opening the cropper again
starts from your original with the crop you chose, so you can widen it back out
without a second pass over the picture.

Automatic downloads keep the 64 MiB limit. Visible stickers download
again when automatic downloads are off, within that same safe limit. Clicking
**Download** explicitly allows attachments up to 2 GiB, including documents and
videos. Downloads stream to a temporary file with at most two active transfers;
verified files enter the cache only after the protocol library checks their
integrity. Transfer progress and cancellation appear beside the attachment.
Cancelled, failed, oversized, or timed-out transfers discard their partial file.
Images above 64 MiB can be saved and opened in another app; Zap Faster does not
load those files into memory for an in-process image preview or clipboard copy.
Attachment preparation and uploads run in selection order, with progress and
cancellation above the composer. Upload progress counts encrypted transfer bytes.
Unfinished attachment preparation is not recovered after closing the app.
If an attachment has expired, Zap Faster asks your phone to upload
it again. A live-location share shows the position this linked device received;
WhatsApp sends later positions only to the phone. Zap Faster marks when newer
positions are available there, but cannot follow the moving location here.

## Locations

**Send location** in the plus menu beside the message field opens a dialog that
takes a spot as text: a pair of coordinates such as `-23.5505, -46.6333`, or a
link to a spot on Google Maps, Apple Maps, OpenStreetMap, or a `geo:` URI.
Coordinates may be separated by a comma or a space, and written with a dot or a
comma as the decimal mark. The dialog shows the spot and a link to it before
anything is sent, and refuses text it cannot read instead of guessing a place.

What goes out is a location that stays where it was sent. A position that keeps
moving is shared from the phone only, so a linked device such as Zap Faster sends
the spot.

A location shows in a chat as a card with its coordinates, the place name when
the sender's phone sent one, and a link that opens in your browser's map.

## Interactive messages

Business templates and button messages show their image above the formatted
text, with options in separate rows below the timestamp. Lists open a grouped
choice dialog, and carousels show separate image cards in a horizontal strip. You can select the message body, use **Copy text** to
include its option labels, and find these messages through search.

- **Web links** have an external-link icon. Click one to open it in your browser,
  or focus it with the keyboard and press Enter.
- **Reply buttons** send the selected response immediately, quoting the original
  message. Zap Faster includes the option identifier so the business can recognize
  the choice. Legacy buttons, hydrated templates, and native-flow quick replies
  are supported.
- **Simple lists** open a dialog with section headings and descriptions. Select an
  item to send it. Legacy single-select lists and native-flow `single_select`
  menus are supported. Opening or dismissing the dialog does not send anything.
- **Copy-code buttons** copy the supplied code to your clipboard without sending
  a message.
- **Unavailable options** have a phone icon and muted text. Forms, payments,
  shopping flows, calls, and carousel choices need WhatsApp Web or your phone.
  Hover over an option to see its explanation. Unsupported or incomplete actions
  never send a guessed text response.

Replies require a connection and a writable chat, and cannot be sent to your own
outgoing cards. During a send, the card waits for its result before accepting
another reply. A failed send shows the normal failure status and allows another
attempt.

**Replies from your other devices** appear as ordinary replies, with a quote
when the original message is included.

Images follow the same automatic-download setting, size limit, and retry
behavior as other photos. Click a downloaded image to open it.

| Dark theme | Light theme |
| --- | --- |
| ![Synthetic business message with an image, three reply buttons, a quoted reply, and a website link in the dark theme](/screenshot-interactive-media.png) | ![The same synthetic interactive messages in the light theme](/screenshot-interactive-light.png) |

![Synthetic reply, list-selection, copy-code, and unavailable-form actions](/screenshot-interactive-actions.png)

All screenshots use offline demo content.

Previously unsupported messages are recovered automatically from the local
archive when their original data is available and they have not been edited.
Existing cards gain their supported actions too. You do not need to link again.
Edited messages keep their current text, and downloaded images stay available.

Embedded videos, documents, and templates containing only
a reference to server-side text still need another client. A **More content in
WhatsApp Web or on your phone** note marks content Zap Faster cannot display.
Interactive messages cannot yet be forwarded from Zap Faster.

### Lists, polls, and carousels

Lists open a centered dialog, like **Show votes**, with the message's title,
sections, option names, and descriptions.
Click an option or focus it and press Enter to send that selection. Close the
dialog or press Escape to leave without choosing.

Poll options show a result track even before votes arrive, and your selection
has a checkmark. **Show votes** opens participant names and vote times. If phone
history is still arriving, the dialog explains that earlier votes may be missing.
New polls received live start at zero without requesting earlier votes. Polls
received from history or while offline still recover results from the phone.
Polls archived by older builds may also need this recovery because those builds
did not retain whether the poll originally arrived live.

Carousel messages keep each card's image, text, and actions together. The timestamp
sits below the last card when the strip fits. When cards extend beyond the view,
round previous/next arrows appear over the strip. Click an arrow or focus it and
press Enter to move one card at a time. The arrows disappear at their respective
ends; **Shift + mouse wheel** and horizontal touchpad scrolling remain available
over the cards, without a bottom scrollbar. Image downloads follow your automatic-download
setting. Web links and copy-code buttons work; calls and unsupported carousel
reply actions remain unavailable.

| Carousel cards | Poll result details |
| --- | --- |
| ![Synthetic carousel with separate image cards and local actions](/screenshot-carousel.png) | ![Synthetic poll results showing participants and vote times](/screenshot-poll-results.png) |

| Grouped list dialog | Poll selection and results |
| --- | --- |
| ![Synthetic list dialog with section headings, descriptions and option selectors](/screenshot-interactive-list.png) | ![Synthetic poll with vote counts, result tracks and the selected answer](/screenshot-poll-voted.png) |

![Synthetic carousel cards in the light theme](/screenshot-carousel-light.png)

## Videos and photos

Click a video to download it to the local cache and play it in its message,
with sound, a seek bar, a speed chip, and a mute switch; round video messages
play inside their circle. Click the speed chip to cycle 1x, 1.5x, and 2x, as on
voice messages: the voice keeps its pitch, in mono while sped up, and the
speed stays for the next video until Zap Faster quits. Already downloaded videos play from their local file. Playback
waits for the download to finish, and the [manual download limit](#attachments)
also applies to videos. Double-click one, or use the button at the end of its
controls, to play it over the whole window:
Space plays and pauses, the arrows jump five seconds, M mutes, S changes the
speed, and Escape puts it back. The built-in player supports H.264 video in MP4 files; other formats
open in your system player.

Click a downloaded photo to open the native media viewer. Zoom with the wheel,
Ctrl+wheel or a pinch, pan by dragging, and use **Copy image** or **Save as…**.
The thumbnail strip and Left/Right arrows browse an archive page around the
selected photo, including photos and videos outside the loaded transcript.
Missing files offer **Download**, progress and cancellation. Videos play in the
viewer with the existing playback, seek, speed and mute controls. Their normal
bubble click still plays inline; **Open in the viewer** is available in the
message menu. Reply, React, Forward and **Show in the chat** act on the selected
message. Navigation itself never downloads a file.

Open a chat's profile and choose **Media, links and docs** to browse its media,
documents or web links, newest first. Media opens in the viewer, documents can
be saved, downloaded or shown in the chat, and links open in the browser.
Each category lists at most 500 items. Link parsing examines only the newest
4,096 messages, including rejected candidates; the panel says when results are
partial and suggests chat search for older items. A viewer opened on an older
photo instead loads a page around that photo. **Refresh** reloads the listing.
Escape returns from the gallery to the profile before closing it. The gallery
respects the account's screen privacy and authenticated locked-chat folder.

The preview closes when its chat is removed, becomes locked outside the unlocked
folder, or when you close the locked folder.
**Save as…** in a downloaded attachment's
right-click menu keeps a copy wherever you choose.

A motion photo has a play button in its corner. Click it to download the short
clip to the local cache and play it over the photo, muted and in a loop; click
again to return to the photo. In the photo's preview, the play button in the
header plays the clip over the whole window with sound. The clip follows the
same download limit.

## Polls

Choose **Create poll** from the plus menu beside the message field to ask a
question with 2 to 12 answers; turn off **Allow multiple answers** for a
single choice. Click an answer to vote, and click it again to take the vote
back. Creating polls in chats with disappearing messages is not supported
yet.

## Voice messages

Voice notes play in the chat with a seekable waveform. Audio files use a music
icon and a seek bar instead. Both keep the same download, playback, seeking and
speed controls. The Forwarded label follows WhatsApp's forwarding metadata
independently of whether the audio is a voice note or an audio file.

The chip beside the waveform or seek bar cycles the playback speed between
1x, 1.5x, and 2x. Right-click the
message for every speed, including 1.25x and 1.75x. The choice is remembered
for later messages. To give one chat its own speed, right-click the chat in
the list and pick **Voice message speed**. That chat then always plays at its own
speed, and the chip there changes only that chat. Pick **Default** to follow
the speed of the other chats again. When a voice message ends, playback carries on through the
voice messages right after it that you have not heard yet, as on the phone;
any other message ends the run. The speaker's pitch stays the same at every
speed. The first play sends
a played receipt, so your phone marks the voice message as heard. With read
receipts off in your account privacy, the receipt goes only to your own
devices and the sender is not told. With **Send read receipts** off in
Zap Faster's settings, no played receipt is sent. A voice message you have not
heard yet shows a dot beside its length; it goes away once you play it here or
on another device. When the composer is empty, the send button becomes a
microphone. Press Enter or the send button to send the recording, or Escape or
the delete button to discard it. Zap Faster raises the volume of quiet recordings.
Starting a reply before recording includes the quoted message.

## Copying

On Windows, drag a downloaded attachment from its message to another
application or a folder to copy its file. Download it first if it is not yet
available locally. Dragging a video or round video note exports its file
without starting playback.

Select and copy any message text. You can begin dragging in the padding around
the text instead of landing precisely on a letter. A selection across messages
uses WhatsApp's sharing format:

```
[18:21, 8/30/2026] Ada Lovelace: Hello from France!
[18:27, 8/30/2026] You: Sure, I will take a look
```

To copy whole messages, choose **Select** from a message's menu (or
Command-click on macOS, Ctrl-click elsewhere), then select the other messages
you want. Press `Cmd+C` on macOS or `Ctrl+C` elsewhere. The clipboard includes
only the selected messages, in chat order, using the same sharing format,
even if some have scrolled off-screen. Attachments contribute a text label and
any caption; their files are not copied. Copying keeps the selection open.

## Chats

To contact someone without saving their number, open **New chat** and choose
**Message a number**. Enter an international number, including its country
code. Spaces, hyphens and balanced parentheses are allowed. Zap Faster checks
whether the number is on WhatsApp before opening the conversation. Failed
checks show an error and can be retried. Closing or replacing the dialog
cancels its pending lookup; results remain with the originating account.

The search bar finds chats by name, number, or latest message; searches all
messages stored on this computer; and finds contacts without an existing chat.
Use `↑`/`↓` to select a matching chat and Enter to open it ready for typing.
Click a message result to jump to it, or a contact to start a chat. Use
`Alt+↑/↓`, or `Ctrl+Shift+[` and `Ctrl+Shift+]` as in WhatsApp, to switch chats
without leaving the composer (Command instead of Ctrl on macOS), and
`Ctrl+1` through `Ctrl+9` to open the chat at that position in the list. Within an open
chat, `PgUp`/`PgDn` scroll by about a page, and `Home`/`End` (or `Ctrl+End`)
jump to the top or the newest message when the input is empty. With text in
the input, they move the text cursor as in any editor.

By default, `Ctrl+W` closes the window (`Command+W` on macOS). In Settings,
System, turn on **Ctrl+W closes the chat** to close the open chat instead.
Your draft stays with that chat. With this option on, the shortcut does nothing
when there is no visible chat, an overlay is open, or the app is locked. Closing
the window from its own close button keeps its existing behavior.

With a mouse that has them, the **Back** and **Forward** side buttons step
through the places you have visited, as in a browser: between chats, and
between the chat list, Settings, and the wallpaper picker. Forward repeats a
step until you visit something new. The buttons do nothing while a dialog,
menu, or other overlay is open; close it first, with Escape or its own button.

Sending while reading older messages keeps your place. Use the
newest-message button or `End` to return to the latest message when you are
ready. The chat list scrolls to the top after you send, where the chat now is.

While scrolling through a conversation, the date of the first visible message
stays at the top of the transcript. Date labels use your selected language and
local calendar date.

To work through older conversations without jumping back to the top, enable
**Keep chat list position after sending** in **Settings > Chats**. This option
starts off and does not change the order of chats or scrolling inside a conversation.
Under the **Favorites** chip, the list keeps the phone's order and stays in place.

A shared contact message shows the name from its vCard. When the card names a
WhatsApp account, **Chat** opens a private conversation with it and, if the
person is not already in Zap Faster's contacts, **Add** saves them, adding them to
your phone's contacts if you chose that for the last contact you added. A card with only a local number shows the number.

A chat or group with disappearing messages shows its timer in the contact or
group details, opened from the header or by right-clicking a row in the chat
list. The line reads **Off** when the timer is turned off and stays away
entirely when Zap Faster has never heard of a timer for that chat. Read-only for
now: change the timer on your phone, and messages Zap Faster sends carry it,
while messages received here stay in the archive after they disappear on your
other devices.

The chips under the search bar narrow the list to **Unread**, **Private**
(one-to-one chats), **Favorites** (in your phone's order), or **Groups**.
Followed channels have their own **Channels** chip, and **Archived** opens the
archived chats. A chip with unread chats shows how many it
has. Click the active chip again, or **All**, to see every chat. The
filter applies only to this list: search and the archive still show everything,
and it resets when Zap Faster restarts. As on the phone, the list leaves out
people you have never exchanged a message or call with, though your phone sends
their chats along with the others. One stays listed while it is pinned, a
favorite, labeled, unread, locked, open, or holds an unsent draft. Search still
finds them, and the first message or call brings one into the list.

Right-click a chat to pin, favorite, archive, mark as unread, or mute it for
eight hours, one week, or indefinitely. These changes also apply on your
phone. **Clear chat** is in this menu and in the chat header's menu; it needs a
connection and asks first. It clears messages on this computer and your phone,
removes cached media from this computer, and keeps the chat. Media saved in
your configured download folder stays there. **Delete chat** in
the same menu also needs a connection and confirmation. Groups and channels
can be left from the same menu, keeping their history here. `Ctrl+E` (Command+E on macOS) archives the
open chat, or unarchives it from **Archived**, as in WhatsApp.

Archived chats stay archived when new messages arrive, unless your phone's
**Keep chats archived** (in WhatsApp's chat settings) is off: then a new message,
received or sent, brings an archived chat back to the list, as on the phone.
Until a phone has sent its setting, **Keep chats archived** in Zap Faster's
Settings decides for that account. Messages older than the archiving, duplicate
deliveries, and history replay leave archived chats in place.

The search icon in a chat's header (`Ctrl+F`) opens a pane listing that
chat's matches, newest first. Its calendar narrows them to one day.

Click the chat header to see its picture, number, and group members. When
WhatsApp lets you edit a group's info, rename it with the pencil beside its
name and click its photo to change it. Clicking a `chat.whatsapp.com` invite
link shows the group and joins it without leaving Zap Faster.

Chat info also shows the group's description above its members.
The text is selectable, preserves line breaks and emoji, and scrolls when long.
Descriptions stay in each account's encrypted archive across restarts and update
when WhatsApp announces changes. Groups without a description omit the section.
Description editing still requires your phone.

## Browser chat links

Zap Faster can open `whatsapp://send` links from a browser or
another application. The link opens its recipient on the active WhatsApp
account and puts any `text=` message in the composer as an unsent draft.
An existing draft is kept, with the link's text appended on a new line.

On Linux, choose Zap Faster as the handler after installing it:

```sh
xdg-mime default zap-faster.desktop x-scheme-handler/whatsapp
xdg-open 'whatsapp://send?phone=15550100123&text=Hello%20from%20a%20link'
```

Use the full international phone number, including the country code. Text
is URL-decoded, including accents, emoji, and line breaks. You can also pass
the same link, a `wa.me` or `api.whatsapp.com/send` web link, or a number
directly to `zap-faster` on the command line:

```sh
zap-faster '+1 (555) 010-0123'
zap-faster 'https://wa.me/15550100123?text=Hello%20from%20a%20link'
```

Numbers must contain 6 to 15 digits, including the country code. Spaces,
parentheses, dots and hyphens are accepted as formatting. Web links opened
in a browser still go to the browser; pass them to `zap-faster` to open
them here. Only recognized WhatsApp hosts and send paths are accepted.

If Zap Faster is already running, the link goes to that copy and brings its
window forward. It waits for the account's archive to load and for the app
to be unlocked. A link cannot open a locked chat outside its locked folder.
Very long links are refused before opening a chat.

## Locked chats

**Lock chat** in a chat's right-click menu moves it into a locked folder: it
leaves the chat list, search, and the unread count, and never raises a
notification. The lock syncs with your phone and other linked devices.

Choose **Locked** beside the chat filters and type your local code to open
them; the first time, Zap Faster offers to set one up. The code is separate from
your phone's and adds no encryption beyond the encrypted message archive.
Leaving the tab, changing the code, or closing the window hides them again.

## Labels

Labels are yours alone. They stay on this computer, they never reach your phone,
and nobody else sees them. They are not WhatsApp Business labels, and Zap Faster
does not read or change those. Open **Labels** in any chat's right-click menu
and choose **Manage labels…** to make one, with a name and one of the offered
colours. Zap Faster keeps up to twenty.

A chat can wear several labels at once. The **Labels** submenu of a chat's
right-click menu lists them, with a checkmark beside the ones the chat wears;
click one to add or remove it. Deleting a label takes it off every chat and
nothing else; the chats keep their messages.

Once a label exists, a row of label chips appears under the other chips, one
per label, with its colour and the number of unread chats wearing it, followed
by a **+** that opens the label manager. Pick a label to list only the chats
wearing it, channels included. A label is one more chip: picking it lets go of
**Unread** or **Groups**, and picking one of those lets go of the label. Like
the other chips, it does not narrow search or the archive. Click the active
label chip again, or **All**, to see every chat.

The button beside **New chat** (`Ctrl+B`) collapses the list to a narrow column
of avatars: unread chats show their badge, hovering names a chat, clicking opens
it, and `Ctrl+B` brings the full list back.

## Notifications and the tray

Closing the window keeps Zap Faster linked in the tray. Click the tray icon or
launch the app again to reopen it. To quit, use **Quit** in the tray menu, or on
Linux **Quit Zap Faster** in the launcher's right-click menu (KDE Plasma's task
manager and application menu, GNOME's Dash), which runs `zapfast quit`.
Launchers that support the Unity Launcher API
show the unread count as a badge on the app icon: KDE Plasma's taskbar, with
**Show badges** enabled in the Task Manager settings, and GNOME's Dash to Dock
or Dash to Panel. Windows overlays a compact unread-message count on Zap Faster's
taskbar button while the window is open, using `99+` above 99. Windows must use
its regular taskbar icon size for overlays to appear. The count
does not count toasts remaining in Windows notification history. On Linux and
Windows, notifications show the chat picture and open the chat at the message
they announced when clicked. On Linux, a notification that arrives while the
window is open behind others also highlights Zap Faster in the taskbar until you
switch to it. Muted chats do not send notifications, and
archived chats stay quiet while they remain archived. You can change both settings.
With several numbers linked, the bell beside a number in the account switcher
mutes all of that number's notifications while it stays connected, and leaves
its unread chats out of the taskbar badge, as a muted chat's are.
On Windows, while every display is off, ZapFast leaves new-message notifications
to the phone instead of queuing desktop banners to appear when the display turns on.
On macOS, the Dock icon also shows the unread count and clears it while the app
is locked. In **Settings > Notifications**, **Show notification sender** controls
chat names, pictures and chat-specific sounds. Turning it off also hides message
content. **Show notification content** can hide previews while retaining the chat
name. These choices apply separately to each account; locks always mask previews.

macOS plays notification sounds through Notification Center, so the system's
Focus and Do Not Disturb settings control them. Custom files must be at most
16 MiB, shorter than 30 seconds, and decodable as mono or stereo audio. Unsupported
files use the system notification sound. The prepared WAV copy stays locally in
`~/Library/Sounds`. Banners still require notification permission and enabled
banner settings for the installed Zap Faster application.

Linux hidden startup waits up to 15 seconds for a tray watcher. If none appears,
Zap Faster opens its window so you can still reach it.
To hear less from busy chats, set **Limit repeat notifications** in
**Settings > Notifications** to direct messages, groups, or both: a chat then
sends one notification, stays quiet for 10 minutes, and notifies again once you
open it. In a group, a mention of you or a reply to you always notifies.

Press `Ctrl+/` or click the keyboard button under the composer to list all
shortcuts.

The open chat's messages form one list, a single Tab stop reached with
`Alt+2` (`Control+2` on macOS). In the list, Up and Down move through messages,
Home and End go to the oldest loaded and the newest message, and PgUp and PgDn
move about a screen; Home on the oldest loaded message loads older history.
The current message stays current while new messages arrive, and follows jumps
the app makes on its own, such as opening a quoted message.

Notification sounds use the native system notification output. Under Settings,
**Microphone and sound output** chooses the account's devices for recording,
voice/video playback and calls. A missing or failed chosen sound output falls
back to the system default with a warning. Recording refuses an unavailable
chosen microphone instead of switching silently. Active playback follows output
changes or pauses with an error. Physical Bluetooth/HDMI routing and device
removal still require attended testing.

Incoming calls play a local two-chime ringtone through the selected call output.
It stops when you answer, decline, or the caller ends the call. Outgoing ringback
starts after the other device reports ringing. Call microphone cleanup uses the
speaker audio as a reference to reduce acoustic echo, with gentle noise
suppression. It runs locally and never records or uploads the reference audio.
If cleanup becomes unavailable, microphone audio is sent without it and an
explicit warning appears.
Headphones can still help when room acoustics or speaker volume cause residual echo.

The rate-limit queue adapts [pulgueta's PR #364](https://github.com/crmne/zapfast/pull/364),
with fork-specific durable reconnect recovery and transfer handling. The gallery
and viewer adapt [pulgueta's PR #264](https://github.com/crmne/zapfast/pull/264)
and [LisandroNahuelH's PR #189](https://github.com/crmne/zapfast/pull/189).
These contributors retain credit for their original work; the fork's adaptation
preserves the existing multi-account and privacy architecture.
Translated submenu sizing also adapts [vncsalencar's PR #435](https://github.com/crmne/zapfast/pull/435),
with a viewport cap for narrow windows.

Offline previews are available as `gallery`, `gallery-docs`, `gallery-links`,
`gallery-pending`, `gallery-failed`, `viewer-missing`, `viewer-progress`,
`viewer-video` and `transfer-progress`, through `--demo --demo-page`.

### Original documents

Choose **Document** on a staged attachment tile to send original image file bytes or lossless PNG clipboard pixels. Original file names and existing file metadata are preserved; clipboard pixels have neither an original filename nor EXIF. See [Original documents]({{ site.baseurl }}/original-documents/) for refusal recovery and acceptance checks.

## Custom emoji fonts

Under **Settings > Appearance > Emoji style**, choose System, Noto Color Emoji,
or **Choose a colour emoji font file**. Supply a local font you are allowed to
use; Zap Faster does not download or include proprietary emoji libraries.
Apple emoji already come from the system font on macOS in System mode.

Import accepts CBDT or sbix colour bitmap fonts (TTF, OTF or collections) up to
256 MiB, using the first face of a collection. COLR/SVG-only fonts and image
packs are unsupported. The application keeps its own copy, so the original
can move. All accounts share the choice. **Restart Zap Faster to apply it.**
Missing emoji in a custom font have no bundled Noto fallback. If the custom
font cannot load at startup, Settings shows the reason and the selected
built-in style remains active. Choose System or Noto to reset the choice and
remove the application's imported copies, then restart.

## Custom interface fonts

Under **Settings > Appearance > Font**, choose System, Inter, or **Choose a font
file**. Zap Faster accepts `.ttf`, `.otf`, `.ttc`, and `.otc` files and keeps a copy
in the `font` folder of its application state directory. The original file may
then move without affecting the app. A collection uses its first face.

Your font leads the interface text. Characters it lacks fall back to the chosen
System or Inter font and the existing installed-font fallbacks. Emoji, monospace
text, and tabular playback timers keep their existing rendering. Variable fonts
use their weight axis for regular, medium, semibold, and bold text. Static fonts
use the chosen face for all weights.

Choosing System or Inter removes the custom copy. An invalid or unavailable
custom font reports an error and returns to the built-in choice. Font files are
limited to 32 MiB. The font choice is shared by all accounts.

### Messages appearance

Settings > Appearance > Theme offers **Messages**, an optional appearance with
round bubbles and a frosted conversation header. It follows your computer's
light or dark appearance. The default theme is unchanged.

macOS uses its native sidebar material, supported Windows 11 versions use Mica,
and Hyprland can blur the desktop behind the window. Other systems retain an
opaque background. Choosing this theme while a window is open may require
closing and reopening the window before desktop translucency appears. The
rounded bubbles and header are available immediately.

### Portable preferences

To keep process preferences beside the executable, create
`zap-faster-portable-settings.txt` there containing exactly
`zap-faster-portable-settings-v1`. Restart Zap Faster. It reads and saves the
adjacent `settings.json`; you may copy your existing process preferences there
before restarting. The executable folder must be writable. Removing this marker
returns to standard process preferences without deleting either file.

This opt-in does not move linked accounts, their archive keys, caches, account
roster or custom themes. Moving an executable and its preferences to another
computer does not transfer WhatsApp identity. Use an encrypted account backup
for that. The updater's existing portable-installation marker alone does not
enable portable preferences.

### Local contact pictures

Open a person's contact information and choose Set local picture. Zap Faster
copies and scales the image into that account's own state folder; the source
file may then be moved. Reset local picture returns to their WhatsApp picture.
This changes display only on this computer, never uploads a picture, and never
replaces the photo downloaded from WhatsApp. Separate accounts keep separate
overrides. Encrypted full account backups include these local pictures.

## History recovery and downloaded files (development builds)

Before pairing a new account, choose **Recent sync** or **Full sync**. Full asks
for a larger initial history profile and continues requesting older history and
attachments across eligible chats. It can take a long time and use substantial
network traffic and disk space. The phone and WhatsApp determine what remains
available; Full cannot guarantee a lifetime archive.

Settings lets you change recovery mode, background media recovery, request
budgets and the storage budget. Reopening resumes durable progress. A storage
budget pauses new background downloads; it does not discard archived files.
Recovery counts and the oldest recovered date describe what has arrived, rather
than treating the phone's percentage as proof of completeness.

Under **Settings > Files**, choose the download root, account/chat or
account/chat/month folders, and readable filenames. These affect future copies
only. Verified private media keeps its archive identity, so moving an exported
copy does not break playback. Existing files are preserved.

## Encrypted account backups (development builds)

Under **Settings > Files**, back up the account and downloaded files with a
passphrase of at least twelve characters. The connection pauses while saving.
Keep the passphrase separately; it cannot be recovered. Missing referenced
attachments stop the backup instead of being silently omitted.

Restore creates a separate account and leaves it offline. Existing accounts are
never overwritten. A full backup includes linked-device identity, preferences,
the encrypted archive and its key, and media. Choose **Resume connection** only
after closing the original installation: pending operations may send on resume,
and the same restored identity must not connect from two computers at once.
The phone can invalidate a saved session, requiring relinking. Actual power-loss
and platform-keyring acceptance remain unverified.

## Voice and video calls (development builds)

In a direct conversation, use the voice or video button in the header. Incoming
calls offer answer or decline. Calls use the selected microphone/sound output;
video requires camera permission. The call device controls let you select a camera
and refresh the device list. A selection applies the next time you turn the camera
on; an unavailable selected camera reports an error. Ending or locking
the app stops capture. Local call history stays in the account's encrypted archive.

Native 1:1 calling is implemented through the official protocol library, but
two-device interoperability and physical permissions/device removal still need
attended acceptance. Group calls await upstream interoperability fixes. Calls
embedded as interactive-message actions have separate unsupported-action rules.

## Status (development builds)

The **Status** chip beside the chat filters opens stories received while this
instance is running. Stories expire 24 hours after their original timestamp and
are not added to the permanent conversation archive. Viewing sends no read
receipt. Photos stay inside the filtered viewer; downloaded videos open in the
system player. Locked senders and screen privacy retain their restrictions.

## Automatic exports (development builds)

With automatic downloads enabled, files copied to your chosen folder default
to those received since enabling the option. **Include older files** opts into
historical copies independently of Full private history recovery. Original
document names are optional and remain sanitized and collision-safe.

## Undo and backup merging (development builds)

Archive and Delete for me offer **Undo** for five seconds before their command
is sent. Closing or crashing during this grace period leaves the request unsent.
An accepted deletion cannot be reversed through Undo.

Backup restoration can optionally merge historical messages into the current
account after identity matching and a review of the exact changes. Current
credentials and pending work stay in place. Imported pending sends never run;
ambiguous edited alternatives are retained separately in the encrypted archive.
Confirmed deletions in the backup are applied as listed in the merge preview.

## Unused cache cleanup (development builds)

Under account storage settings, **Unused cache cleanup** can remove old files
that are no longer referenced by the archive. It is off by default. Every
archived attachment, background recovery file and tracked export stays protected.
The age uses the file modification time. See the [cache policy](../cache-retention.md)
for safety limits and remaining platform validation.

## Small windows and larger text (development builds)

Dialogs scroll when their contents exceed the window. Confirmation actions wrap,
and contact name fields stack in narrow dialogs. Menus keep long action labels
readable instead of hiding their endings. Emoji and reaction pickers fit the
window; their tabs can scroll horizontally.

The account switcher scrolls when several accounts are linked. Gallery downloads
can be canceled while starting, as well as during transfer. Hover over a shortened
document or link name to read it in full. Backup forms explain mismatching
passphrases before opening the file picker.

Screen privacy also hides account-switcher identities and call names/video
previews according to its selected options. Hovering reveals them as it does
elsewhere in the app. Call preparation remains cancellable; its status does not
mean that the other device has started ringing.


### Development call controls and launch links

In development builds, **Minimize call** collapses the call panel to a movable
video overlay, or a compact voice bar. Mute, camera, end-call and restore controls
remain available. Minimizing keeps the call and its media devices running.
Screen privacy and locked-chat protections also apply to the overlay.

Voice and video calls negotiate standard Opus through whatsapp-rust. Camera
frames retain capture timing; excess native samples are drained and a dropped
encoded frame requests a fresh keyframe. These changes have synthetic source
checks; bidirectional audio after camera-on and smooth peer video still need an
attended call on the installed build.

A `https://chat.whatsapp.com/…` link passed to the executable opens the existing
invite preview. It waits for unlock and a connection on the account that received
the launch. Joining requires confirmation in the preview. Older running copies
that cannot accept the link must be quit before launching it again.

The sticker picker shares two background download slots across recent and chat
stickers, resumes after rate limits, and refreshes its shelves in small batches.
An explicit click during a picker fetch is remembered and can request a re-upload
if that background fetch fails.
