//! Every space ever seen on this endpoint, so "show all" can list the closed
//! and hidden ones. Client-owned: the file is shared by all windows of the
//! endpoint and re-read when another window changes it.

use std::{collections::HashSet, path::Path, time::SystemTime};

use serde::{Deserialize, Serialize};

use super::*;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct KnownSpace {
    pub(super) label: String,
    pub(super) cwd: String,
    /// Typed into the shell when the space is reopened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) workspace_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct KnownSpaces {
    #[serde(default)]
    pub(super) spaces: Vec<KnownSpace>,
    #[serde(default)]
    pub(super) hidden: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SpaceAction {
    Hide(String),
    Show(String),
    Open(usize),
}

impl KnownSpaces {
    /// Records what the snapshot shows. Returns whether anything changed.
    pub(super) fn sync(&mut self, snapshot: &ClientShellSnapshot) -> bool {
        let before = self.clone();
        let live = snapshot
            .workspaces
            .iter()
            .map(|workspace| workspace.workspace_id.clone())
            .collect::<HashSet<_>>();
        for workspace in &snapshot.workspaces {
            let known = self
                .spaces
                .iter()
                .position(|space| space.workspace_id.as_ref() == Some(&workspace.workspace_id))
                .or_else(|| {
                    // A closed space opened again under the same name is the same space.
                    self.spaces.iter().position(|space| {
                        space.label == workspace.label
                            && space
                                .workspace_id
                                .as_ref()
                                .is_none_or(|id| !live.contains(id))
                    })
                });
            let index = known.unwrap_or_else(|| {
                self.spaces.push(KnownSpace {
                    cwd: workspace.new_workspace_cwd.clone(),
                    ..KnownSpace::default()
                });
                self.spaces.len() - 1
            });
            let space = &mut self.spaces[index];
            space.label.clone_from(&workspace.label);
            space.workspace_id = Some(workspace.workspace_id.clone());
            if space.command.is_none() {
                space.command = snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.workspace_id == workspace.workspace_id)
                    .and_then(|agent| agent.agent.clone());
            }
        }
        self.hidden.retain(|id| live.contains(id));
        *self != before
    }

    /// Adds roster agents not known yet, so "show all" lists every agent even
    /// before it was ever opened. A space with the same name or folder is the
    /// same agent (a renamed space keeps its folder).
    pub(super) fn seed(&mut self, roster: &[KnownSpace]) -> bool {
        let same_dir = |a: &str, b: &str| {
            a == b
                || std::fs::canonicalize(a)
                    .ok()
                    .is_some_and(|a| std::fs::canonicalize(b).ok() == Some(a))
        };
        let mut changed = false;
        for agent in roster {
            let known = self.spaces.iter().any(|space| {
                space.label == agent.label || same_dir(&space.cwd, &agent.cwd)
            });
            if !known {
                self.spaces.push(agent.clone());
                changed = true;
            }
        }
        changed
    }

    pub(super) fn is_hidden(&self, workspace_id: &str) -> bool {
        self.hidden.contains(workspace_id)
    }

    /// Rows for the spaces that are not in the regular list, in creation order.
    pub(super) fn rows(&self, snapshot: &ClientShellSnapshot) -> Vec<(String, SpaceAction)> {
        self.spaces
            .iter()
            .enumerate()
            .filter_map(|(index, space)| {
                let live = space.workspace_id.as_ref().and_then(|id| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == *id)
                });
                let action = match live {
                    None => SpaceAction::Open(index),
                    Some(workspace)
                        if self.is_hidden(&workspace.workspace_id) && !workspace.focused =>
                    {
                        SpaceAction::Show(workspace.workspace_id.clone())
                    }
                    Some(_) => return None,
                };
                Some((space.label.clone(), action))
            })
            .collect()
    }

    fn load(path: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    fn store(&self, path: &Path) -> Result<(), String> {
        let content = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("failed to encode known spaces: {error}"))?;
        let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temp_path, content)
            .and_then(|()| std::fs::rename(&temp_path, path))
            .map_err(|error| format!("failed to write known spaces: {error}"))
    }
}

/// The agent roster: `HERDR_ROSTER` or ~/dotfiles/herdr/workspaces.tsv
/// (label, folder, agent, separated by tabs). Missing file means no roster.
fn roster() -> &'static [KnownSpace] {
    static ROSTER: std::sync::OnceLock<Vec<KnownSpace>> = std::sync::OnceLock::new();
    ROSTER.get_or_init(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::env::var("HERDR_ROSTER")
            .unwrap_or_else(|_| format!("{home}/dotfiles/herdr/workspaces.tsv"));
        std::fs::read_to_string(path)
            .map(|text| parse_roster(&text, &home))
            .unwrap_or_default()
    })
}

