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
4. All install targets are validated with a single `nix search` query; suggests alternatives if a name is not found
5. `packages.conf` is updated
6. `nixos-rebuild switch` is run once; on failure, `packages.conf` is rolled back

## Build

```bash
cargo build --release
```
