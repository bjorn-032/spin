use clap::{Arg, ArgAction, Command as ClapCommand};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::{mpsc, OnceLock};
use std::time::Instant;

const PACKAGES_FILE: &str = "/etc/nixos/packages.conf";

// Move value-taking short flags (i, r) to the end of combined groups like -ip → -pi,
// so clap doesn't consume the following flag chars as the value.
fn reorder_args(args: Vec<String>) -> Vec<String> {
    const VALUE_FLAGS: &[char] = &['i', 'r'];
    args.into_iter().map(|arg| {
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            let chars: Vec<char> = arg[1..].chars().collect();
            if chars.iter().any(|c| VALUE_FLAGS.contains(c)) {
                let mut non_val: Vec<char> = chars.iter().copied().filter(|c| !VALUE_FLAGS.contains(c)).collect();
                let val: Vec<char> = chars.iter().copied().filter(|c| VALUE_FLAGS.contains(c)).collect();
                non_val.extend(val);
                return format!("-{}", non_val.iter().collect::<String>());
            }
        }
        arg
    }).collect()
}

fn main() -> ExitCode {
    let matches = build_cli().get_matches_from(reorder_args(std::env::args().collect::<Vec<_>>()));

    let is_profile = matches.get_flag("profile");
    let is_temp = matches.get_flag("temp");
    let is_font = matches.get_flag("font");
    let do_sync = matches.get_flag("sync");
    let do_upgrade = matches.get_flag("upgrade");
    let do_clean = matches.get_flag("clean");
    let clean_all = matches.get_flag("all");
    let install_pkgs: Vec<String> = matches.get_many::<String>("install").unwrap_or_default().cloned().collect();
    let remove_pkgs: Vec<String>  = matches.get_many::<String>("remove").unwrap_or_default().cloned().collect();
    let search_pkgs: Vec<String>  = matches.get_many::<String>("search").unwrap_or_default().cloned().collect();

    // 1. Sync channels first, bail early on failure.
    if do_sync {
        println!("[spin] Syncing package list (updating Nix channels)...");
        if !run_privileged("nix-channel", &["--update"]) {
            return ExitCode::FAILURE;
        }
    }

    // 2. Handle non-system operations: search, clean, temp shell, user profile.
    if !search_pkgs.is_empty() {
        cmd_search(&search_pkgs);
        return ExitCode::SUCCESS;
    }
    if do_clean {
        return if cmd_clean(clean_all) { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    if is_temp {
        if !validate_packages(&install_pkgs) {
            return ExitCode::FAILURE;
        }
        let nix_args: Vec<String> = ["shell", "--impure"].iter().map(|s| s.to_string())
            .chain(install_pkgs.iter().map(|p| format!("nixpkgs#{}", p)))
            .collect();
        let nix_args_ref: Vec<&str> = nix_args.iter().map(String::as_str).collect();
        println!("[spin] Opening temporary shell with {}...", install_pkgs.join(", "));
        return if run_nix(&nix_args_ref) { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    if is_profile {
        if !install_pkgs.is_empty() {
            if !validate_packages(&install_pkgs) {
                return ExitCode::FAILURE;
            }
            println!("[spin] Installing {} to user profile...", install_pkgs.join(", "));
            // One invocation for every package — a single flake lock/eval.
            let mut nix_args: Vec<String> =
                ["profile", "install", "--impure"].iter().map(|s| s.to_string()).collect();
            nix_args.extend(install_pkgs.iter().map(|p| format!("nixpkgs#{}", p)));
            let nix_args_ref: Vec<&str> = nix_args.iter().map(String::as_str).collect();
            if !run_nix_streaming(&nix_args_ref) {
                return ExitCode::FAILURE;
            }
        }
        for pkg in &remove_pkgs {
            if let Err(e) = remove_profile(pkg) {
                eprintln!("[spin] Error: {}", e);
                return ExitCode::FAILURE;
            }
        }
        if !install_pkgs.is_empty() || !remove_pkgs.is_empty() {
            return ExitCode::SUCCESS;
        }
    }

    // 3. Prepare system changes: update packages.conf in a single read/write,
    //    no rebuild yet. `rollback` holds the pre-change lists if the file was
    //    written, so a failed rebuild can restore it.
    let kind = if is_font { "Font" } else { "Package" };
    let mut added: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut rollback: Option<(Vec<String>, Vec<String>)> = None;

    if !install_pkgs.is_empty() || !remove_pkgs.is_empty() {
        if let Err(e) = ensure_packages_file() {
            eprintln!("[spin] Error: {}", e);
            return ExitCode::FAILURE;
        }
        let (mut packages, mut fonts) = match read_conf() {
            Ok(conf) => conf,
            Err(e) => {
                eprintln!("[spin] Error: {}", e);
                return ExitCode::FAILURE;
            }
        };
        let snapshot = (packages.clone(), fonts.clone());

        {
            // `-f` targets the fonts.packages list, otherwise systemPackages.
            let target = if is_font { &mut fonts } else { &mut packages };

            // Only names not already present need adding (and validating).
            let mut new_names: Vec<String> = Vec::new();
            for pkg in &install_pkgs {
                if target.contains(pkg) {
                    println!("[spin] {} '{}' is already installed.", kind, pkg);
                } else if !new_names.contains(pkg) {
                    new_names.push(pkg.clone());
                }
            }

            if !new_names.is_empty() && !validate_packages(&new_names) {
                return ExitCode::FAILURE;
            }

            for pkg in new_names {
                println!("[spin] Queued '{}' for installation.", pkg);
                target.push(pkg.clone());
                added.push(pkg);
            }
            target.sort();

            for pkg in &remove_pkgs {
                if target.contains(pkg) {
                    target.retain(|p| p != pkg);
                    println!("[spin] Queued '{}' for removal.", pkg);
                    removed.push(pkg.clone());
                } else {
                    eprintln!(
                        "[spin] Error: {} '{}' is not managed by spin (not in {}).",
                        kind.to_lowercase(), pkg, PACKAGES_FILE
                    );
                    return ExitCode::FAILURE;
                }
            }
        }

        // Single write covering every requested change.
        if !added.is_empty() || !removed.is_empty() {
            if let Err(e) = write_conf(&packages, &fonts) {
                eprintln!("[spin] Error: {}", e);
                return ExitCode::FAILURE;
            }
            rollback = Some(snapshot);
        }
    }

    // 4. Single rebuild — use --upgrade when that flag was given.
    let needs_rebuild = do_upgrade || rollback.is_some();
    if needs_rebuild {
        let flags: &[&str] = if do_upgrade { &["switch", "--upgrade"] } else { &["switch"] };
        if !run_privileged_streaming("nixos-rebuild", flags) {
            // Restore packages.conf to its pre-change state.
            if let Some((p, f)) = rollback {
                let _ = write_conf(&p, &f);
            }
            eprintln!("[spin] Error: nixos-rebuild failed — changes rolled back.");
            return ExitCode::FAILURE;
        }
    }

    for pkg in &added   { println!("[spin] {} '{}' installed successfully.", kind, pkg); }
    for pkg in &removed { println!("[spin] {} '{}' removed successfully.", kind, pkg); }
    if do_upgrade && added.is_empty() && removed.is_empty() {
        println!("[spin] System updated successfully.");
    }

    ExitCode::SUCCESS
}

fn build_cli() -> ClapCommand {
    ClapCommand::new("spin")
        .bin_name("spin")
        .about("spin - Simple Package Installer for Nix")
        .long_about(
            "SPIN (Simple Package Installer for Nix) is a friendly wrapper around NixOS package management.\n\
             System packages are tracked in /etc/nixos/packages.conf.\n\n\
             NOTE: Add the following line to the imports list in /etc/nixos/configuration.nix\n\
             to activate spin-managed packages:\n\
             \n  imports = [ ./packages.conf ];\n\n\
             System operations (install, remove, upgrade, sync) require root/sudo.",
        )
        .version("0.1.0")
        .arg_required_else_help(true)
        .arg(
            Arg::new("install")
                .short('i')
                .long("install")
                .value_name("PACKAGE")
                .num_args(1..)
                .help("Install one or more packages (system-wide by default)"),
        )
        .arg(
            Arg::new("remove")
                .short('r')
                .long("remove")
                .value_name("PACKAGE")
                .num_args(1..)
                .help("Remove one or more packages"),
        )
        .arg(
            Arg::new("sync")
                .short('s')
                .long("sync")
                .action(ArgAction::SetTrue)
                .help("Update the Nix channel package list"),
        )
        .arg(
            Arg::new("upgrade")
                .short('u')
                .long("upgrade")
                .visible_alias("update")
                .action(ArgAction::SetTrue)
                .help("Upgrade all system packages (nixos-rebuild switch --upgrade)"),
        )
        .arg(
            Arg::new("profile")
                .short('p')
                .long("profile")
                .action(ArgAction::SetTrue)
                .help("With -i/-r: target the current user's Nix profile instead of the system"),
        )
        .arg(
            Arg::new("temp")
                .short('t')
                .long("temp")
                .action(ArgAction::SetTrue)
                .help("With -i: start a temporary nix shell with the package (not persisted)"),
        )
        .arg(
            Arg::new("font")
                .short('f')
                .long("font")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["temp", "profile"])
                .help("With -i/-r: install/remove fonts (fonts.packages) instead of system packages"),
        )
        .arg(
            Arg::new("search")
                .short('q')
                .long("query")
                .value_name("PACKAGE")
                .num_args(1..)
                .help("Search nixpkgs for one or more packages"),
        )
        .arg(
            Arg::new("clean")
                .short('c')
                .long("clean")
                .action(ArgAction::SetTrue)
                .help("Delete generations older than 7 days and update bootloader"),
        )
        .arg(
            Arg::new("all")
                .long("all")
                .action(ArgAction::SetTrue)
                .help("With --clean: delete ALL old generations instead of just >7d"),
        )
}

// ── Privilege helpers ─────────────────────────────────────────────────────────

fn is_root() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        Command::new("id")
            .arg("-u")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
            .unwrap_or(false)
    })
}

