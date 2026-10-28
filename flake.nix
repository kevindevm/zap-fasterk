{
  description = "Fast native WhatsApp client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # rust-toolchain.toml pins the compiler so local builds and CI agree.
    # This reads that file rather than restating the version here.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      systems = [
        "aarch64-linux"
        "x86_64-linux"
        # Nixpkgs unstable no longer supports x86_64-darwin.
        "aarch64-darwin"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            }
          )
        );
    in
    {
      devShells = forAllSystems (
        pkgs:
        let
          inherit (pkgs.stdenv.hostPlatform) isLinux;
        in
        {
          default = pkgs.mkShell (
            {
              packages =
                with pkgs;
                [
                  (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
                  rust-analyzer
                  pkg-config
                  cmake
                  perl
                  llvmPackages.libclang
                  rustPlatform.bindgenHook
                ]
                # On macOS the app links the system frameworks instead.
                ++ lib.optionals isLinux [
                  alsa-lib
                  libxkbcommon
                  wayland
                  libGL
                  libx11
                  libxcursor
                  libxi
                  libxrandr
                ];
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              ZAPFAST_TEST_RTL_FONT = "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans.ttf";
            }
            // pkgs.lib.optionalAttrs isLinux {
              # The GUI dlopens its Wayland, X11 and GL libraries at run time.
              LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (
                with pkgs;
                [
                  libxkbcommon
                  wayland
                  libGL
                  libx11
                  libxcursor
                  libxi
                  libxrandr
                ]
              );
            }
          );
        }
      );

      packages = forAllSystems (
        pkgs:
        let
          inherit (pkgs.stdenv.hostPlatform) isLinux isDarwin;
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
          runtimeLibs = with pkgs; [
            libxkbcommon
            wayland
            libGL
            libx11
            libxcursor
            libxi
            libxrandr
          ];
          zap-fasterk = rustPlatform.buildRustPackage rec {
            pname = "zap-fasterk";
            version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
            src = self;

            # Import registry dependencies directly from Cargo.lock so ordinary
            # lock-file updates do not require refreshing a vendor hash. Git
            # dependencies remain pinned to their exact locked revisions.
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };

            nativeBuildInputs =
              with pkgs;
              [
                llvmPackages.libclang
                pkg-config
                cmake
                perl
                makeWrapper
                rustPlatform.bindgenHook
              ]
              # Sandboxed stand-ins for iconutil and codesign in bundle.sh.
              ++ lib.optionals isDarwin [
                icnsify
                rcodesign
              ];
            # On macOS the app links the system frameworks instead.
            buildInputs = pkgs.lib.optionals isLinux (
              with pkgs;
              [
                alsa-lib
                libGL
                libx11
              ]
            );
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            ZAPFAST_TEST_RTL_FONT = "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans.ttf";

            postFixup =
              # The GUI dlopens its Wayland, X11 and GL libraries at run time.
              pkgs.lib.optionalString isLinux ''
                wrapProgram $out/bin/zap-fasterk \
                  --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath runtimeLibs}
              ''
              # Sign after the strip hook, which would invalidate the signature.
              # The entitlement allows microphone access for voice messages.
              + pkgs.lib.optionalString isDarwin ''
                rcodesign sign \
                  --entitlements-xml-file packaging/macos/entitlements.plist \
                  "$out/Applications/Zap Fasterk.app"
              '';

            postInstall =
              pkgs.lib.optionalString isLinux ''
                install -Dm644 packaging/applications/zap-fasterk.desktop \
                  $out/share/applications/zap-fasterk.desktop
                install -Dm644 packaging/icons/zap-fasterk.svg \
                  $out/share/icons/hicolor/scalable/apps/zap-fasterk.svg
                install -Dm644 contrib/omarchy/zap-fasterk.json.tpl \
                  $out/share/zap-fasterk/omarchy/zap-fasterk.json.tpl
                install -Dm755 contrib/omarchy/zap-fasterk-theme \
                  $out/share/zap-fasterk/omarchy/zap-fasterk-theme
              ''
              # Mirrors packaging/macos/bundle.sh: Zap Fasterk.app around the built
              # binary, with the version stripped of any prerelease suffix.
              + pkgs.lib.optionalString isDarwin ''
                app="$out/Applications/Zap Fasterk.app"
                mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
                mv "$out/bin/zap-fasterk" "$app/Contents/MacOS/zap-fasterk"
                chmod 755 "$app/Contents/MacOS/zap-fasterk"
                # The command runs the bundled executable, so macOS applies the
                # bundle's Info.plist and entitlements, and start at login
                # records the bundled path.
                makeWrapper "$app/Contents/MacOS/zap-fasterk" "$out/bin/zap-fasterk"
                sed "s/__VERSION__/''${version%%-*}/g" packaging/macos/Info.plist \
                  > "$app/Contents/Info.plist"
                icnsify packaging/macos/icon-1024.png \
                  --output "$app/Contents/Resources/zap-fasterk.icns"
              '';

            meta = {
              description = "Fast native WhatsApp client";
              homepage = "https://github.com/kevindevm/zap-fasterk";
              license = with pkgs.lib.licenses; [ mit gpl2Only ];
              mainProgram = "zap-fasterk";
              # The flake publishes only aarch64-darwin. Older nixpkgs, for
              # example through follows, still list x86_64-darwin.
              platforms = pkgs.lib.platforms.linux ++ [ "aarch64-darwin" ];
            };
          };
        in
        {
          default = zap-fasterk;
          inherit zap-fasterk;
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
