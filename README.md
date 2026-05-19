# deckfile

Declarative Stream Deck controller. One YAML file (`deckfile.yaml`)
defines your entire deck — buttons, dials, icons, shell actions, visual
state indicators. No GUI, no plugins.

Built around [`elgato-streamdeck`](https://crates.io/crates/elgato-streamdeck).
Stream Deck **Plus** (keys + dials + LCD touchscreen) is the primary
target; other Stream Deck models work too.

## Install

### Nix flake

```nix
{
  inputs.deckfile.url = "github:andreyantipov/deckfile";

  outputs = { nixpkgs, deckfile, ... }: {
    nixosConfigurations.<host> = nixpkgs.lib.nixosSystem {
      modules = [
        deckfile.nixosModules.udev   # uaccess for Elgato 0fd9
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

User-level (home-manager) — autostart via systemd:

```nix
imports = [ inputs.deckfile.homeManagerModules.default ];
```

### Cargo

```sh
cargo install deckfile
# also needed on the system:
# - libudev / libusb (for hidapi)
# - udev rule:
#     SUBSYSTEM=="usb", ATTRS{idVendor}=="0fd9", TAG+="uaccess"
#     KERNEL=="hidraw*", ATTRS{idVendor}=="0fd9", TAG+="uaccess"
```

## Usage

```
deckfile                run the daemon (default action)
deckfile -f PATH        run with explicit deckfile.yaml path
deckfile -d             daemonize (fork + setsid + detach)
deckfile validate       parse-check deckfile.yaml without hardware
deckfile devices        list connected Stream Decks
```

deckfile.yaml lookup order: `$DECKFILE` → `./deckfile.yaml` →
`$XDG_CONFIG_HOME/deckfile/deckfile.yaml`.

## deckfile.yaml

```yaml
device:
  brightness: 60

vars:
  shell: zellij                       # ${shell} below expands to "zellij"

buttons:
  0:
    label: "▶"
    bg: "#1e4d2b"
    on_press: ${shell}
    # Visual state — daemon polls these paths and swaps variants:
    state_file: /tmp/some-task.pid    # exists → *_active variant
    label_active: "■"
    bg_active: "#cc2222"
    processing_file: /tmp/some-task.processing  # exists → *_processing
    label_processing: "…"             # priority: processing > active > idle
    bg_processing: "#cc8800"

  1:
    label: "TERM"
    on_press: foot

dials:
  0:
    on_turn_up:   wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+
    on_turn_down: wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-
    on_press:     wpctl set-mute   @DEFAULT_AUDIO_SINK@ toggle
```

See `deckfile.yaml.example` in the repo for the full schema.

## MCP integration (Claude / other LLM agents)

> **Status: planned for v0.2.** Stub below shows the target shape.

`deckfile mcp` will expose an MCP server so LLM agents can read and
edit your deckfile.yaml conversationally — "put a microphone icon on
key 0 and bind it to my voice script". Once shipped, register it with
Claude Code by adding to `~/.claude.json`:

```json
{
  "mcpServers": {
    "deckfile": {
      "command": "deckfile",
      "args": ["mcp"]
    }
  }
}
```

Tools the server will expose: `read_deckfile`, `set_button`, `set_dial`,
`reload`, `validate`, `list_devices`.

## License

MIT