fn run_command(program: &str, args: &[&str]) -> bool {
    match Command::new(program).args(args).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("[spin] '{}' exited with {}", program, status);
            false
        }
        Err(e) => {
            eprintln!("[spin] Failed to run '{}': {}", program, e);
            false
        }
    }
}

fn run_privileged(program: &str, args: &[&str]) -> bool {
    if is_root() {
        run_command(program, args)
    } else {
        let mut sudo_args = vec![program];
        sudo_args.extend_from_slice(args);
        run_command("sudo", &sudo_args)
    }
}

fn write_privileged(path: &str, content: &str) -> Result<(), String> {
    let tee_cmd = if is_root() { "tee" } else { "sudo tee" };
    let mut child = Command::new("sh")
        .args(["-c", &format!("{} {} > /dev/null", tee_cmd, path)])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Cannot write {}: {}", path, e))?;

    child
        .stdin
        .take()
        .unwrap()
        .write_all(content.as_bytes())
        .map_err(|e| format!("Cannot write {}: {}", path, e))?;

    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Cannot write {} (permission denied?)", path))
    }
}

fn run_streaming(program: &str, args: &[&str]) -> bool {
    run_streaming_env(program, args, &[])
}

fn run_streaming_env(program: &str, args: &[&str], env: &[(&str, &str)]) -> bool {
    const WINDOW_SIZE: usize = 5;
    const MAX_WIDTH: usize = 120;
    const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in env { cmd.env(k, v); }
    let mut child = match cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[spin] Failed to run '{}': {}", program, e);
            return false;
        }
    };

    let (tx, rx) = mpsc::channel::<String>();

    let stdout = child.stdout.take().unwrap();
    let tx1 = tx.clone();
    std::thread::spawn(move || {
        BufReader::new(stdout)
            .lines()
            .flatten()
            .for_each(|l| { let _ = tx1.send(l); });
    });

    let stderr = child.stderr.take().unwrap();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        BufReader::new(stderr)
            .lines()
            .flatten()
            .for_each(|l| { let _ = tx2.send(l); });
    });
    drop(tx);

    let start = Instant::now();
    let mut window: VecDeque<String> = VecDeque::with_capacity(WINDOW_SIZE);
    let mut diagnostics: Vec<String> = Vec::new();
    let mut spin_idx: usize = 0;
    let mut rows: usize = 0;

    for line in rx.iter() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("warning:") || trimmed.starts_with("error:") || trimmed.starts_with("trace:") {
            diagnostics.push(line.clone());
            continue;
        }

        if rows > 0 {
            print!("\x1B[{}A\x1B[J", rows);
            let _ = std::io::stdout().flush();
        }

        let secs = start.elapsed().as_secs();
        println!(
            "{} building... ({:02}:{:02})",
            SPINNER[spin_idx % SPINNER.len()],
            secs / 60,
            secs % 60,
        );
        spin_idx += 1;

        window.push_back(line);
        if window.len() > WINDOW_SIZE {
            window.pop_front();
        }

        rows = 1 + window.len();
        for l in &window {
            let truncated: String = l.chars().take(MAX_WIDTH).collect();
            println!("  \x1B[2m{}\x1B[0m", truncated);
        }
    }

    if rows > 0 {
        print!("\x1B[{}A\x1B[J", rows);
        let _ = std::io::stdout().flush();
    }

    for line in &diagnostics {
        let trimmed = line.trim_start();
        if trimmed.starts_with("error:") {
            eprintln!("\x1B[31m{}\x1B[0m", line);
        } else if trimmed.starts_with("warning:") {
            eprintln!("\x1B[33m{}\x1B[0m", line);
        } else {
            eprintln!("{}", line);
        }
    }

    child.wait().map(|s| s.success()).unwrap_or(false)
}

