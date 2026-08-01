//! Interactive TUI for `otto plugin` (launched when no subcommand is given).
//! Uses `inquire` for menus, prompts, and selection lists.
//! Delegates all operations to the backing functions in `plugin_cli`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use inquire::{Confirm, MultiSelect, Select, Text};

use crate::plugin_cli;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the interactive plugin manager. Loops until the user exits.
/// `base` is the `.claude/` parent directory — either the user's home (user-global scope) or
/// a project root (project-level scope). It is passed to every `plugin_cli` operation.
pub async fn interactive_plugin_ui(base: PathBuf) -> anyhow::Result<()> {
    loop {
        let choice = main_menu(&base)?;
        match choice {
            MainChoice::ListPlugins => list_plugins(&base)?,
            MainChoice::InstallPlugin => install_plugin(&base).await?,
            MainChoice::UninstallPlugin => uninstall_plugin(&base).await?,
            MainChoice::TogglePlugins => toggle_plugins(&base).await?,
            MainChoice::ManageMarketplaces => marketplace_menu(&base).await?,
            MainChoice::Exit => break,
        }
        if !confirm_return_to_menu()? {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main menu
// ---------------------------------------------------------------------------

enum MainChoice {
    ListPlugins,
    InstallPlugin,
    UninstallPlugin,
    TogglePlugins,
    ManageMarketplaces,
    Exit,
}

fn main_menu(base: &Path) -> anyhow::Result<MainChoice> {
    let is_project = base != dirs::home_dir().unwrap_or_default();
    let title = if is_project {
        "Plugin Manager (project scope)"
    } else {
        "Plugin Manager (user scope)"
    };
    let choices = vec![
        "List plugins",
        "Install a plugin",
        "Uninstall a plugin",
        "Enable/Disable plugins",
        "Manage marketplaces",
        "Exit",
    ];
    let selected = Select::new(title, choices).with_vim_mode(true).prompt()?;
    Ok(match selected {
        "List plugins" => MainChoice::ListPlugins,
        "Install a plugin" => MainChoice::InstallPlugin,
        "Uninstall a plugin" => MainChoice::UninstallPlugin,
        "Enable/Disable plugins" => MainChoice::TogglePlugins,
        "Manage marketplaces" => MainChoice::ManageMarketplaces,
        "Exit" => MainChoice::Exit,
        _ => unreachable!(),
    })
}

fn confirm_return_to_menu() -> anyhow::Result<bool> {
    Ok(Confirm::new("Return to menu?")
        .with_default(true)
        .prompt()?)
}

// ---------------------------------------------------------------------------
// List plugins
// ---------------------------------------------------------------------------

fn list_plugins(home: &Path) -> anyhow::Result<()> {
    let plugins = plugin_cli::plugin_list(home)?;
    if plugins.is_empty() {
        let have_marketplaces = !plugin_cli::read_lockfile(home).entries.is_empty();
        if have_marketplaces {
            println!("All plugins are installed. Use 'Enable/Disable' to toggle them.");
        } else {
            println!(
                "No marketplaces in this scope. Add one with 'Manage marketplaces > Add a marketplace'."
            );
        }
        return Ok(());
    }
    let mut enabled_count = 0;
    println!();
    for (key, enabled) in &plugins {
        let status = if *enabled { "enabled  " } else { "available" };
        println!("  [{status}] {key}");
        if *enabled {
            enabled_count += 1;
        }
    }
    println!(
        "\n{} plugin(s) total, {} enabled",
        plugins.len(),
        enabled_count
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Install a plugin
// ---------------------------------------------------------------------------

async fn install_plugin(home: &Path) -> anyhow::Result<()> {
    let plugins = plugin_cli::plugin_list(home)?;
    let available: Vec<_> = plugins.iter().filter(|(_, enabled)| !*enabled).collect();

    if available.is_empty() {
        let have_marketplaces = !plugin_cli::read_lockfile(home).entries.is_empty();
        if have_marketplaces {
            println!("All available plugins are already installed.");
        } else {
            println!("No marketplaces in this scope. Add a marketplace first.");
        }
        return Ok(());
    }

    let choices: Vec<_> = available.iter().map(|(key, _)| key.as_str()).collect();
    let selected = Select::new("Select a plugin to install:", choices)
        .with_vim_mode(true)
        .prompt()?;

    if !Confirm::new(&format!("Install {selected}?"))
        .with_default(true)
        .prompt()?
    {
        println!("Install cancelled.");
        return Ok(());
    }

    match plugin_cli::plugin_install(selected, home).await {
        Ok(_) => println!("Installed {selected}"),
        Err(e) => eprintln!("Failed to install {selected}: {e}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Uninstall a plugin
// ---------------------------------------------------------------------------

async fn uninstall_plugin(home: &Path) -> anyhow::Result<()> {
    let plugins = plugin_cli::plugin_list(home)?;
    let enabled: Vec<_> = plugins.iter().filter(|(_, enabled)| *enabled).collect();

    if enabled.is_empty() {
        println!("No installed plugins to uninstall.");
        return Ok(());
    }

    let choices: Vec<_> = enabled.iter().map(|(key, _)| key.as_str()).collect();
    let selected = Select::new("Select a plugin to uninstall:", choices)
        .with_vim_mode(true)
        .prompt()?;

    if !Confirm::new(&format!("Uninstall {selected}?"))
        .with_default(false)
        .prompt()?
    {
        println!("Uninstall cancelled.");
        return Ok(());
    }

    match plugin_cli::plugin_uninstall(selected, home) {
        Ok(_) => println!("Uninstalled {selected}"),
        Err(e) => eprintln!("Failed to uninstall {selected}: {e}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Enable/Disable plugins (multi-select)
// ---------------------------------------------------------------------------

async fn toggle_plugins(home: &Path) -> anyhow::Result<()> {
    let plugins = plugin_cli::plugin_list(home)?;
    if plugins.is_empty() {
        println!("No plugins in this scope. Add a marketplace first.");
        return Ok(());
    }

    let all_keys: Vec<&str> = plugins.iter().map(|(key, _)| key.as_str()).collect();
    let defaults: Vec<usize> = plugins
        .iter()
        .enumerate()
        .filter(|(_, (_, enabled))| *enabled)
        .map(|(i, _)| i)
        .collect();

    let selected = MultiSelect::new(
        "Toggle plugins (Space to toggle, Enter to confirm):",
        all_keys.clone(),
    )
    .with_default(&defaults)
    .with_vim_mode(true)
    .prompt()?;

    let wanted: HashSet<&str> = selected.iter().copied().collect();
    let mut had_changes = false;

    for (key, currently_enabled) in &plugins {
        let should_be_enabled = wanted.contains(key.as_str());
        if should_be_enabled == *currently_enabled {
            continue;
        }
        had_changes = true;
        if should_be_enabled {
            eprintln!("Enabling {key}...");
            match plugin_cli::plugin_install(key, home).await {
                Ok(_) => println!("Enabled {key}"),
                Err(e) => eprintln!("Failed to enable {key}: {e}"),
            }
        } else {
            match plugin_cli::plugin_uninstall(key, home) {
                Ok(_) => println!("Disabled {key}"),
                Err(e) => eprintln!("Failed to disable {key}: {e}"),
            }
        }
    }

    if !had_changes {
        println!("No changes made.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Marketplace submenu
// ---------------------------------------------------------------------------

async fn marketplace_menu(home: &Path) -> anyhow::Result<()> {
    loop {
        let choices = vec![
            "List marketplaces",
            "Add a marketplace",
            "Remove a marketplace",
            "Update marketplace(s)",
            "Back to main menu",
        ];
        let choice = Select::new("Marketplace Manager", choices)
            .with_vim_mode(true)
            .prompt()?;

        match choice {
            "List marketplaces" => list_marketplaces(home)?,
            "Add a marketplace" => add_marketplace(home).await?,
            "Remove a marketplace" => remove_marketplace(home)?,
            "Update marketplace(s)" => update_marketplace(home).await?,
            "Back to main menu" => break,
            _ => unreachable!(),
        }

        if choice != "Back to main menu" && !confirm_return_to_menu()? {
            break;
        }
    }
    Ok(())
}

fn list_marketplaces(home: &Path) -> anyhow::Result<()> {
    let lock = plugin_cli::read_lockfile(home);
    if lock.entries.is_empty() {
        println!("No marketplaces in this scope.");
        return Ok(());
    }
    let is_project = home != dirs::home_dir().unwrap_or_default();
    let scope_tag = if is_project { "[project]" } else { "[user]" };
    println!();
    for (name, entry) in &lock.entries {
        let short_commit = &entry.commit[..entry.commit.len().min(12)];
        println!("  {scope_tag} {name}");
        println!("           url:    {}", entry.url);
        println!("           ref:    {}", entry.git_ref);
        println!("           commit: {short_commit}");
    }
    println!(
        "\n{} marketplace(s) total, {} materialized plugin(s)",
        lock.entries.len(),
        lock.plugins.len(),
    );
    Ok(())
}

async fn add_marketplace(home: &Path) -> anyhow::Result<()> {
    let url = Text::new("Marketplace git URL:")
        .with_help_message("e.g. https://github.com/example/my-marketplace.git")
        .prompt()?;

    if url.trim().is_empty() {
        println!("Add cancelled.");
        return Ok(());
    }

    let use_ref = Confirm::new("Pin to a specific branch/tag/commit?")
        .with_default(false)
        .prompt()?;

    let ref_ = if use_ref {
        Some(
            Text::new("Branch, tag, or commit:")
                .with_help_message("e.g. main, v1.0, abc123")
                .prompt()?,
        )
    } else {
        None
    };

    match plugin_cli::marketplace_add(url.trim(), ref_.as_deref(), home).await {
        Ok(name) => println!("Installed marketplace '{name}'"),
        Err(e) => eprintln!("Failed to add marketplace: {e}"),
    }
    Ok(())
}

fn remove_marketplace(home: &Path) -> anyhow::Result<()> {
    let lock = plugin_cli::read_lockfile(home);
    let names: Vec<&str> = lock.entries.keys().map(|s| s.as_str()).collect();
    if names.is_empty() {
        println!("No marketplaces to remove.");
        return Ok(());
    }

    let selected = Select::new("Select a marketplace to remove:", names)
        .with_vim_mode(true)
        .prompt()?;

    let plugin_count = lock
        .plugins
        .iter()
        .filter(|(k, _)| k.ends_with(&format!("@{selected}")))
        .count();

    let warning = if plugin_count > 0 {
        format!("Remove marketplace '{selected}' and its {plugin_count} plugin(s)?")
    } else {
        format!("Remove marketplace '{selected}'?")
    };

    if !Confirm::new(&warning).with_default(false).prompt()? {
        println!("Remove cancelled.");
        return Ok(());
    }

    match plugin_cli::marketplace_remove(selected, home) {
        Ok(_) => println!("Removed marketplace '{selected}'"),
        Err(e) => eprintln!("Failed to remove marketplace: {e}"),
    }
    Ok(())
}

async fn update_marketplace(home: &Path) -> anyhow::Result<()> {
    let lock = plugin_cli::read_lockfile(home);
    if lock.entries.is_empty() {
        println!("No marketplaces to update.");
        return Ok(());
    }

    let choices = vec![
        "Update all marketplaces",
        "Select one marketplace",
        "Cancel",
    ];
    let choice = Select::new("Update marketplaces", choices)
        .with_vim_mode(true)
        .prompt()?;

    let name: Option<&str> = match choice {
        "Update all marketplaces" => None,
        "Select one marketplace" => {
            let names: Vec<&str> = lock.entries.keys().map(|s| s.as_str()).collect();
            Some(
                Select::new("Select marketplace to update:", names)
                    .with_vim_mode(true)
                    .prompt()?,
            )
        }
        _ => return Ok(()),
    };

    eprintln!("Updating... (this may take a moment)");
    match plugin_cli::marketplace_update(name, home).await {
        Ok(updated) => {
            if updated.is_empty() {
                println!("Nothing to update (all are up to date).");
            } else {
                println!("Updated: {}", updated.join(", "));
            }
        }
        Err(e) => eprintln!("Failed to update: {e}"),
    }
    Ok(())
}
