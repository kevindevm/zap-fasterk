# ZapFaster

**Native WhatsApp for power users. Built by one.**

ZapFaster is a fork of [ZapFast](https://github.com/crmne/zapfast), maintained by a daily WhatsApp power user. The goal is to bring the useful workflows people once relied on WhatsApp plugins for into a fast native app, and get fixes and new features into users' hands faster.

That means better ways to manage busy conversations, work with attachments, navigate by keyboard, and control your desktop experience. Plugin-inspired workflows are a direction for development, not a claim that every old plugin feature is already available. Requests from people who use WhatsApp every day help shape what comes next.

ZapFaster is a small native WhatsApp client for Windows, built with Rust, [egui](https://github.com/emilk/egui), [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust) and [fastframe](https://github.com/crmne/fastframe). It links to your phone as a companion device. No browser engine, telemetry, hosted backend or ZapFaster account is required. This fork targets Windows only.

The original project and its contributors made this possible. This fork preserves the MIT license, copyright notices and contributor credit. It has its own maintenance and release schedule. Contributions and fixes from either project are welcome.

## Download and run

Download installers and portable builds from **[ZapFaster releases](https://github.com/eternalwaitt/zap-faster/releases)**. Upstream ZapFast installers, its Homebrew tap, AUR packages and website downloads install the original project.

The current fork release, [v0.30.0](https://github.com/eternalwaitt/zap-faster/releases/tag/v0.30.0), is available for Windows. To build from source, use the toolchain in `rust-toolchain.toml`:

```sh
git clone https://github.com/eternalwaitt/zap-faster.git
cd zap-faster
cargo run --locked --release
```

Windows builds need Visual Studio C++ Build Tools, CMake, LLVM/libclang and Perl. See [the build and packaging guide](PACKAGING.md).

## How ZapFaster differs from ZapFast

ZapFast provides the native foundation. ZapFaster builds on it with its own product priorities and release schedule: more control for frequent WhatsApp users, a shorter path from a reported problem to a reviewed fix, and contributions that use AI tools with human accountability.

The following improvements are integrated into this fork's source, relative to the ZapFast baseline it started from. Some come from original ZapFast contributors; others were developed by this fork's maintainer. Both projects continue to evolve, so this is not a live comparison with upstream's latest version. Availability in a download depends on its release.

- More reliable text selection, screenshot pasting and scrolling; Windows attachment dragging.
- Media albums, attachment drafts tied to their chat, crop/rotate before sending, and an archive-backed gallery and media viewer.
- Durable waiting sends, conservative recovery after reconnects, and cancellable attachment transfers with progress.
- Private group replies, multi-recipient forwarding, batch deletion and safer failed edits.
- Unsent drafts at the top of the chat list and an option to keep the list position after sending.
- Audio files distinct from voice notes, AAC video sound, playback speeds and motion photos.
- On-demand local Whisper voice transcripts, with optional automatic transcription (off by default).
- Chat links as unsent drafts, unsaved phone-number chats, location sharing and phone-number actions.
- Screen privacy, read-receipt privacy, attachment storage usage and notification throttling.
- Group descriptions and disappearing-message timers, keyboard shortcuts and mouse-side navigation.

See [ZapFast's pull requests](https://github.com/crmne/zapfast/pulls) for work proposed to the original project. Source links and author credit for changes we integrate remain in commit history and release notes.

## Existing ZapFast users

Quit ZapFast before starting ZapFaster. The fork intentionally retains ZapFast's existing settings, session, encrypted archive, OS keyring identity and single-instance lock. It uses the existing `zapfast` data directories; installing both does not create independent accounts. Back up your data before switching, and use one application at a time. The fork does not read your archive during builds or development.

ZapFaster checks its own GitHub releases for updates and verifies them with its own Ed25519 signing key. It does not consume upstream ZapFast updates. Update signatures are separate from Windows Authenticode; download pages state any platform signing limitations.

The [settings and files guide](docs/_reference/settings-and-files.md) describes local storage, encryption and network behavior. GIF search contacts Giphy only when used and configured; preview links and location map links may contact the relevant website when opened. Whisper downloads its 1.5 GB multilingual model from Hugging Face when first requested, verifies its pinned SHA-256 digest, and transcribes on your CPU. Transcripts are stored in the encrypted account archive; model files are in the account cache. No feature uploads message content to a transcription or analytics service.

## Contribute

Help build the WhatsApp client you want to use every day. Bug reports, feature requests, code, testing, translations and documentation are welcome through our [issues](https://github.com/eternalwaitt/zap-faster/issues) and [pull requests](https://github.com/eternalwaitt/zap-faster/pulls).

**AI-assisted coding is welcome.** A developer must review the code, understand it, test it, and sign off on the submission. Maintainer review and required checks still apply before merging. See [CONTRIBUTING.md](CONTRIBUTING.md) for how to get started and what a good contribution includes.

We welcome compatible changes from ZapFast and can contribute fixes back. When bringing work from another project, link the source and credit its authors.

```sh
cargo run --locked --features demo -- --demo
```

The demo uses synthetic conversations offline. [DEMO.md](DEMO.md) documents its screens and layout checks.

## Disclaimer and license

This is an unofficial client, unaffiliated with WhatsApp or Meta. Using unofficial clients may violate WhatsApp's terms and risk account suspension.

MIT; see [LICENSE](LICENSE) and [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). Original ZapFast copyright and attribution are retained. Fonts, icons, wallpaper and notification sounds keep their respective licenses.
