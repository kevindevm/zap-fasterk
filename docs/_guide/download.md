---
title: Download Zap Faster
---

Download **[Zap Faster v0.30.0](https://github.com/eternalwaitt/zap-faster/releases/tag/v0.30.0)** for Windows, Linux and macOS.

[Windows](https://github.com/eternalwaitt/zap-faster/releases/download/v0.30.0/zap-faster-v0.30.0-x86_64-pc-windows-msvc.msi) · [Windows ARM](https://github.com/eternalwaitt/zap-faster/releases/download/v0.30.0/zap-faster-v0.30.0-aarch64-pc-windows-msvc.msi) · [Mac](https://github.com/eternalwaitt/zap-faster/releases/download/v0.30.0/zap-faster-v0.30.0-macos-universal.dmg) · [Linux](https://github.com/eternalwaitt/zap-faster/releases/download/v0.30.0/zap-faster-v0.30.0-x86_64-unknown-linux-gnu.tar.gz) · [Linux ARM](https://github.com/eternalwaitt/zap-faster/releases/download/v0.30.0/zap-faster-v0.30.0-aarch64-unknown-linux-gnu.tar.gz) · [Flatpak](https://github.com/eternalwaitt/zap-faster/releases/download/v0.30.0/zap-faster-v0.30.0-x86_64.flatpak)

The Windows installer is a per-user MSI package and needs no administrator rights. If you installed with the older setup program (`-setup.exe`), run the `.msi` once: it installs into the same folder, takes over the old Settings > Apps entry, and leaves your chats and settings alone.

The release also includes DEB, RPM and AppImage packages for both Linux architectures. Portable Windows archives, checksums, the signed update manifest and package recipes are on the same release page.

ZapFast's website, Homebrew tap and AUR packages install the upstream project. Zap Faster packages have their own filenames and updater signature key. There is no separately published Zap Faster Homebrew tap, AUR entry or Flathub listing yet.

Quit ZapFast before launching Zap Faster. Both use the existing archive, keyring entries and single-instance guard. Only one can run against that data at a time.

Windows installers are not Authenticode signed. macOS builds are ad-hoc signed and not Apple notarized unless the release explicitly states otherwise. These platform signatures are separate from Zap Faster's signed update manifest.