fn run_privileged_streaming(program: &str, args: &[&str]) -> bool {
    if is_root() {
        run_streaming(program, args)
    } else {
        let mut sudo_args = vec![program];
        sudo_args.extend_from_slice(args);
        run_streaming("sudo", &sudo_args)
    }
}

// Wraps every `nix <subcommand>` call with the experimental-features flags
// required by Lix/Nix when nix-command/flakes are not enabled system-wide.
fn run_nix(args: &[&str]) -> bool {
    let mut full: Vec<&str> = vec!["--extra-experimental-features", "nix-command flakes"];
    full.extend_from_slice(args);
    match Command::new("nix").args(&full).env("NIXPKGS_ALLOW_UNFREE", "1").status() {
        Ok(s) => s.success(),
        Err(e) => { eprintln!("[spin] Failed to run 'nix': {}", e); false }
    }
}

fn run_nix_streaming(args: &[&str]) -> bool {
    let mut full: Vec<&str> = vec!["--extra-experimental-features", "nix-command flakes"];
    full.extend_from_slice(args);
    run_streaming_env("nix", &full, &[("NIXPKGS_ALLOW_UNFREE", "1")])
}

fn nix_output(args: &[&str]) -> Option<String> {
    let mut full: Vec<&str> = vec!["--extra-experimental-features", "nix-command flakes"];
    full.extend_from_slice(args);
    Command::new("nix")
        .args(&full)
        .env("NIXPKGS_ALLOW_UNFREE", "1")
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

// ── packages.conf I/O ─────────────────────────────────────────────────────────

// Reads the two lists spin manages out of packages.conf:
// environment.systemPackages and fonts.packages.
fn read_conf() -> Result<(Vec<String>, Vec<String>), String> {
    if !Path::new(PACKAGES_FILE).exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let content = fs::read_to_string(PACKAGES_FILE)
        .map_err(|e| format!("Cannot read {}: {}", PACKAGES_FILE, e))?;
    Ok((
        parse_list(&content, "systemPackages"),
        parse_list(&content, "fonts.packages"),
    ))
}

// Collects entries from the `with pkgs; [ ... ];` block whose opening line
// contains `marker`. The two markers are mutually exclusive substrings, so each
// call independently finds its own block in the shared file.
fn parse_list(content: &str, marker: &str) -> Vec<String> {
    let mut packages = Vec::new();
    let mut in_list = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if !in_list {
            if trimmed.contains(marker) {
                in_list = true;
            }
            continue;
        }

        if trimmed.starts_with("];") {
            break;
        }

        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "[" {
            continue;
        }

        let pkg = trimmed.trim_end_matches(';').trim().to_string();
        if !pkg.is_empty() {
            packages.push(pkg);
        }
    }

    packages
}

