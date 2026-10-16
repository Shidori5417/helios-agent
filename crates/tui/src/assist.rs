//! Composer assists layered on the model: `@file` mention popup state, Ctrl+R
//! reverse history search and the Shift+Tab permission toggle. Key handlers
//! return `true` when they consumed the event.

use crate::{mention, TuiModel};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Only the newest history entries are searched.
pub const MAX_REVERSE_SEARCH_HISTORY: usize = 1_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReverseSearch {
    pub query: String,
    /// How many older matches to skip (Ctrl+R again cycles).
    pub skip: usize,
}

impl TuiModel {
    fn mention_token(&self) -> Option<mention::MentionToken> {
        if self.overlay.is_some() || self.reverse_search.is_some() {
            return None;
        }
        mention::mention_token(&self.composer.text, self.composer.cursor)
            .filter(|token| self.mention_dismissed != Some(token.start))
    }

    /// Builds the bounded file index on first `@` use and tracks the query.
    /// Call after every edit of the composer text.
    pub fn refresh_mentions(&mut self) {
        let Some(token) = mention::mention_token(&self.composer.text, self.composer.cursor) else {
            self.mention_dismissed = None;
            self.mention_query.clear();
            return;
        };
        if self
            .mention_dismissed
            .is_some_and(|start| start != token.start)
        {
            self.mention_dismissed = None;
        }
        if self.file_index.is_none() {
            let root = self
                .workspace_root
                .clone()
                .or_else(|| std::env::current_dir().ok());
            self.file_index = Some(
                root.map(|root| mention::walk_workspace(&root))
                    .unwrap_or_default(),
            );
        }
        if token.query != self.mention_query {
            self.mention_query = token.query;
            self.mention_selected = 0;
        }
    }

    /// Ranked file candidates for the active `@` token (empty when inactive).
    pub fn mention_matches(&self) -> Vec<String> {
        let (Some(token), Some(files)) = (self.mention_token(), &self.file_index) else {
            return Vec::new();
        };
        mention::rank_files(files, &token.query, mention::MAX_POPUP_ROWS)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Up/Down select, Tab/Enter insert the path, Esc closes the popup.
    pub fn handle_mention_key(&mut self, key: KeyEvent) -> bool {
        let matches = self.mention_matches();
        if matches.is_empty() {
            return false;
        }
        let Some(token) = self.mention_token() else {
            return false;
        };
        self.mention_selected = self.mention_selected.min(matches.len() - 1);
        match key.code {
            KeyCode::Up => {
                self.mention_selected = self
                    .mention_selected
                    .checked_sub(1)
                    .unwrap_or(matches.len() - 1);
            }
            KeyCode::Down => self.mention_selected = (self.mention_selected + 1) % matches.len(),
            KeyCode::Tab | KeyCode::Enter => {
                let (text, cursor) = mention::insert_path(
                    &self.composer.text,
                    &token,
                    &matches[self.mention_selected],
                );
                self.composer.text = text;
                self.composer.cursor = cursor;
                self.mention_dismissed = None;
                self.mention_query.clear();
            }
            KeyCode::Esc => self.mention_dismissed = Some(token.start),
            _ => return false,
        }
        true
    }

    /// History indices matching the current Ctrl+R query, newest first.
    pub fn reverse_matches(&self) -> Vec<usize> {
        let Some(search) = &self.reverse_search else {
            return Vec::new();
        };
        let query = search.query.to_lowercase();
        let oldest = self
            .history
            .len()
            .saturating_sub(MAX_REVERSE_SEARCH_HISTORY);
        (oldest..self.history.len())
            .rev()
            .filter(|&index| self.history[index].to_lowercase().contains(&query))
            .collect()
    }

    /// `(reverse-i-search)`query`: match` while Ctrl+R search is open.
    pub fn reverse_prompt(&self) -> Option<String> {
        let search = self.reverse_search.as_ref()?;
        let found = self
            .reverse_matches()
            .get(search.skip)
            .map(|&index| self.history[index].replace('\n', " "))
            .unwrap_or_default();
        Some(format!("(reverse-i-search)`{}`: {found}", search.query))
    }

    /// Ctrl+R opens the search; while open it owns the keyboard.
    pub fn handle_reverse_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.reverse_search.is_none() {
            if ctrl && key.code == KeyCode::Char('r') {
                self.reverse_search = Some(ReverseSearch::default());
                return true;
            }
            return false;
        }
        match key.code {
            KeyCode::Char('r') if ctrl => {
                let count = self.reverse_matches().len();
                if let Some(search) = self.reverse_search.as_mut() {
                    search.skip = (search.skip + 1).min(count.saturating_sub(1));
                }
            }
            KeyCode::Char('c' | 'g') if ctrl => self.reverse_search = None,
            KeyCode::Esc => self.reverse_search = None,
            KeyCode::Enter => {
                let skip = self.reverse_search.as_ref().map_or(0, |search| search.skip);
                let picked = self
                    .reverse_matches()
                    .get(skip)
                    .map(|&index| self.history[index].clone());
                self.reverse_search = None;
                if let Some(text) = picked {
                    self.composer.cursor = text.len();
                    self.composer.text = text;
                    self.history_index = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(search) = self.reverse_search.as_mut() {
                    search.query.pop();
                    search.skip = 0;
                }
            }
            KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(search) = self.reverse_search.as_mut() {
                    search.query.push(ch);
                    search.skip = 0;
                }
            }
            _ => {}
        }
        true
    }

