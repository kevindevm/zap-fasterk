# Packaging

[`native-packages.yaml`](native-packages.yaml) is the packaging configuration:
it pins the shared CLI and nFPM versions and declares Linux amd64/arm64 inputs,
DEB/RPM/AppImage contents, dependencies, recipe templates and downstream repositories.
Application assets and native recipes stay in `packaging/`.

Windows releases ship the portable zip and a per-user Windows Installer
package, `zap-fasterk-vX.Y.Z-<target>.msi`, built by native-packages from
`packaging/windows/zap-fasterk.wxs` (`packaging/windows/msi.rb`).
Releases up to 0.30.0 also shipped an Inno Setup program (`-setup.exe`);
it is gone, because the package is the one the updater prefers and the one
tested. The MSI uses the fork's own upgrade identity and only takes over an
Inno installation registered under the fork's AppId. It installs into that
same folder, removes Inno's uninstaller and its Settings > Apps entry, and
leaves the user's data (session, archive, settings) alone, as they live
outside the program folder. It preserves the profile/keyring and refuses
changes while Zap Fasterk is running. The updater verifies the selected
release package before installation. `packaging/windows/zap-fasterk.iss`
stays only so `test-installer.ps1` can make an Inno install for the package
to take over. Building MSI packages needs WiX 5.0.2 and
`WixToolset.Util.wixext`; the disposable Windows CI runner exercises
installation, takeover, update and removal. Never run
`packaging/windows/test-installer.ps1` against a personal Windows installation;
it refuses non-GitHub-runner invocation and changes the runner's installation.

Version 0.20.0 introduces the Zap Fasterk fork and `zap-fasterk` binary. The GitHub
repository is `kevindevm/zap-fasterk`, so source archives extract into
`zap-fasterk-VERSION`. Recipes belong to this fork and do not replace upstream
packages automatically. AUR and Homebrew publication destinations are not configured.

```sh
gem install native-packages --version 0.9.0
native-packages validate
native-packages doctor --target linux-amd64 --target linux-arm64
native-packages build --release v1.2.3 --target linux-amd64 --target linux-arm64
```

Replace `v1.2.3` with an existing stable application release. Local use also
requires nFPM 2.47.0, `bsdtar`, `readelf`, Python 3 and, for the AppImage,
`mksquashfs` (squashfs-tools); AUR generation needs `makepkg`
or Docker. CI installs its tooling. To package local release archives, put
every configured input and recipe asset under `dist/`, then run
`native-packages build --version 1.2.3 --target linux-amd64 --target linux-arm64`. Outputs go to
`dist/packages/1.2.3`; use `--output` for a fresh destination when rebuilding.

Stable tags run the existing native build jobs first. After binaries and
`checksums.txt` are published, the shared workflow verifies their hashes,
builds the configured packages, and attaches them to the GitHub release.
Configured recipes are attached as an archive. Package checksums are separate
from the original binary checksums. PR validation never publishes.

Linux package finalization rejects inputs requiring glibc newer than 2.35,
matching the Ubuntu 22.04 build container. Older published binaries built on
Ubuntu 24.04 must be rebuilt before using these lower dependency declarations.
The AppImage hook writes passive stable-release update information into the
runtime's reserved ELF section before package checksums are recorded. It keeps
the appended filesystem and offsets intact. Packaging CI generates an
architecture-specific `.zsync` sidecar and separate sidecar checksums for external
AppImage updaters. This metadata does not enable automatic updates or change
Zap Fasterk's built-in signature verification. Local package builds need the CI
sidecar step before external update metadata can resolve successfully.

Review or publish an existing build with the same installed CLI:

```sh
native-packages publish --from dist/packages/1.2.3 --to github
native-packages repositories
native-packages status --offline
```

For applications with configured AUR or Homebrew destinations, stage the
recipes with `native-packages stage TARGET dist/packages/1.2.3/recipes`,
inspect `native-packages diff TARGET`, run native package validation, and
publish with `native-packages publish TARGET`. These destinations use ignored
managed Git clones, recorded in this application's YAML configuration.
AUR automation needs `PUBLISH_AUR=true`, `AUR_SSH_KEY` and `AUR_KNOWN_HOSTS`;
Homebrew automation needs `PUBLISH_HOMEBREW=true` and
`HOMEBREW_TAP_GITHUB_TOKEN`. Enable only configured destinations.

