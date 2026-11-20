//! Where copies of catalog servers live.
//!
//! The catalog is the source, but the same server definitions are copied into
//! more places than the projects kms discovers:
//!
//! | category | files | synchronized by default |
//! |---|---|---|
//! | `Settings` | `<dir>/.kiro/settings/mcp.json` | yes |
//! | `Boost` | `<dir>/.ai/mcp/mcp.json` | yes |
//! | `Example` | `<dir>/.kiro/settings/mcp.json.example` | no (`--include examples`) |
//! | `Agent` | `~/.kiro/agents/*.json`, `~/.kiro/mates/agents/*.json` | no (`--include agents`) |
//!
//! `<dir>` is a project, one of its worktrees, or a Herd clone
//! (`~/.config/herd/devtools/clones/<id>/<name>`). Categories that are not
//! synchronized by default are still compared and reported.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::worktrees::path_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// A project's own Kiro config.
    Settings,
    /// Laravel Boost's `.ai/mcp/mcp.json`.
    Boost,
    /// A tracked `mcp.json.example`.
    Example,
    /// A Kiro agent spec embedding servers.
    Agent,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Settings => "settings",
            Category::Boost => "boost",
            Category::Example => "example",
            Category::Agent => "agent",
        }
    }
}

/// Which opt-in categories a command covers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Include {
    pub agents: bool,
    pub examples: bool,
}

impl Include {
    /// Parse the value of `--include`: a comma-separated list of `agents` and
    /// `examples`.
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut include = Include::default();
        for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "agents" => include.agents = true,
                "examples" => include.examples = true,
                other => {
                    return Err(format!(
                        "Unknown --include value \"{other}\". Expected agents, examples."
                    ))
                }
            }
        }
        Ok(include)
    }

    /// Whether `category` is in scope.
    pub fn covers(self, category: Category) -> bool {
        match category {
            Category::Settings | Category::Boost => true,
            Category::Example => self.examples,
            Category::Agent => self.agents,
        }
    }
}

/// One file that may hold copies of catalog servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyFile {
    pub path: PathBuf,
    pub category: Category,
    /// Whether it sits in a git worktree, so `--no-worktrees` can skip it.
    pub in_worktree: bool,
}

/// A directory that may hold Kiro and Boost configs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDir {
    pub dir: PathBuf,
    pub worktree: bool,
}

/// Machine-level places, injectable so tests never read the real home.
#[derive(Debug, Clone)]
pub struct Locations {
    pub home: PathBuf,
    /// `~/.config/herd/devtools/clones`.
    pub herd_clones: PathBuf,
}

impl Locations {
    pub fn for_home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            herd_clones: home
                .join(".config")
                .join("herd")
                .join("devtools")
                .join("clones"),
        }
    }

    pub fn detect() -> Option<Self> {
        dirs::home_dir().map(|home| Self::for_home(&home))
    }

    /// Where the agent specs live.
    pub fn agent_dirs(&self) -> [PathBuf; 2] {
        [
            self.home.join(".kiro").join("agents"),
            self.home.join(".kiro").join("mates").join("agents"),
        ]
    }
}

/// The copy files that exist under one directory.
pub fn files_in_dir(dir: &Path, in_worktree: bool) -> Vec<CopyFile> {
    let candidates = [
        (
            dir.join(".kiro").join("settings").join("mcp.json"),
            Category::Settings,
        ),
        (
            dir.join(".ai").join("mcp").join("mcp.json"),
            Category::Boost,
        ),
        (
            dir.join(".kiro").join("settings").join("mcp.json.example"),
            Category::Example,
        ),
    ];
    candidates
        .into_iter()
        .filter(|(path, _)| path.is_file())
        .map(|(path, category)| CopyFile {
            path,
            category,
            in_worktree,
        })
        .collect()
}

/// `clones/<id>/<name>` directories.
pub fn herd_clone_dirs(clones_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(ids) = std::fs::read_dir(clones_root) else {
        return found;
    };
    for id in ids.flatten().filter(|e| e.path().is_dir()) {
        let Ok(names) = std::fs::read_dir(id.path()) else {
            continue;
        };
        for name in names.flatten().filter(|e| e.path().is_dir()) {
            found.push(name.path());
        }
    }
    found.sort();
    found
}