    /// Local command Shift+Tab submits: toggles between the two existing
    /// modes, plan (read-only) and workspace-write. There is deliberately no
    /// auto-approve mode, and the header is only updated by the command's
    /// own result.
    pub fn permission_cycle(&self) -> Option<&'static str> {
        match self
            .permission_mode
            .as_deref()?
            .to_ascii_lowercase()
            .as_str()
        {
            "readonly" | "read-only" => Some("/permissions workspace-write"),
            "workspacewrite" | "workspace-write" => Some("/plan"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn model_with_files(files: &[&str]) -> TuiModel {
        TuiModel {
            file_index: Some(files.iter().map(|file| (*file).to_owned()).collect()),
            ..TuiModel::default()
        }
    }

    fn type_text(model: &mut TuiModel, text: &str) {
        model.composer.text = text.into();
        model.composer.cursor = text.len();
        model.refresh_mentions();
    }

    #[test]
    fn mention_popup_selects_inserts_and_closes() {
        let mut model = model_with_files(&["src/lib.rs", "src/main.rs", "README.md"]);
        type_text(&mut model, "fix @main");
        assert_eq!(model.mention_matches(), vec!["src/main.rs"]);
        assert!(model.handle_mention_key(key(KeyCode::Tab)));
        assert_eq!(model.composer.text, "fix src/main.rs");
        assert_eq!(model.composer.cursor, model.composer.text.len());
        assert!(model.mention_matches().is_empty());

        type_text(&mut model, "see @src");
        assert!(model.mention_matches().len() >= 2);
        assert!(model.handle_mention_key(key(KeyCode::Down)));
        assert_eq!(model.mention_selected, 1);
        assert!(model.handle_mention_key(key(KeyCode::Esc)));
        assert!(model.mention_matches().is_empty());
        assert_eq!(model.composer.text, "see @src");
        assert!(!model.handle_mention_key(key(KeyCode::Char('x'))));
    }

    #[test]
    fn email_like_text_does_not_open_the_popup() {
        let mut model = model_with_files(&["a.rs"]);
        type_text(&mut model, "mail me a@b");
        assert!(model.mention_matches().is_empty());
        assert!(!model.handle_mention_key(key(KeyCode::Tab)));
    }

    #[test]
    fn reverse_search_filters_cycles_and_fills_the_composer() {
        let mut model = TuiModel {
            history: vec![
                "cargo test".into(),
                "git status".into(),
                "cargo build".into(),
            ],
            ..TuiModel::default()
        };
        assert!(!model.handle_reverse_key(key(KeyCode::Char('x'))));
        assert!(model.handle_reverse_key(ctrl('r')));
        for ch in "CARGO".chars() {
            assert!(model.handle_reverse_key(key(KeyCode::Char(ch))));
        }
        assert_eq!(model.reverse_matches(), vec![2, 0]);
        assert!(model
            .reverse_prompt()
            .is_some_and(|p| p.ends_with("cargo build")));
        assert!(model.handle_reverse_key(ctrl('r')));
        assert!(model
            .reverse_prompt()
            .is_some_and(|p| p.ends_with("cargo test")));
        assert!(model.handle_reverse_key(key(KeyCode::Enter)));
        assert_eq!(model.composer.text, "cargo test");
        assert!(model.reverse_search.is_none());

        assert!(model.handle_reverse_key(ctrl('r')));
        assert!(model.handle_reverse_key(key(KeyCode::Esc)));
        assert!(model.reverse_search.is_none());
        assert_eq!(model.composer.text, "cargo test");
    }

    #[test]
    fn shift_tab_only_toggles_between_existing_modes() {
        let mut model = TuiModel::default();
        assert_eq!(model.permission_cycle(), None);
        model.permission_mode = Some("ReadOnly".into());
        assert_eq!(
            model.permission_cycle(),
            Some("/permissions workspace-write")
        );
        model.permission_mode = Some("WorkspaceWrite".into());
        assert_eq!(model.permission_cycle(), Some("/plan"));
        model.permission_mode = Some("danger-full-access".into());
        assert_eq!(model.permission_cycle(), None);
        assert!(!model.handle_mention_key(key(KeyCode::BackTab)));
        assert!(!model.handle_reverse_key(key(KeyCode::BackTab)));
    }

    #[test]
    fn mode_change_event_updates_the_toggle_state() {
        let mut model = TuiModel {
            permission_mode: Some("WorkspaceWrite".into()),
            ..TuiModel::default()
        };
        assert_eq!(model.permission_cycle(), Some("/plan"));
        model.reduce(&agent_protocol::Event::PermissionModeChanged(
            "ReadOnly".into(),
        ));
        assert_eq!(model.permission_mode.as_deref(), Some("ReadOnly"));
        assert_eq!(
            model.permission_cycle(),
            Some("/permissions workspace-write")
        );
    }
}