The native macOS configuration, Windows and Flatpak build steps remain responsible
for their native artifacts. Additional nFPM formats require suitable platform
inputs and dependencies; adding a format does not port the application.
See the [shared CLI documentation](https://github.com/crmne/native-packages/tree/v0.9.0)
for commands and supported formats.

To upgrade the tool, change `tool.version` in `native-packages.yaml`, the matching immutable workflow reference, and any release-job gem installation
pin together. Applications need no packaging Gemfile, lockfile or Ruby wrapper.

## Automatic macOS notarization

`packaging/macos/entitlements.plist` grants microphone access under the hardened
runtime, and `Info.plist` supplies the permission prompt. `bundle.sh` embeds the
entitlement in its initial signature so native-packages preserves it when signing
with Developer ID. After notarization, `verify.sh` mounts the final DMG and checks
its ticket, Gatekeeper acceptance, both architectures, and microphone metadata.

The macOS release job builds the app first, then uses
`native-packages.yaml` and `packaging/macos/dmg.rb` to package it.
The shared gem signs its owned input copy, notarizes the DMG, staples and validates
Apple's ticket, and only then records final checksums. Configure these repository
secrets, which the job exposes as environment variables:

- `APPLE_CERTIFICATE_P12`: base64 PKCS#12 Developer ID Application certificate and private key.
- `APPLE_CERTIFICATE_PASSWORD`: the export password.
- `APPLE_SIGNING_IDENTITY`: exact `Developer ID Application: Name (TEAMID)` identity.
- `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD`: Apple email, Team ID and app-specific password.

A complete set enables notarization automatically. An incomplete set fails;
no values retain local builds without Developer ID signing. Application inputs
and the user's normal keychains remain unchanged. See the shared
[Apple setup and phase contract](https://github.com/crmne/native-packages/blob/v0.9.0/docs/apple-notarization.md).

After preparing `dist/macos-input` on a Mac, test packaging without publishing:

```sh
native-packages build \
  --version 1.2.3 --target macos-universal --defer-recipes --output dist/macos-packages-test
```

Secret configuration applies to future builds. Existing published DMGs retain
their original signatures; this setup does not replace release assets.

Linux releases build inside Ubuntu 22.04 containers (glibc 2.35), including
the native ARM runner, so Debian 12's glibc 2.36 can run them. DEB/RPM recipes declare
runtime-loaded Wayland, X11 and EGL libraries as well as ALSA and its PulseAudio
plugin. Packaging CI builds both architectures from an explicitly supplied published
fork version; release runs use their own tag. Source-only PRs run the user-installer
checks. Clean Ubuntu, Debian and Fedora containers
install and remove each package, check GUI libraries loaded with `dlopen`, and
verify desktop and theme assets. Run the same check locally with
`bash packaging/test-install.sh ubuntu:24.04 /path/to/native-packages-output`. The macOS job selects `macos-universal`
from the same configuration with `--defer-recipes`, leaving Linux inputs and AUR
recipe generation to the Linux packaging job after release assets exist.

## Flatpak

`packaging/flatpak/io.github.kevindevm.ZapFasterk.yml` builds from source, with offline Cargo
sources generated from the selected revision's lockfile. The adjacent bundle
manifest reuses the Linux release binary, as in Spotifast. Both grant Wayland/X11,
GPU, audio, network, keyring and tray access; attachments chosen by the user use
portals. No home-directory permission is granted. `--persist=.local/state` keeps
the archive and session on Flatpak versions without `XDG_STATE_HOME`.

Generate a pinned Flathub checkout (Python needs `aiohttp`, `tomlkit` and `PyYAML`):

```sh
packaging/flatpak/flathub.sh vX.Y.Z /path/to/flathub-checkout
flatpak-builder --user --install --force-clean build-dir /path/to/flathub-checkout/io.github.kevindevm.ZapFasterk.yml
```

Flathub submission/review is a separate publication step; the manifest alone does
not make Zap Fasterk available in Flathub. A maintainer must submit it manually:
[Flathub's requirements](https://docs.flathub.org/docs/for-app-authors/requirements#generative-ai-policy)
prohibit AI agents from submitting or writing submission interactions and require
disclosure of generated material. Review the manifests and these changes before
submitting. The manifests use the current Freedesktop 26.08 runtime; the CI builder
container is 25.08 and installs the runtime and SDK named by the manifest. The GitHub release job includes the bundle
in `checksums.txt`. No existing release files are replaced by this change.
