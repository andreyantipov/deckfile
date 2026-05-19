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
            # First build pin'нет Cargo.lock — пока его нет, выставляем
            # allowBuiltinFetchGit чтобы git-deps elgato-streamdeck (если
            # будут) подтянулись. После первого `cargo build` коммитим
            # Cargo.lock и убираем эту опцию.
            allowBuiltinFetchGit = true;
          };
          nativeBuildInputs = with pkgs; [
            pkg-config
            rust
          ];
          buildInputs = with pkgs; [
            hidapi
            libusb1
            udev
            # DejaVu для дефолтного font fallback в runtime через env.
            dejavu_fonts
          ];

          # Бинарь зовёт hidapi/libudev в runtime — ELF deps уже подхвачены
          # autoPatchelf через nix-build, но fontconfig для рендеринга
          # лейблов хорошо иметь в env.
          postInstall = ''
            wrapProgram $out/bin/deckfile \
              --set-default DECKFILE_FONT \
                "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans-Bold.ttf"
          '';
          nativeBuildInputs2 = [ pkgs.makeWrapper ];

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
            dejavu_fonts
          ];
          shellHook = ''
            export DECKFILE_FONT="${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans-Bold.ttf"
          '';
        };
      }) // {

      # System-level udev — даёт юзеру доступ к Elgato устройствам без
      # отдельной группы (через uaccess ACL). Подключается параллельно
      # с packages — это nixosModule, не пакет.
      nixosModules.udev = { ... }: {
        services.udev.extraRules = ''
          SUBSYSTEM=="usb", ATTRS{idVendor}=="0fd9", TAG+="uaccess"
          KERNEL=="hidraw*", ATTRS{idVendor}=="0fd9", TAG+="uaccess"
        '';
      };

      # User-level home-manager module: пакет + systemd-user-service.
      # Conf-файл deckfile.yaml юзер пишет руками (или потом MCP-агент).
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
              ExecStart = "${pkg}/bin/deckfile run";
              Restart = "on-failure";
              RestartSec = 10;
              Environment = [ "RUST_LOG=deckfile=info" ];
            };
            Install.WantedBy = [ "graphical-session.target" ];
          };
        };
    };
}
