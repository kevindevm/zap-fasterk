# Portable encrypted account backups

Settings, Files saves a passphrase-protected account backup. At least twelve
passphrase characters are required. Keep the passphrase separately; Zap Fasterk
cannot recover it. Saving never overwrites an existing backup.

Full backups include an independent, consistent SQLCipher archive snapshot and
its key, linked-device identity and session SQLite side files, account settings,
wallpaper, local contact-picture overrides, saved stickers, and downloaded media, avatars and sticker caches.
Referenced attachments saved outside the account cache are included too. The
account connection and downloads must be paused for the entire snapshot and copy.
Missing referenced files stop the operation instead of silently omitting them.
Global preferences, application credentials and custom themes are excluded.

Data, filenames, and the archive key are encrypted with AES-256-GCM in sequenced
64 KiB authenticated frames. PBKDF2-HMAC-SHA256 derives the package key using a
fresh 128-bit salt and 600,000 iterations. The authenticated completion frame is
required; trailing bytes are rejected. The archive remains encrypted throughout
staging. Session and attachment staging is private and is removed on cancellation.

Restore verifies the complete package, archive and session database integrity
before installation. It creates a separate account and never overwrites existing
account folders. It rewrites attachment and wallpaper paths, and rekeys the
archive using a verified OS-keyring credential bound to its new account folder.
Files are staged beside their destination folders before publication. A durable
transaction records the exact new account, staging folders, random ownership token
and credential fingerprint before any account data or key is published. Account
roster registration is durably saved after publication. Startup reconciles these
transactions before opening account workers: registered restores are retained;
unregistered restores lose only their verified transaction-owned folders and key.
Ordinary failures use the same rollback path. Conflicting folders, changed keys,
invalid records, and inaccessible keyrings stop recovery without deleting unrelated
data. Private decrypted preparation folders carry their own ownership marker and
are discarded on restart. No overwrite is offered,
so an existing account and its linked-device identity remain intact.

A full backup retains a linked-device session and opens offline after restore.
In Settings, Files, choose Resume connection and confirm that the original
installation is closed. Resuming can send pending messages and account changes
from the backup. Account preferences are preserved for that resumed connection.
The phone may invalidate that
session, so relinking can still be necessary. An older archive-only backup opens
as an offline archive and cannot resume a connection. Restoring the same session
on multiple computers at once is not a supported connection arrangement.

The format supports one million files, 16 GiB per file, one TiB total payload,
and a 64 MiB manifest. Restore requires free space for staging and installed
copies. The backup contains sensitive session credentials; possession of the
backup and passphrase permits access to the restored account.

Synthetic tests simulate interruption after journal reservation, key publication,
cache publication and state publication, plus registered completion and conflicting
destinations. Windows, Linux and macOS use non-replacing publication primitives;
filesystem/keyring durability after power loss and attended restore remain platform
validation requirements. A conflict may require manual recovery before startup.
