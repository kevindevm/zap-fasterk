# CLI companion (`zapfasterk-cli`)

Console companion to Zap Fasterk for local automation: listing, reading,
searching, watching and sending WhatsApp messages from `cmd`, PowerShell or
scripts. It talks to the already-running app on the same computer; nothing
leaves the machine and no network service is involved.

All ids and names below are fictional. Substitute your own account and
conversations.

## Requirements

- The app (`zap-fasterk.exe`) must already be running.
- Each account must opt in: Settings, Privacy, Allow local command-line access.
  The default is off, and restoring a backup turns it back off.
- With a single linked account `--account` may be omitted; with several, pass
  the numeric account-folder id.

## Conventions

- Every listed row prints as one JSON object per line, suitable for pipes
  (`| findstr`, `jq`).
- Problems print to stderr with a nonzero exit code.
- `zapfasterk-cli` waits for each answer like a normal console program. With
  the GUI binary itself, either wait explicitly
  (`start /wait zap-fasterk.exe chats`) or pipe the output.

## List conversations

```powershell
zapfasterk-cli chats            # direct chats, most recent first
zapfasterk-cli groups           # group chats only
zapfasterk-cli channels         # followed channels only
zapfasterk-cli chats --kind all # everything together

zapfasterk-cli chats --sort name    # alphabetical
zapfasterk-cli chats --sort unread  # unread first
zapfasterk-cli channels --unread    # only chats needing attention
zapfasterk-cli chats --archived     # archived are hidden by default
zapfasterk-cli chats --limit 100    # pages of 25 are drained for you
```

Each row carries `id`, `name`, `kind` (`direct`, `group` or `broadcast`),
`unread`, `last_activity`, `pinned` and `archived`.

```powershell
zapfasterk-cli contacts --all  # address book; without --all only saved names
```

## Read and search

```powershell
# recent messages of one conversation (chat ids come from `chats`)
zapfasterk-cli messages 15550101111@s.whatsapp.net --limit 10

# one archive page with its cursor, for scripts
zapfasterk-cli read --chat 15550101111@s.whatsapp.net --limit 10

# search message text and captions everywhere, or in one chat
zapfasterk-cli search "fotos del viaje"
zapfasterk-cli search "fotos" --chat 15550101111@s.whatsapp.net

# follow new arrivals for 60 seconds (Ctrl+C stops an endless watch)
zapfasterk-cli watch --seconds 60
```

Reading never sends read receipts. Disappearing, phone-only and locked chats
are always withheld, and locked chats are refused everywhere.

## Send

The target accepts a full chat id, a bare phone number, or a conversation
name. Numbers and names must single out one conversation; otherwise the error
lists the candidates with their full ids.

```powershell
# these three reach the same conversation
zapfasterk-cli send 15550101111@s.whatsapp.net "Hola" --request-id a1b2c3d4e5f60718293a4b5c6d7e8f90
zapfasterk-cli send 15550101111 "Hola" --request-id a1b2c3d4e5f60718293a4b5c6d7e8f90
zapfasterk-cli send Marta "Hola" --request-id a1b2c3d4e5f60718293a4b5c6d7e8f90

# text from stdin (-), files, and both
echo Hola | zapfasterk-cli send 15550101111 - --request-id b1b2c3d4e5f60718293a4b5c6d7e8f91
zapfasterk-cli send 15550101111 "Mira esto" --file .\foto.png --request-id c1b2c3d4e5f60718293a4b5c6d7e8f92

# repeat safely: the same id returns the original instead of duplicating
zapfasterk-cli send 15550101111 "Hola" --request-id a1b2c3d4e5f60718293a4b5c6d7e8f90

# check what a send did
zapfasterk-cli status 15550101111@s.whatsapp.net --request-id a1b2c3d4e5f60718293a4b5c6d7e8f90
```

### The request id

Every send carries a 32-digit hexadecimal id that you choose up front. It is
an idempotency key: the app reserves it before sending, so repeating the same
command (after a cut connection, a closed terminal, a doubt) returns the
original message instead of sending twice.

- Passing `--request-id` is optional. Without it the CLI mints one and prints
  `Request id: ...` on stderr; save it if you may want to check or repeat.
- Reusing an id with different text or a different chat is refused.
- A fresh run mints a fresh id, so only repeat deliberately with a printed id.

## Control the app

```powershell
zapfasterk-cli reload-themes  # reload palettes without showing the window
zapfasterk-cli quit           # quit the background app
zapfasterk-cli --help         # full option list
```

## Reference

The exact opt-in contract, privacy rules, paging and fragment limits live in
[local automation](local-automation.md).
