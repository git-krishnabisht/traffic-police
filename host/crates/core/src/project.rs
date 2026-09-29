//! Project config: `.traffic-police/project.toml`, found by walking up from the working
//! directory (ARCHITECTURE.md §5.13).
//!
//! ```toml
//! package = "com.example.app"                       # default app (device mode)
//! source_roots = ["app/src/main/java", "app/src/main/kotlin"]
//! ```
//!
//! Relative source roots are resolved against the directory that contains `.traffic-police/`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DIR: &str = ".traffic-police";
pub const FILE: &str = "project.toml";

/// Sections that are planned but not applied yet; present ones are reported, not ignored.
const NOT_YET: [&str; 1] = ["redaction"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    /// The directory that contains `.traffic-police/`.
    pub root: PathBuf,
    pub package: Option<String>,
    pub source_roots: Vec<PathBuf>,
    /// Things the user should know (unknown keys, settings not applied yet).
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("{path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
}

#[derive(Deserialize)]
struct Raw {
    package: Option<String>,
    #[serde(default)]
    source_roots: Vec<PathBuf>,
    #[serde(flatten)]
    rest: toml::Table,
}

/// The nearest `.traffic-police/project.toml` at or above `start`, if any.
pub fn find(start: &Path) -> Result<Option<ProjectConfig>, ProjectError> {
    for dir in start.ancestors() {
        let path = dir.join(DIR).join(FILE);
        if path.is_file() {
            let text =
                std::fs::read_to_string(&path).map_err(|source| ProjectError::Read { path: path.clone(), source })?;
            return parse(dir, &path, &text).map(Some);
        }
    }
    Ok(None)
}

pub fn parse(root: &Path, path: &Path, text: &str) -> Result<ProjectConfig, ProjectError> {
    let raw: Raw =
        toml::from_str(text).map_err(|e| ProjectError::Parse { path: path.to_path_buf(), message: e.to_string() })?;
    let mut warnings = Vec::new();
    for key in raw.rest.keys() {
        if NOT_YET.contains(&key.as_str()) {
            warnings.push(format!("{}: [{key}] is not applied yet in this version", path.display()));
        } else {
            warnings.push(format!("{}: unknown key `{key}` ignored", path.display()));
        }
    }
    let source_roots = raw.source_roots.into_iter().map(|p| if p.is_absolute() { p } else { root.join(p) }).collect();
    Ok(ProjectConfig { root: root.to_path_buf(), package: raw.package, source_roots, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_resolves_roots() {
        let root = Path::new("/work/app");
        let text = "package = \"com.example.app\"\nsource_roots = [\"app/src/main/java\", \"/abs/src\"]\n";
        let c = parse(root, Path::new("/work/app/.traffic-police/project.toml"), text).unwrap();
        assert_eq!(c.package.as_deref(), Some("com.example.app"));
        assert_eq!(c.source_roots, vec![PathBuf::from("/work/app/app/src/main/java"), PathBuf::from("/abs/src")]);
        assert!(c.warnings.is_empty());
    }

    #[test]
    fn reports_unknown_and_unapplied_keys_and_errors() {
        let p = Path::new("p.toml");
        let c = parse(Path::new("/w"), p, "colour = 1\n[redaction]\nheaders = [\"x\"]\n").unwrap();
        assert_eq!(c.warnings.len(), 2, "{:?}", c.warnings);
        assert!(c.warnings.iter().any(|w| w.contains("[redaction] is not applied yet")));
        assert!(c.warnings.iter().any(|w| w.contains("unknown key `colour`")));
        let e = parse(Path::new("/w"), p, "source_roots = \"not a list\"\n").unwrap_err();
        assert!(e.to_string().starts_with("p.toml: "), "{e}");
    }

    #[test]
    fn finds_the_nearest_project_dir() {
        let base = std::env::temp_dir().join(format!("tp-project-test-{}", std::process::id()));
        let deep = base.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(base.join("a").join(DIR)).unwrap();
        std::fs::write(base.join("a").join(DIR).join(FILE), "source_roots = [\"src\"]\n").unwrap();
        let c = find(&deep).unwrap().expect("found");
        assert_eq!(c.root, base.join("a"));
        assert_eq!(c.source_roots, vec![base.join("a/src")]);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
