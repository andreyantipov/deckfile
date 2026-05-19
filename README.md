# deckfile

Declarative Stream Deck controller. One YAML file (`deckfile.yaml`)
defines your entire deck — buttons, dials, icons, shell actions, visual
toggle states. No GUI, no plugins, no terminal multiplexer hacks.

Built around [`elgato-streamdeck`](https://crates.io/crates/elgato-streamdeck).
Stream Deck **Plus** (8 keys + 4 dials + LCD touchscreen) is the primary
target; other models work too.

## Install

### Via Nix flake

```nix
# flake.nix
{
  inputs.deckfile.url = "github:andreyantipov/deckfile";

  outputs = { nixpkgs, deckfile, ... }: {
    nixosConfigurations.studio = nixpkgs.lib.nixosSystem {
      modules = [
        deckfile.nixosModules.udev    # TAG=uaccess для 0fd9 (Elgato)
        ({ pkgs, ... }: {
          environment.systemPackages = [
            deckfile.packages.x86_64-linux.default
          ];
        })
      ];
    };
  };
}
```

### Via Cargo

```sh
cargo install deckfile
# + system: hidapi headers, libudev
# + udev rule: SUBSYSTEM=="usb", ATTRS{idVendor}=="0fd9", TAG+="uaccess"
```

## Quick start

1. Write `~/.config/deckfile/deckfile.yaml`:

   ```yaml
   device:
     brightness: 60
     font: /run/current-system/sw/share/fonts/truetype/DejaVuSans-Bold.ttf

   buttons:
     0:
       label: "🎤"
       bg: "#1e4d2b"
       press: "hermes-voice-toggle"
       state_file: "/tmp/hermes-voice.pid"
       label_active: "●"
       bg_active: "#cc2222"

     1:
       label: "WEB"
       press: "xdg-open http://localhost:3000"

   dials:
     0:
       turn_up: "wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+"
       turn_down: "wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-"
       press: "wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"
   ```

2. Run:

   ```sh
   deckfile run
   ```

Or as a systemd-user service via the flake's `homeManagerModules.default`.

## CLI

```
deckfile run [--config PATH]   start the daemon
deckfile validate [PATH]       check syntax without touching hardware
deckfile devices               list connected Stream Decks
```

## Roadmap

- `deckfile mcp` — MCP server so LLM agents can manage your `deckfile.yaml`
  declaratively (Claude / other agents read schema, propose edits, hot-reload)
- Custom `Deckfile` DSL syntax (Dockerfile/Justfile-style) — v1.0
- LCD touchscreen rendering (Plus's 800x100 strip)
- Multi-page support

## License

MIT