fn write_conf(packages: &[String], fonts: &[String]) -> Result<(), String> {
    let mut content = String::new();
    content.push_str("# Managed by spin - Simple Package Installer for Nix\n");
    content.push_str("# Do not edit manually. Use: spin -i <pkg> / -r <pkg>  (fonts: spin -fi / -fr)\n");
    content.push_str("{ pkgs, ... }:\n{\n");

    content.push_str("  environment.systemPackages = with pkgs; [\n");
    for pkg in packages {
        content.push_str("    ");
        content.push_str(pkg);
        content.push('\n');
    }
    content.push_str("  ];\n");

    content.push_str("  fonts.packages = with pkgs; [\n");
    for font in fonts {
        content.push_str("    ");
        content.push_str(font);
        content.push('\n');
    }
    content.push_str("  ];\n}\n");

    write_privileged(PACKAGES_FILE, &content)
}

const CONFIG_FILE: &str = "/etc/nixos/configuration.nix";

fn ensure_packages_file() -> Result<(), String> {
    if !Path::new(PACKAGES_FILE).exists() {
        write_conf(&[], &[])?;
        println!("[spin] Created {}", PACKAGES_FILE);
    }
    check_config_imports();
    Ok(())
}

fn check_config_imports() {
    let content = match fs::read_to_string(CONFIG_FILE) {
        Ok(c) => c,
        Err(_) => return, // can't read config, skip silently
    };

    let imported = content
        .lines()
        .any(|l| !l.trim_start().starts_with('#') && l.contains("packages.conf"));

    if !imported {
        eprintln!(
            "[spin] Error: {} is not imported in {}.\n\
             \n\
             Add it to the imports list in {} and re-run:\n\
             \n\
             \x20\x20imports = [\n\
             \x20\x20  ./hardware-configuration.nix\n\
             \x20\x20  ./packages.conf        # <-- add this\n\
             \x20\x20];\n",
            PACKAGES_FILE, CONFIG_FILE, CONFIG_FILE
        );
        std::process::exit(1);
    }
}