/// `*.json` agent specs. Backups (`*.json.bak`), examples and anything that
/// does not end in exactly `.json` are not specs.
pub fn agent_files(locations: &Locations) -> Vec<CopyFile> {
    let mut found = Vec::new();
    for dir in locations.agent_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_json = path.extension().is_some_and(|ext| ext == "json");
            if is_json && path.is_file() {
                found.push(CopyFile {
                    path,
                    category: Category::Agent,
                    in_worktree: false,
                });
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Every copy file for the given directories, plus Herd clones and agents,
/// each path once.
pub fn collect(projects: &[ProjectDir], locations: &Locations) -> Vec<CopyFile> {
    let mut files = Vec::new();
    for project in projects {
        files.extend(files_in_dir(&project.dir, project.worktree));
    }
    for clone in herd_clone_dirs(&locations.herd_clones) {
        files.extend(files_in_dir(&clone, false));
    }
    files.extend(agent_files(locations));

    let mut seen = std::collections::HashSet::new();
    files.retain(|file| seen.insert(path_key(&file.path)));
    files
}

/// Where KiroCrew would also read a server from when it rebuilds an agent
/// spec. A server present there is re-rendered from that source, so fixing the
/// agent file alone does not last.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentSources {
    /// `~/.kiro/settings/mcp.json`
    pub global: bool,
    /// `~/.kiro/crew/mcp.json`
    pub crew_store: bool,
}

impl AgentSources {
    pub fn any(self) -> bool {
        self.global || self.crew_store
    }
}

fn file_defines(path: &Path, server: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| super::jsonedit::parse(&raw).ok())
        .and_then(|data| data.get("mcpServers").cloned())
        .is_some_and(|servers| matches!(servers, Value::Object(map) if map.contains_key(server)))
}

/// Which KiroCrew rebuild sources define `server`.
pub fn agent_sources_for(locations: &Locations, server: &str) -> AgentSources {
    AgentSources {
        global: file_defines(
            &locations
                .home
                .join(".kiro")
                .join("settings")
                .join("mcp.json"),
            server,
        ),
        crew_store: file_defines(
            &locations.home.join(".kiro").join("crew").join("mcp.json"),
            server,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn include_parses_known_values_only() {
        assert_eq!(
            Include::parse("agents,examples").unwrap(),
            Include {
                agents: true,
                examples: true
            }
        );
        assert!(!Include::parse("agents").unwrap().examples);
        assert!(Include::parse("agents,bogus").is_err());
        assert!(Include::default().covers(Category::Boost));
        assert!(!Include::default().covers(Category::Agent));
    }

    #[test]
    fn finds_every_location_and_ignores_backups() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let proj = dir.path().join("proj");
        let wt = proj.join(".worktrees").join("feat");
        touch(&proj.join(".kiro/settings/mcp.json"), "{}");
        touch(&proj.join(".kiro/settings/mcp.json.example"), "{}");
        touch(&proj.join(".ai/mcp/mcp.json"), "{}");
        touch(&wt.join(".kiro/settings/mcp.json"), "{}");
        let locations = Locations::for_home(&home);
        touch(
            &locations.herd_clones.join("4273542e/site/.ai/mcp/mcp.json"),
            "{}",
        );
        touch(&home.join(".kiro/agents/kirocrew.json"), "{}");
        touch(&home.join(".kiro/agents/kirocrew.json.bak"), "{}");
        touch(&home.join(".kiro/agents/agent_config.json.example"), "{}");
        touch(&home.join(".kiro/mates/agents/mate.json"), "{}");

        let files = collect(
            &[
                ProjectDir {
                    dir: proj.clone(),
                    worktree: false,
                },
                ProjectDir {
                    dir: wt.clone(),
                    worktree: true,
                },
            ],
            &locations,
        );
        let count = |category| files.iter().filter(|f| f.category == category).count();
        assert_eq!(count(Category::Settings), 2);
        assert_eq!(count(Category::Boost), 2, "project and Herd clone");
        assert_eq!(count(Category::Example), 1);
        assert_eq!(count(Category::Agent), 2, "no .bak, no .example");
        assert!(files
            .iter()
            .any(|f| f.in_worktree && f.path.starts_with(&wt)));
    }

    #[test]
    fn a_path_listed_twice_is_collected_once() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        touch(&proj.join(".kiro/settings/mcp.json"), "{}");
        let entry = ProjectDir {
            dir: proj,
            worktree: false,
        };
        let files = collect(
            &[entry.clone(), entry],
            &Locations::for_home(&dir.path().join("home")),
        );
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn agent_sources_report_where_a_server_is_also_defined() {
        let dir = tempfile::tempdir().unwrap();
        let locations = Locations::for_home(dir.path());
        touch(
            &dir.path().join(".kiro/crew/mcp.json"),
            r#"{ "mcpServers": { "a": { "command": "x" } } }"#,
        );
        let a = agent_sources_for(&locations, "a");
        assert!(a.crew_store && !a.global && a.any());
        assert!(!agent_sources_for(&locations, "b").any());
    }
}
