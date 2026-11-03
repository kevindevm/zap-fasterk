# Packaging

[`native-packages.yaml`](native-packages.yaml) is the packaging configuration:
it pins the shared CLI and declares the Windows amd64/arm64 inputs.
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
packages automatically.

```sh
gem install native-packages --version 0.9.0
native-packages validate
native-packages doctor --target windows-amd64 --target windows-arm64
native-packages build --release v1.2.3 --target windows-amd64 --target windows-arm64
```

Replace `v1.2.3` with an existing stable application release.

Stable tags run the existing native build jobs first. After binaries and
`checksums.txt` are published, the shared workflow verifies their hashes,
builds the configured packages, and attaches them to the GitHub release.
PR validation never publishes.

Review or publish an existing build with the same installed CLI:

```sh
native-packages publish --from dist/packages/1.2.3 --to github
native-packages repositories
native-packages status --offline
```

To upgrade the tool, change `tool.version` in `native-packages.yaml`, the matching immutable workflow reference, and any release-job gem installation
pin together. Applications need no packaging Gemfile, lockfile or Ruby wrapper.
