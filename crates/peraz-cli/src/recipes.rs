//! Finding recipes and the `peraz recipe` command.
//!
//! Recipes come from three places, later ones replacing earlier ones by name:
//! 1. built into Peraz,
//! 2. `<config dir>/recipes/<name>.toml`,
//! 3. a path given directly to `--recipe` (anything containing a path separator or ending in
//!    `.toml`).
//!
//! The config directory is `PERAZ_CONFIG_DIR` when set, otherwise the platform's usual place:
//! `~/Library/Application Support/peraz` (macOS), `$XDG_CONFIG_HOME/peraz` or `~/.config/peraz`
//! (Linux), `%APPDATA%\peraz` (Windows).

use std::path::{Path, PathBuf};

use peraz_core::settings::{BUILTIN_RECIPES, Settings};
use serde::Serialize;

use crate::{exit, print_json};

/// Where a recipe came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Builtin,
    User,
    File,
}

/// A recipe ready to use.
#[derive(Debug, Clone, Serialize)]
pub struct Recipe {
    pub name: String,
    pub source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub settings: Settings,
}

/// Peraz's configuration folder.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PERAZ_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = || {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library/Application Support/peraz"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("peraz"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".config")))
            .map(|c| c.join("peraz"))
    }
}

/// The folder holding user recipes.
pub fn recipes_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("recipes"))
}

fn load_file(path: &Path, name: String, source: Source) -> Result<Recipe, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read recipe {}: {e}", path.display()))?;
    let settings =
        Settings::from_toml(&text).map_err(|e| format!("recipe {}: {e}", path.display()))?;
    Ok(Recipe {
        name,
        source,
        path: Some(path.to_owned()),
        settings,
    })
}

/// Every available recipe, by name: built-ins, replaced by user recipes of the same name.
/// Broken user recipes are reported as errors next to the good ones.
pub fn all() -> (Vec<Recipe>, Vec<String>) {
    all_in(recipes_dir().as_deref())
}

fn all_in(user_dir: Option<&Path>) -> (Vec<Recipe>, Vec<String>) {
    let mut recipes: Vec<Recipe> = BUILTIN_RECIPES
        .iter()
        .map(|b| Recipe {
            name: b.name.to_owned(),
            source: Source::Builtin,
            path: None,
            settings: Settings::from_toml(b.toml).expect("built-in recipes are valid"),
        })
        .collect();
    let mut errors = Vec::new();
    if let Some(dir) = user_dir
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            match load_file(&path, name, Source::User) {
                Ok(recipe) => {
                    recipes.retain(|r| r.name != recipe.name);
                    recipes.push(recipe);
                }
                Err(e) => errors.push(e),
            }
        }
    }
    recipes.sort_by(|a, b| a.name.cmp(&b.name));
    (recipes, errors)
}

/// Finds a recipe by name, or loads it from a path.
pub fn find(name_or_path: &str) -> Result<Recipe, String> {
    find_in(name_or_path, recipes_dir().as_deref())
}

fn find_in(name_or_path: &str, user_dir: Option<&Path>) -> Result<Recipe, String> {
    let looks_like_path = name_or_path.contains(['/', '\\'])
        || Path::new(name_or_path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("toml"));
    if looks_like_path {
        let path = Path::new(name_or_path);
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        return load_file(path, name, Source::File);
    }
    let (recipes, errors) = all_in(user_dir);
    if let Some(recipe) = recipes.into_iter().find(|r| r.name == name_or_path) {
        return Ok(recipe);
    }
    let mut message = format!("no recipe named `{name_or_path}`; run `peraz recipe list`");
    if let Some(error) = errors
        .into_iter()
        .find(|e| e.contains(&format!("{name_or_path}.toml")))
    {
        message = error;
    }
    Err(message)
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// List built-in and user recipes.
    List {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show a recipe's settings as TOML (ready to copy into your own recipe).
    Show {
        /// Recipe name or path to a .toml file.
        name: String,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Serialize)]
struct List<'a> {
    recipes_dir: Option<PathBuf>,
    recipes: &'a [Recipe],
    errors: &'a [String],
}

pub fn run(command: &Command) -> u8 {
    match command {
        Command::List { json } => {
            let (recipes, errors) = all();
            if *json {
                print_json(List {
                    recipes_dir: recipes_dir(),
                    recipes: &recipes,
                    errors: &errors,
                });
            } else {
                for recipe in &recipes {
                    let tag = match recipe.source {
                        Source::Builtin => "built-in",
                        Source::User | Source::File => "yours",
                    };
                    let description = recipe.settings.description.as_deref().unwrap_or("");
                    println!("{:<18} {:<9} {description}", recipe.name, tag);
                }
                match recipes_dir() {
                    Some(dir) => println!("\nYour recipes go in {}", dir.display()),
                    None => println!("\nSet PERAZ_CONFIG_DIR to keep your own recipes."),
                }
                for error in &errors {
                    eprintln!("warning: {error}");
                }
            }
            if errors.is_empty() {
                exit::SUCCESS
            } else {
                exit::INPUT_FAILED
            }
        }
        Command::Show { name, json } => match find(name) {
            Ok(recipe) => {
                if *json {
                    print_json(recipe);
                } else {
                    let origin = recipe.path.as_ref().map_or_else(
                        || "built into Peraz".to_owned(),
                        |p| p.display().to_string(),
                    );
                    println!(
                        "# {} ({origin})\n{}",
                        recipe.name,
                        recipe.settings.to_toml()
                    );
                }
                exit::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                exit::INPUT_FAILED
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_recipes_replace_builtins_and_paths_load_directly() {
        let dir = std::env::temp_dir().join(format!("peraz-recipes-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("recipes")).unwrap();
        std::fs::write(
            dir.join("recipes/web-images.toml"),
            "version = 1\ndescription = \"mine\"\nformat = \"avif\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("recipes/broken.toml"),
            "version = 1\nqualty = \"high\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("one-off.toml"),
            "version = 1\nquality = \"small\"\n",
        )
        .unwrap();
        let user = dir.join("recipes");
        let (recipes, errors) = all_in(Some(&user));
        let web = recipes.iter().find(|r| r.name == "web-images").unwrap();
        assert_eq!(
            (web.source, web.settings.format.as_deref()),
            (Source::User, Some("avif"))
        );
        assert!(
            recipes
                .iter()
                .any(|r| r.name == "gif-preview" && r.source == Source::Builtin)
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            find_in("broken", Some(&user))
                .unwrap_err()
                .contains("qualty")
        );
        assert!(
            find_in("nope", Some(&user))
                .unwrap_err()
                .contains("peraz recipe list")
        );
        let file = find_in(dir.join("one-off.toml").to_str().unwrap(), Some(&user)).unwrap();
        assert_eq!(file.source, Source::File);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