// ── Nixpkgs search ────────────────────────────────────────────────────────────

type SearchMap = serde_json::Map<String, serde_json::Value>;

fn search_nixpkgs(query: &str) -> SearchMap {
    let json = nix_output(&["search", "--no-update-lock-file", "--json", "nixpkgs", query])
        .unwrap_or_default();
    serde_json::from_str(&json).unwrap_or_default()
}

// True if `s` could be a nixpkgs attribute path (also makes it safe to splice
// into the Nix expression below without further escaping).
fn is_attr_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

// Top-level attribute names of nixpkgs. Unlike `nix search`, this forces only
// the attrset keys (no per-package metadata), so it stays sub-second once
// nixpkgs is in the eval cache.
fn nixpkgs_attr_names() -> Vec<String> {
    let expr = "builtins.attrNames (import <nixpkgs> { })";
    nix_output(&["eval", "--impure", "--json", "--expr", expr])
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .unwrap_or_default()
}

// Levenshtein edit distance between two strings.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

// Ranks `cand` as a suggestion for the mistyped `typo`; lower is better.
// None means `cand` is too far from `typo` to be worth suggesting.
fn suggestion_score(typo: &str, cand: &str) -> Option<usize> {
    let typo = typo.to_lowercase();
    let cand = cand.to_lowercase();
    if cand == typo {
        Some(0)
    } else if cand.starts_with(&typo) {
        Some(1)
    } else if cand.contains(&typo) {
        Some(2)
    } else {
        // Allow more slack for longer names; never suggest wild guesses.
        let dist = edit_distance(&typo, &cand);
        (dist <= (typo.len() / 3).max(2)).then_some(3 + dist)
    }
}

// Verifies every name resolves to a real nixpkgs attribute *before* a rebuild.
//
// `nix search` is slow because it forces every package's metadata. Both the
// existence check and the typo suggestions avoid it: existence is one
// `nix eval` with `lib.hasAttrByPath`, and suggestions match locally against
// nixpkgs' top-level attribute names — both only inspect attrset keys, so they
// stay sub-second once nixpkgs is in the eval cache.
//
// Returns true if all names are valid; prints errors + suggestions otherwise.
// If nix itself cannot be queried, validation is skipped (warns, does not block).
fn validate_packages(names: &[String]) -> bool {
    let mut missing: Vec<&str> = Vec::new();
    let checkable: Vec<&str> = names
        .iter()
        .filter_map(|n| {
            if is_attr_name(n) {
                Some(n.as_str())
            } else {
                missing.push(n.as_str());
                None
            }
        })
        .collect();

    if !checkable.is_empty() {
        let list = checkable
            .iter()
            .map(|n| format!("\"{}\"", n))
            .collect::<Vec<_>>()
            .join(" ");
        let expr = format!(
            "let pkgs = import <nixpkgs> {{ }}; lib = pkgs.lib; names = [ {} ]; \
             in builtins.listToAttrs (map (n: {{ name = n; \
             value = lib.hasAttrByPath (lib.splitString \".\" n) pkgs; }}) names)",
            list
        );

        let parsed = nix_output(&["eval", "--impure", "--json", "--expr", expr.as_str()])
            .and_then(|json| {
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json).ok()
            });

        match parsed {
            Some(map) => {
                for name in &checkable {
                    if map.get(*name).and_then(|v| v.as_bool()) != Some(true) {
                        missing.push(name);
                    }
                }
            }
            None => {
                eprintln!("[spin] Warning: could not reach nixpkgs to validate names — skipping check.");
            }
        }
    }

    if missing.is_empty() {
        return true;
    }

    // Only on an actual typo: suggest close attribute names. We match locally
    // against nixpkgs' top-level attribute names (cheap — keys only) rather
    // than `nix search`, which forces every package's metadata.
    let attrs = nixpkgs_attr_names();
    for name in &missing {
        eprintln!("[spin] Error: '{}' was not found in nixpkgs.", name);
        let mut suggestions: Vec<(usize, usize, &str)> = attrs
            .iter()
            .filter_map(|attr| Some((suggestion_score(name, attr)?, attr.len(), attr.as_str())))
            .collect();
        suggestions.sort();
        suggestions.truncate(5);
        if !suggestions.is_empty() {
            let names: Vec<&str> = suggestions.iter().map(|&(_, _, p)| p).collect();
            eprintln!("       Did you mean: {}?", names.join(", "));
        }
    }
    false
}