fn parse_roster(text: &str, home: &str) -> Vec<KnownSpace> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let label = cols.next()?.trim();
            let cwd = cols.next()?.trim();
            if label.is_empty() || cwd.is_empty() {
                return None;
            }
            let cwd = match cwd.strip_prefix('~') {
                Some(rest) => format!("{home}{rest}"),
                None => cwd.to_owned(),
            };
            // Same mapping as ~/dotfiles/bin/agente-cmd.
            let agent = cols.next().unwrap_or("").trim();
            let command = match agent {
                "claude" | "codex" | "gemini" | "opencode" => Some(agent.to_owned()),
                _ => agent
                    .strip_prefix("claude:")
                    .map(|name| format!("claude --agent {name}"))
                    .or_else(|| {
                        agent
                            .strip_prefix("openclaw:")
                            .map(|id| format!("openclaw tui agent:{id}:main"))
                    }),
            };
            Some(KnownSpace {
                label: label.to_owned(),
                cwd,
                command,
                workspace_id: None,
            })
        })
        .collect()
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

impl ClientShellState {
    fn known_spaces_path(&self) -> Option<std::path::PathBuf> {
        if self.active_endpoint_id != ClientEndpointId::Local {
            return None;
        }
        Some(
            self.config
                .preferences_path
                .as_deref()?
                .with_extension("spaces.json"),
        )
    }

    fn reload_known_spaces(&mut self, path: &Path) {
        let stamp = modified(path);
        if stamp != self.known_spaces_modified {
            self.known_spaces = KnownSpaces::load(path).unwrap_or_default();
            self.known_spaces_modified = stamp;
        }
    }

    fn store_known_spaces(&mut self, path: &Path) {
        if let Err(error) = self.known_spaces.store(path) {
            self.set_endpoint_error(error);
        }
        self.known_spaces_modified = modified(path);
    }

    pub(super) fn sync_known_spaces(&mut self) {
        let Some(path) = self.known_spaces_path() else {
            return;
        };
        self.reload_known_spaces(&path);
        let changed = match self.snapshot.as_deref() {
            // Live spaces first, so a renamed roster agent matches its open space.
            Some(snapshot) => self.known_spaces.sync(snapshot) | self.known_spaces.seed(roster()),
            None => false,
        };
        if changed {
            self.store_known_spaces(&path);
        }
    }

    pub(super) fn visible_workspace_entries(
        &self,
        snapshot: &ClientShellSnapshot,
        collapsed_groups: &HashSet<String>,
    ) -> Vec<WorkspaceEntry> {
        let mut entries = render::workspace_entries(snapshot, collapsed_groups);
        if self.active_endpoint_id == ClientEndpointId::Local {
            retain_visible(&mut entries, snapshot, &self.known_spaces);
        }
        entries
    }

    pub(super) fn set_space_hidden(
        &mut self,
        workspace_id: String,
        hidden: bool,
        outcome: &mut ClientShellInput,
    ) {
        let Some(path) = self.known_spaces_path() else {
            return;
        };
        self.reload_known_spaces(&path);
        if hidden {
            self.known_spaces.hidden.insert(workspace_id.clone());
        } else {
            self.known_spaces.hidden.remove(&workspace_id);
        }
        self.store_known_spaces(&path);
        // The focused space always stays listed, so hiding it has to move focus away.
        let next = if hidden {
            self.snapshot.as_deref().and_then(|snapshot| {
                let focused = snapshot
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.workspace_id == workspace_id && workspace.focused);
                let collapsed = HashSet::new();
                focused
                    .then(|| self.visible_workspace_entries(snapshot, &collapsed))
                    .and_then(|entries| {
                        entries
                            .iter()
                            .map(|entry| &snapshot.workspaces[entry.index].workspace_id)
                            .find(|id| **id != workspace_id)
                            .cloned()
                    })
            })
        } else {
            Some(workspace_id)
        };
        if let Some(workspace_id) = next {
            self.push_endpoint_method(
                crate::api::schema::Method::WorkspaceFocus(crate::api::schema::WorkspaceTarget {
                    workspace_id,
                }),
                outcome,
            );
        }
        outcome.repaint = true;
    }

    pub(super) fn activate_space_action(
        &mut self,
        action: SpaceAction,
        outcome: &mut ClientShellInput,
    ) {
        match action {
            SpaceAction::Hide(workspace_id) => self.set_space_hidden(workspace_id, true, outcome),
            SpaceAction::Show(workspace_id) => self.set_space_hidden(workspace_id, false, outcome),
            SpaceAction::Open(index) => {
                let Some(space) = self.known_spaces.spaces.get(index).cloned() else {
                    return;
                };
                self.push_endpoint_method_with_kind(
                    crate::api::schema::Method::WorkspaceCreate(
                        crate::api::schema::WorkspaceCreateParams {
                            source_workspace_id: None,
                            cwd: Some(space.cwd),
                            focus: true,
                            label: Some(space.label),
                            env: Default::default(),
                        },
                    ),
                    PendingEndpointKind::SpaceReopen {
                        command: space.command,
                    },
                    outcome,
                );
                outcome.repaint = true;
            }
        }
    }
}

