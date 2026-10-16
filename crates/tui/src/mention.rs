//! `@file` mention helpers: token detection, fuzzy scoring and a bounded,
//! symlink-safe workspace walker. Pure functions; no file contents are read.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

pub const MAX_WALK_ENTRIES: usize = 5_000;
pub const MAX_WALK_DEPTH: usize = 6;
pub const MAX_POPUP_ROWS: usize = 8;
const SKIP_DIRS: [&str; 2] = ["target", "node_modules"];

/// The `@token` under the cursor: byte range of the whole token plus the
/// query typed after `@` (up to the cursor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionToken {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

pub fn mention_token(text: &str, cursor: usize) -> Option<MentionToken> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let start = text[..cursor]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let head = &text[start..cursor];
    let query = head.strip_prefix('@')?;
    let end = text[cursor..]
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| cursor + i)
        .unwrap_or(text.len());
    Some(MentionToken {
        start,
        end,
        query: query.to_owned(),
    })
}

/// Replaces only the token with the path (backtick-quoted when it has spaces).
/// Returns the new text and cursor.
pub fn insert_path(text: &str, token: &MentionToken, path: &str) -> (String, usize) {
    let inserted = if path.chars().any(char::is_whitespace) {
        format!("`{path}`")
    } else {
        path.to_owned()
    };
    let mut out = String::with_capacity(text.len() + inserted.len());
    out.push_str(&text[..token.start]);
    out.push_str(&inserted);
    let cursor = out.len();
    out.push_str(&text[token.end..]);
    (out, cursor)
}

/// Case-insensitive subsequence score; `None` when `query` is not a subsequence.
/// Basename matches are boosted and shorter paths preferred.
pub fn fuzzy_score(query: &str, path: &str) -> Option<i64> {
    let len_penalty = path.chars().count() as i64;
    if query.is_empty() {
        return Some(-len_penalty);
    }
    let (full, _) = subsequence(query, path)?;
    let base_start = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    let mut score = full - len_penalty;
    if let Some((base, prefix)) = subsequence(query, &path[base_start..]) {
        score += 100 + base;
        if prefix {
            score += 50;
        }
    }
    Some(score)
}

/// Returns (score, matched-at-start) for a greedy subsequence match.
fn subsequence(query: &str, haystack: &str) -> Option<(i64, bool)> {
    let mut wanted = query.chars().flat_map(char::to_lowercase).peekable();
    let mut score = 0i64;
    let mut previous: Option<char> = None;
    let mut last_matched = false;
    let mut at_start = false;
    for (index, c) in haystack.chars().enumerate() {
        let Some(&next) = wanted.peek() else { break };
        if c.to_lowercase().next() == Some(next) {
            wanted.next();
            score += 1;
            if last_matched {
                score += 5;
            }
            if matches!(previous, Some('/' | '_' | '-' | '.') | None) {
                score += 3;
            }
            if index == 0 {
                at_start = true;
            }
            last_matched = true;
        } else {
            last_matched = false;
        }
        previous = Some(c);
    }
    wanted.peek().is_none().then_some((score, at_start))
}

pub fn rank_files<'a>(files: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    let mut scored: Vec<(i64, &str)> = files
        .iter()
        .filter_map(|path| fuzzy_score(query, path).map(|s| (s, path.as_str())))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, p)| p).collect()
}

/// Breadth-first bounded walk returning workspace-relative file paths.
/// Never follows symlinks (they are skipped entirely); skips `.git`,
/// `target`, `node_modules` and every dot-directory.
pub fn walk_workspace(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    let mut seen = 0usize;
    queue.push_back((root.to_path_buf(), 0));
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = read.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            seen += 1;
            if seen > MAX_WALK_ENTRIES * 2 || files.len() >= MAX_WALK_ENTRIES {
                return files;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if kind.is_dir() {
                if depth + 1 < MAX_WALK_DEPTH
                    && !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name)
                {
                    queue.push_back((entry.path(), depth + 1));
                }
            } else if kind.is_file() {
                if let Ok(relative) = entry.path().strip_prefix(root) {
                    if let Some(relative) = relative.to_str() {
                        files.push(relative.to_owned());
                    }
                }
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_detection() {
        assert_eq!(mention_token("@sr", 3).unwrap().query, "sr");
        let t = mention_token("see @src/li more", 11).unwrap();
        assert_eq!((t.start, t.end, t.query.as_str()), (4, 11, "src/li"));
        assert!(mention_token("a@b", 3).is_none());
        assert!(mention_token("hello", 5).is_none());
        assert_eq!(mention_token("x @", 3).unwrap().query, "");
        assert!(mention_token("é@b", 4).is_none());
    }

    #[test]
    fn insertion_replaces_only_token() {
        let text = "look at @sr and @other";
        let t = mention_token(text, 11).unwrap();
        let (out, cursor) = insert_path(text, &t, "src/lib.rs");
        assert_eq!(out, "look at src/lib.rs and @other");
        assert_eq!(cursor, "look at src/lib.rs".len());
        let (out, _) = insert_path("@a", &mention_token("@a", 2).unwrap(), "my dir/f.rs");
        assert_eq!(out, "`my dir/f.rs`");
    }

    #[test]
    fn scorer_orders_basename_and_short_paths_first() {
        assert!(fuzzy_score("zz", "abc").is_none());
        let files: Vec<String> = [
            "docs/lib_notes/readme.md",
            "crates/tui/src/lib.rs",
            "lib.rs",
            "src/library/other.txt",
        ]
        .map(String::from)
        .to_vec();
        let ranked = rank_files(&files, "lib.rs", 8);
        assert_eq!(ranked[0], "lib.rs");
        assert_eq!(ranked[1], "crates/tui/src/lib.rs");
        assert!(!ranked.contains(&"docs/lib_notes/readme.md"));
        assert_eq!(rank_files(&files, "", 2).len(), 2);
        assert_eq!(rank_files(&files, "LIB", 8)[0], "lib.rs");
    }

    #[test]
    fn walker_is_bounded_and_does_not_follow_symlinks() {
        let root = std::env::temp_dir().join(format!("helios-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        for dir in [".git", "target", "node_modules", ".hidden"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for f in [
            "a/b/c.rs",
            "top.rs",
            ".git/x",
            "target/x",
            "node_modules/x",
            ".hidden/x",
        ] {
            std::fs::write(root.join(f), "").unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&root, root.join("a/loop")).unwrap();
            std::os::unix::fs::symlink(root.join("top.rs"), root.join("link.rs")).unwrap();
        }
        let files = walk_workspace(&root);
        assert_eq!(files, vec!["top.rs".to_owned(), "a/b/c.rs".to_owned()]);

        let mut deep = root.join("deep");
        for i in 0..10 {
            deep = deep.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.rs"), "").unwrap();
        assert!(!walk_workspace(&root).iter().any(|f| f.ends_with("deep.rs")));

        let many = root.join("many");
        std::fs::create_dir_all(&many).unwrap();
        for i in 0..(MAX_WALK_ENTRIES + 50) {
            std::fs::write(many.join(format!("f{i}")), "").unwrap();
        }
        assert!(walk_workspace(&root).len() <= MAX_WALK_ENTRIES);
        let _ = std::fs::remove_dir_all(&root);
    }
}
