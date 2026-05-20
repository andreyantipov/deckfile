# deckfile flake — Rust app + udev rule + home-manager module.
#
# Consume from other flakes:
#   inputs.deckfile.url = "github:andreyantipov/deckfile";
#   modules = [
#     inputs.deckfile.nixosModules.udev
#     ({ pkgs, ... }: {
#       environment.systemPackages = [ inputs.deckfile.packages.x86_64-linux.default ];
#     })
#   ];

{
  description = "deckfile — declarative Stream Deck controller";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rust = pkgs.rust-bin.stable.latest.default;

        deckfile = pkgs.rustPlatform.buildRustPackage {
          pname = "deckfile";
          version = "0.1.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            # Allow git deps to be fetched during the initial pin until
            # Cargo.lock is committed.
            allowBuiltinFetchGit = true;
          };
          nativeBuildInputs = with pkgs; [
            pkg-config
            rust
            makeWrapper
          ];
          buildInputs = with pkgs; [
            hidapi
            libusb1
            udev
            # Slint's software renderer with `systemfonts` enabled
            # calls into fontconfig at runtime for glyph lookup; the
            # `-sys` crate links against the system library at build
            # time, so we need fontconfig (and freetype, pulled in
            # transitively but listed for clarity) on the path.
            fontconfig
            freetype
            # DejaVu provides a fallback path for plain alphabetic labels.
            # Icons themselves come from the lucide-icons Rust crate
            # (font bytes embedded in the binary via include_bytes!).
            dejavu_fonts
          ];

          postInstall = ''
            wrapProgram $out/bin/deckfile \
              --set-default DECKFILE_FONT \
                "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans-Bold.ttf"
          '';

          meta = with pkgs.lib; {
            description = "Declarative Stream Deck controller";
            homepage = "https://github.com/andreyantipov/deckfile";
            license = licenses.mit;
            mainProgram = "deckfile";
            platforms = platforms.linux;
          };
        };

      in
      {
        packages.default = deckfile;
        packages.deckfile = deckfile;

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rust
            pkg-config
            hidapi
            libusb1
            udev
            fontconfig
            freetype
            dejavu_fonts
          ];
          shellHook = ''
            export DECKFILE_FONT="${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans-Bold.ttf"
          '';
        };
      }) // {

      # System-level udev rule — grants the active local user access to
      # Elgato USB devices through uaccess ACL, no extra group required.
      nixosModules.udev = { ... }: {
        services.udev.extraRules = ''
          SUBSYSTEM=="usb", ATTRS{idVendor}=="0fd9", TAG+="uaccess"
          KERNEL=="hidraw*", ATTRS{idVendor}=="0fd9", TAG+="uaccess"
        '';
      };

      # User-level home-manager module: ships the binary and a
      # systemd-user-service that runs the daemon at session start.
      # The deckfile.yaml itself is owned by the user; this module
      # does not seed it.
      homeManagerModules.default = { config, lib, pkgs, ... }:
        let
          system = pkgs.stdenv.hostPlatform.system;
          pkg = self.packages.${system}.default;
        in {
          home.packages = [ pkg ];

          systemd.user.services.deckfile = {
            Unit = {
              Description = "deckfile — declarative Stream Deck controller";
              After = [ "graphical-session.target" ];
              PartOf = [ "graphical-session.target" ];
            };
            Service = {
              Type = "simple";
              ExecStart = "${pkg}/bin/deckfile";
              Restart = "on-failure";
              RestartSec = 10;
              Environment = [ "RUST_LOG=deckfile=info" ];
            };
            Install.WantedBy = [ "graphical-session.target" ];
          };
        };
    };
}
