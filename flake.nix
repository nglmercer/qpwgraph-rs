{
  description = "Rust/Slint PipeWire patchbay with optional ALSA MIDI support";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems = nixpkgs.lib.genAttrs systems;
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      version = cargoToml.workspace.package.version;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          inherit (pkgs) lib;
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "qpwgraph-rs";
            inherit version;
            src = ./.;

            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "--package" "pw-graph-app" ];
            cargoTestFlags = [ "--workspace" "--all-features" ];

            nativeBuildInputs = with pkgs; [
              cmake
              llvmPackages.libclang
              pkg-config
            ];

            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            buildInputs = with pkgs; [
              alsa-lib
              gtk3
              libxkbcommon
              opus
              pipewire
              wayland
              xorg.libX11
              xorg.libXcursor
              xorg.libXi
              xorg.libXinerama
              xorg.libXrandr
            ];

            installPhase = ''
              runHook preInstall
              install -Dm755 \
                target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/release/qpwgraph-rs \
                $out/bin/qpwgraph-rs
              install -Dm644 packaging/io.github.nglmercer.qpwgraph-rs.desktop \
                $out/share/applications/io.github.nglmercer.qpwgraph-rs.desktop
              install -Dm644 packaging/io.github.nglmercer.qpwgraph-rs.metainfo.xml \
                $out/share/metainfo/io.github.nglmercer.qpwgraph-rs.metainfo.xml
              install -Dm644 packaging/io.github.nglmercer.qpwgraph-rs.svg \
                $out/share/icons/hicolor/scalable/apps/io.github.nglmercer.qpwgraph-rs.svg
              runHook postInstall
            '';

            meta = {
              homepage = "https://github.com/nglmercer/qpwgraph-rs";
              description = "Visual PipeWire and ALSA MIDI connection manager";
              license = lib.licenses.gpl3Plus;
              mainProgram = "qpwgraph-rs";
              platforms = lib.platforms.linux;
            };
          };
        });

      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              cmake
              llvmPackages.libclang
              rustc
              rustfmt
            ];

            nativeBuildInputs = with pkgs; [ pkg-config ];

            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            buildInputs = with pkgs; [
              alsa-lib
              gtk3
              libxkbcommon
              opus
              pipewire
              wayland
              xorg.libX11
              xorg.libXcursor
              xorg.libXi
              xorg.libXinerama
              xorg.libXrandr
            ];
          };
        });

      checks = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = self.packages.${system}.default;

          format = pkgs.runCommand "qpwgraph-rs-format-check" {
            nativeBuildInputs = [ pkgs.rustfmt ];
          } ''
            cp -r ${./.} source
            chmod -R u+w source
            cd source
            cargo fmt --all -- --check
            touch $out
          '';
        });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/qpwgraph-rs";
        };
      });
    };
}
