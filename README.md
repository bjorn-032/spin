# SPIN (Simple Package Installer for Nix)

A friendly CLI wrapper around NixOS package management. System packages are tracked in `/etc/nixos/packages.conf`.

## Setup

Add spin's package file to your `configuration.nix` imports:

```nix
imports = [ ./packages.conf ];
```

## Usage

```
spin [FLAGS] [OPTIONS]
```

| Flag/Option | Short | Description |
|---|---|---|
| `--install <pkg>...` | `-i` | Install one or more packages system-wide |
| `--remove <pkg>...` | `-r` | Remove one or more packages |
| `--sync` | `-s` | Update Nix channels |
| `--upgrade` | `-u` | Upgrade all system packages |
| `--profile` | `-p` | Target the current user's Nix profile instead of the system |
| `--temp` | `-t` | Open a temporary `nix shell` with the package (not persisted) |
| `--font` | `-f` | With `-i`/`-r`: install/remove fonts (`fonts.packages`) instead of system packages |
| `--query <pkg>...` | `-q` | Search nixpkgs for one or more packages |
| `--clean` | `-c` | Delete generations older than 7 days and update bootloader |
| `--all` | | With `--clean`: delete ALL old generations instead of just >7d |

Flags can be combined. System operations require sudo.

## Examples

```bash
# Install a package system-wide
spin -i ripgrep

# Install multiple packages and sync channels first
spin -si git htop neovim

# Upgrade all packages
spin -u

# Install to your user profile (no sudo needed)
spin -pi helix

# Try a package without installing it
spin -ti cowsay

# Install fonts (added to fonts.packages so fontconfig picks them up)
spin -fi fira-code noto-fonts

# Remove a font
spin -fr fira-code

# Remove a package
spin -r htop

# Search for a package
spin -q ripgrep

# Clean up old generations (older than 7 days)
spin -c

# Clean up ALL old generations
spin -c --all
```

## How it works

1. `--sync` updates Nix channels (`nix-channel --update`)
2. `--query` queries nixpkgs and prints matching packages with versions and descriptions
3. `--clean` runs `nix-collect-garbage` to delete old generations, then `nixos-rebuild boot` to update the bootloader; `--all` removes all old generations instead of just those older than 7 days
4. Install targets are validated up front with a single fast `nix eval` (an attribute-existence check, not a full `nix search`); on a typo it suggests alternatives and aborts before any rebuild
5. `packages.conf` is updated in one write — system packages go to `environment.systemPackages`, fonts (`-f`) to `fonts.packages`
6. `nixos-rebuild switch` is run once; on failure, `packages.conf` is rolled back

`packages.conf` holds both an `environment.systemPackages` block and a `fonts.packages` block, so a single `./packages.conf` import covers both — no extra setup for fonts.

## Build

```bash
cargo build --release
```