pub(super) fn retain_visible(
    entries: &mut Vec<WorkspaceEntry>,
    snapshot: &ClientShellSnapshot,
    spaces: &KnownSpaces,
) {
    entries.retain(|entry| {
        let workspace = &snapshot.workspaces[entry.index];
        workspace.focused || !spaces.is_hidden(&workspace.workspace_id)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(id: &str, label: &str, focused: bool) -> ClientShellWorkspace {
        ClientShellWorkspace {
            workspace_id: id.into(),
            active_tab_id: String::new(),
            new_workspace_cwd: format!("/{label}"),
            number: 1,
            label: label.into(),
            custom_label: true,
            branch: None,
            git_ahead_behind: None,
            tokens: Vec::new(),
            worktree: None,
            focused,
            agent_status: crate::api::schema::AgentStatus::Idle,
        }
    }

    fn snapshot(workspaces: Vec<ClientShellWorkspace>) -> ClientShellSnapshot {
        ClientShellSnapshot {
            workspaces,
            ..super::super::tests::snapshot()
        }
    }

    #[test]
    fn closed_spaces_stay_known_and_reopening_by_name_reuses_the_entry() {
        let mut spaces = KnownSpaces::default();
        assert!(spaces.sync(&snapshot(vec![
            workspace("w1", "general", true),
            workspace("w2", "pacha", false),
        ])));

        let one_left = snapshot(vec![workspace("w1", "general", true)]);
        assert!(!spaces.sync(&one_left));
        assert_eq!(
            spaces.rows(&one_left),
            [("pacha".to_owned(), SpaceAction::Open(1))]
        );

        spaces.sync(&snapshot(vec![
            workspace("w1", "general", true),
            workspace("w9", "pacha", false),
        ]));
        assert_eq!(spaces.spaces.len(), 2);
        assert_eq!(spaces.spaces[1].workspace_id.as_deref(), Some("w9"));
    }

    #[test]
    fn roster_agents_are_listed_once_matching_renamed_spaces_by_folder() {
        let roster = parse_roster(
            "# comment\npacha\t~/ws/pacha\topenclaw:pacha\tbusiness\nweb\t/web\tclaude:jobs\ngwagon\t/gwagon\tnone\n",
            "/h",
        );
        assert_eq!(roster.len(), 3);
        assert_eq!(roster[0].cwd, "/h/ws/pacha");
        assert_eq!(roster[0].command.as_deref(), Some("openclaw tui agent:pacha:main"));
        assert_eq!(roster[1].command.as_deref(), Some("claude --agent jobs"));
        assert_eq!(roster[2].command, None);

        let mut spaces = KnownSpaces::default();
        let live = snapshot(vec![workspace("w7", "Coach", true)]);
        spaces.sync(&live);
        spaces.spaces[0].cwd = "/gwagon".into();
        assert!(spaces.seed(&roster));
        assert!(!spaces.seed(&roster));
        assert_eq!(
            spaces.rows(&live),
            [
                ("pacha".to_owned(), SpaceAction::Open(1)),
                ("web".to_owned(), SpaceAction::Open(2)),
            ]
        );
    }

    #[test]
    fn renaming_keeps_one_entry_and_the_first_cwd() {
        let mut spaces = KnownSpaces::default();
        spaces.sync(&snapshot(vec![workspace("w1", "gwagon", true)]));
        spaces.sync(&snapshot(vec![workspace("w1", "Coach", true)]));
        assert_eq!(spaces.spaces.len(), 1);
        assert_eq!(spaces.spaces[0].label, "Coach");
        assert_eq!(spaces.spaces[0].cwd, "/gwagon");
    }

    #[test]
    fn hidden_spaces_leave_the_list_unless_focused_and_are_pruned_when_closed() {
        let mut spaces = KnownSpaces::default();
        let both = snapshot(vec![
            workspace("w1", "general", true),
            workspace("w2", "pacha", false),
        ]);
        spaces.sync(&both);
        spaces.hidden.insert("w1".into());
        spaces.hidden.insert("w2".into());

        let mut entries = render::workspace_entries(&both, &HashSet::new());
        retain_visible(&mut entries, &both, &spaces);
        assert_eq!(entries.iter().map(|e| e.index).collect::<Vec<_>>(), [0]);
        assert_eq!(
            spaces.rows(&both),
            [("pacha".to_owned(), SpaceAction::Show("w2".into()))]
        );

        spaces.sync(&snapshot(vec![workspace("w1", "general", true)]));
        assert!(!spaces.is_hidden("w2"));
    }
}
