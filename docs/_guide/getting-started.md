---
title: Getting Started
description: Install Zap Fasterk, link your phone, and load chat history.
nav_order: 2
---

## Install

The [Download page](/download/) has the Windows installer and portable builds.

Or build from source with a recent stable [Rust](https://rustup.rs):

```sh
git clone https://github.com/kevindevm/zap-fasterk zapfasterk
cd zapfasterk
cargo run --locked --release
```

The build needs Visual Studio C++ Build Tools, CMake, LLVM/libclang, and
Perl; `rust-toolchain.toml` pins the Rust version. libopus and the H.264
decoder build from source.

## Link with your phone

On first launch, Zap Fasterk follows your system appearance. Choose another theme
in **Settings > Appearance**; Zap Fasterk remembers your choice.

Zap Fasterk links as a companion device, like WhatsApp Web. Start it and either:

- scan the QR code with your phone (WhatsApp, **Settings**, **Linked
  devices**, **Link a device**), or
- click **Link with phone number** and enter the eight-character code on your
  phone.

The link survives restarts. Your phone does not need to stay on the same
network or be online to read messages already stored in Zap Fasterk.

Several numbers can stay linked in one window. Click your own picture at the
top of the chat list to switch between them or to add another (**Settings >
Account > Add account** works too); a dot on it means another number has
unread chats. `Ctrl+Shift+A` (Command+Shift+A on macOS) switches to the next
number. Each number keeps its own keys, archive, media, and
notification, receipt, download, and wallpaper settings. To silence a number
without unlinking it, click the bell beside it in the switcher: it stays
connected and its chats stay readable, but it sends no desktop notifications
until you click the bell again. A muted number shows a crossed-out bell there,
and its unread chats count toward neither the dot nor the taskbar badge.

## Message history

After linking, the phone sends recent history. The chat list appears within
seconds, and messages can take a few minutes to finish loading. Zap Fasterk stores
new messages in its own archive. When you scroll past the stored history,
Zap Fasterk asks your phone for older messages. The phone must be online.

## Try it in your own chat

Use WhatsApp's **Message yourself** chat to try messages, reactions, edits,
voice messages, and attachments privately.