// ── Search / clean commands ───────────────────────────────────────────────────

fn cmd_search(names: &[String]) {
    let pattern = names.join("|");
    print!("[spin] Searching nixpkgs for {}... ", names.join(", "));
    let _ = std::io::stdout().flush();
    let map = search_nixpkgs(&pattern);
    println!();

    for name in names {
        let mut results: Vec<(u8, String, String, String)> = map
            .iter()
            .filter_map(|(_, val)| {
                let pname = val["pname"].as_str()?.to_string();
                if !pname.contains(name.as_str()) {
                    return None;
                }
                let version = val["version"].as_str().unwrap_or("").to_string();
                let desc = val["description"].as_str().unwrap_or("").to_string();
                let score = if pname == *name { 0 } else if pname.starts_with(name.as_str()) { 1 } else { 2 };
                Some((score, pname, version, desc))
            })
            .collect();
        results.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        results.truncate(10);

        if results.is_empty() {
            println!("No results for '{}'.", name);
        } else {
            println!("Results for '{}':", name);
            for (_, pname, version, desc) in results {
                if version.is_empty() {
                    println!("  {}  —  {}", pname, desc);
                } else {
                    println!("  {} ({})  —  {}", pname, version, desc);
                }
            }
        }
        println!();
    }
}

fn cmd_clean(all: bool) -> bool {
    println!("[spin] Deleting old generations...");
    let gc_args: &[&str] = if all {
        &["--delete-old"]
    } else {
        &["--delete-older-than", "7d"]
    };
    if !run_privileged_streaming("nix-collect-garbage", gc_args) {
        return false;
    }
    println!("[spin] Updating bootloader...");
    run_privileged_streaming("nixos-rebuild", &["boot"])
}

// ── Profile package operations ────────────────────────────────────────────────

fn remove_profile(name: &str) -> Result<(), String> {
    let stdout = nix_output(&["profile", "list"])
        .ok_or_else(|| "Failed to list profile".to_string())?;

    // nix profile list format (nix ≥ 2.4):
    //   Index:           0
    //   Flake attribute: legacyPackages.x86_64-linux.git
    //   Original URL:    flake:nixpkgs
    //   Locked URL:      ...
    //   Store paths:     /nix/store/...
    //
    // We look for an "Index:" block whose "Flake attribute:" line ends with .<name>
    let mut current_index: Option<String> = None;
    let mut found_index: Option<String> = None;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(idx) = trimmed.strip_prefix("Index:") {
            current_index = Some(idx.trim().to_string());
        } else if trimmed.starts_with("Flake attribute:") {
            if trimmed.ends_with(&format!(".{}", name)) {
                found_index = current_index.clone();
                break;
            }
        }
    }

    match found_index {
        Some(idx) => {
            println!(
                "[spin] Removing '{}' (profile index {}) from user profile...",
                name, idx
            );
            if !run_nix(&["profile", "remove", &idx]) {
                return Err(format!("Failed to remove '{}' from profile.", name));
            }
            println!("[spin] Package '{}' removed from user profile.", name);
            Ok(())
        }
        None => Err(format!(
            "Package '{}' not found in user profile.\n\
             Run 'nix profile list' to see what is installed.",
            name
        )),
    }
}
