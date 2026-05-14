use clap::{Arg, ArgAction, Command as ClapCommand};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
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
        let nix_args: Vec<String> = ["shell", "--impure"].iter().map(|s| s.to_string())
            .chain(install_pkgs.iter().map(|p| format!("nixpkgs#{}", p)))
            .collect();
        let nix_args_ref: Vec<&str> = nix_args.iter().map(String::as_str).collect();
        println!("[spin] Opening temporary shell with {}...", install_pkgs.join(", "));
        return if run_nix(&nix_args_ref) { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    if is_profile {
        for pkg in &install_pkgs {
            println!("[spin] Installing '{}' to user profile...", pkg);
            if !run_nix_streaming(&["profile", "install", "--impure", &format!("nixpkgs#{}", pkg)]) {
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

    // 3. Prepare system changes: update packages.conf only, no rebuild yet.
    //    Track what changed so we can roll back if the rebuild fails.
    let mut added: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();

    for pkg in &install_pkgs {
        match prepare_install(pkg) {
            Ok(true)  => added.push(pkg.clone()),
            Ok(false) => {}
            Err(e)    => { eprintln!("[spin] Error: {}", e); return ExitCode::FAILURE; }
        }
    }
    for pkg in &remove_pkgs {
        match prepare_remove(pkg) {
            Ok(())  => removed.push(pkg.clone()),
            Err(e)  => { eprintln!("[spin] Error: {}", e); return ExitCode::FAILURE; }
        }
    }

    // 4. Single rebuild — use --upgrade when that flag was given.
    let needs_rebuild = do_upgrade || !added.is_empty() || !removed.is_empty();
    if needs_rebuild {
        let flags: &[&str] = if do_upgrade { &["switch", "--upgrade"] } else { &["switch"] };
        if !run_privileged_streaming("nixos-rebuild", flags) {
            // Roll back conf changes so the file stays consistent.
            if !added.is_empty() {
                let mut pkgs = read_packages().unwrap_or_default();
                pkgs.retain(|p| !added.contains(p));
                let _ = write_packages(&pkgs);
            }
            if !removed.is_empty() {
                let mut pkgs = read_packages().unwrap_or_default();
                pkgs.extend(removed.iter().cloned());
                pkgs.sort();
                let _ = write_packages(&pkgs);
            }
            eprintln!("[spin] Error: nixos-rebuild failed — changes rolled back.");
            return ExitCode::FAILURE;
        }
    }

    for pkg in &added   { println!("[spin] Package '{}' installed successfully.", pkg); }
    for pkg in &removed { println!("[spin] Package '{}' removed successfully.", pkg); }
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
            "spin is a friendly wrapper around NixOS package management.\n\
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
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
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

fn read_packages() -> Result<Vec<String>, String> {
    if !Path::new(PACKAGES_FILE).exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(PACKAGES_FILE)
        .map_err(|e| format!("Cannot read {}: {}", PACKAGES_FILE, e))?;
    Ok(parse_packages(&content))
}

fn parse_packages(content: &str) -> Vec<String> {
    let mut packages = Vec::new();
    let mut in_list = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if !in_list {
            if trimmed.contains("systemPackages") {
                in_list = true;
            }
            continue;
        }

        if trimmed.starts_with("];") || trimmed == "];" {
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

fn write_packages(packages: &[String]) -> Result<(), String> {
    let mut content = String::from(
        "# Managed by spin - Simple Package Installer for Nix\n\
         # Do not edit manually. Use: spin -i <package> / spin -r <package>\n\
         { pkgs, ... }:\n\
         {\n\
           environment.systemPackages = with pkgs; [\n",
    );
    for pkg in packages {
        content.push_str(&format!("    {}\n", pkg));
    }
    content.push_str("  ];\n}\n");

    write_privileged(PACKAGES_FILE, &content)
}

const CONFIG_FILE: &str = "/etc/nixos/configuration.nix";

fn ensure_packages_file() -> Result<(), String> {
    if !Path::new(PACKAGES_FILE).exists() {
        write_packages(&[])?;
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

// ── System package operations ─────────────────────────────────────────────────

// Adds the package to packages.conf. Does NOT search or rebuild.
// Returns Ok(true) if added, Ok(false) if already present.
fn prepare_install(name: &str) -> Result<bool, String> {
    ensure_packages_file()?;

    let mut packages = read_packages()?;

    if packages.contains(&name.to_string()) {
        println!("[spin] Package '{}' is already installed.", name);
        return Ok(false);
    }

    packages.push(name.to_string());
    packages.sort();
    write_packages(&packages)?;
    println!("[spin] Queued '{}' for installation.", name);
    Ok(true)
}

// Removes the package from packages.conf. Does NOT rebuild.
fn prepare_remove(name: &str) -> Result<(), String> {
    let mut packages = read_packages()?;

    if !packages.contains(&name.to_string()) {
        return Err(format!(
            "Package '{}' is not managed by spin (not in {}).",
            name, PACKAGES_FILE
        ));
    }

    packages.retain(|p| p != name);
    write_packages(&packages)?;
    println!("[spin] Queued '{}' for removal.", name);
    Ok(())
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
