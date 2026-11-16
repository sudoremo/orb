use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: Option<i64>,
    pub account_name: String,
    pub name: String,

    // Sync state fields (merged from sync_state table)
    pub uidvalidity: Option<u32>,
    pub highest_uid: Option<u32>,
    pub highest_modseq: Option<u64>,
    pub last_sync_time: Option<String>,
    pub sync_version: i64,
}

impl Folder {
    pub fn new(account_name: String, name: String) -> Self {
        Self {
            id: None,
            account_name,
            name,
            uidvalidity: None,
            highest_uid: None,
            highest_modseq: None,
            last_sync_time: None,
            sync_version: 0,
        }
    }

    /// Update sync state after successful sync
    pub fn update_sync_state(
        &mut self,
        uidvalidity: u32,
        highest_uid: u32,
        highest_modseq: u64,
        sync_time: String,
    ) {
        self.uidvalidity = Some(uidvalidity);
        self.highest_uid = Some(highest_uid);
        self.highest_modseq = Some(highest_modseq);
        self.last_sync_time = Some(sync_time);
    }

    /// Check if UIDVALIDITY changed (requires full resync)
    pub fn uidvalidity_changed(&self, new_uidvalidity: u32) -> bool {
        match self.uidvalidity {
            Some(current) => current != new_uidvalidity,
            None => false, // First sync, not a "change"
        }
    }
}

/// Folder with display information for UI rendering
/// Combines the folder data with computed display properties
#[derive(Debug, Clone, Serialize)]
pub struct FolderDisplay {
    /// The underlying folder (None for account headers)
    pub folder: Option<Folder>,
    /// Display name (may be overridden by config)
    pub display_name: String,
    /// Whether to show unread count badge
    pub show_unread: bool,
    /// Whether to show total count badge
    pub show_total: bool,
    /// Number of unread messages
    pub unread_count: u32,
    /// Total number of messages
    pub total_count: u32,
    /// Account name this folder belongs to (for multi-account display)
    pub account_name: String,
    /// True if this is an account header row (not a real folder)
    pub is_account_header: bool,
    /// True if this folder is pinned to the top of the list
    pub is_pinned: bool,
    /// Whether the folder opens as conversations.
    pub is_threaded: bool,
    /// The list's columns in this folder, when the folder has its own.
    pub columns: Option<Vec<String>>,
    /// What the folder is for — `inbox`, `drafts`, `sent`, `archive`, `junk`
    /// or `trash` — by the server's special-use flag or the account's config.
    pub role: Option<String>,
    /// How many of the folders listed above it it sits inside.
    pub depth: u32,
    /// The server's name of the folder it sits inside, when that is listed.
    pub parent: Option<String>,
    /// The name below the parent: `2025` for `Archive/2025`.
    pub label: String,
    /// The order the folder opens in: its own `sort`, else the account's
    /// `default_sort`.
    pub sort: crate::store::MessageSort,
}

impl FolderDisplay {
    /// Create a new FolderDisplay with the given properties
    pub fn new(
        folder: Folder,
        display_name: String,
        show_unread: bool,
        show_total: bool,
        unread_count: u32,
        total_count: u32,
    ) -> Self {
        let account_name = folder.account_name.clone();
        Self {
            folder: Some(folder),
            label: display_name.clone(),
            display_name,
            show_unread,
            show_total,
            unread_count,
            total_count,
            account_name,
            is_account_header: false,
            is_pinned: false,
            is_threaded: false,
            columns: None,
            role: None,
            depth: 0,
            parent: None,
            sort: crate::store::MessageSort::DateNewest,
        }
    }

    /// Create a pinned folder display for the top of the folder list
    pub fn pinned(
        folder: Folder,
        display_name: String,
        show_unread: bool,
        show_total: bool,
        unread_count: u32,
        total_count: u32,
    ) -> Self {
        let account_name = folder.account_name.clone();
        Self {
            folder: Some(folder),
            label: display_name.clone(),
            display_name,
            show_unread,
            show_total,
            unread_count,
            total_count,
            account_name,
            is_account_header: false,
            is_pinned: true,
            is_threaded: false,
            columns: None,
            role: None,
            depth: 0,
            parent: None,
            sort: crate::store::MessageSort::DateNewest,
        }
    }

    /// Create an account header row for multi-account folder list
    pub fn account_header(account_name: &str) -> Self {
        Self {
            folder: None,
            label: account_name.to_string(),
            display_name: account_name.to_string(),
            show_unread: false,
            show_total: false,
            unread_count: 0,
            total_count: 0,
            account_name: account_name.to_string(),
            is_account_header: true,
            is_pinned: false,
            is_threaded: false,
            columns: None,
            role: None,
            depth: 0,
            parent: None,
            sort: crate::store::MessageSort::DateNewest,
        }
    }

    /// Get the actual folder name (not the display name)
    /// Returns empty string for account headers
    pub fn name(&self) -> &str {
        self.folder.as_ref().map(|f| f.name.as_str()).unwrap_or("")
    }

    /// Check if this is a real folder (not an account header)
    pub fn is_folder(&self) -> bool {
        !self.is_account_header
    }
}

/// A folder's place in the sidebar's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nested {
    pub name: String,
    pub depth: u32,
    pub parent: Option<String>,
    /// The part of the name below the parent, or the whole name at the top.
    pub leaf: String,
}

/// The folders as a tree, each directly after its parent: siblings keep the
/// order they are given in, and a folder whose parent is not listed — a
/// `\Noselect` node like `[Gmail]`, or one the config hides — sits under the
/// nearest ancestor that is, or at the top.
///
/// A server that keeps every folder under `INBOX` (`INBOX.Sent`, the
/// Courier and Cyrus layout) nests nothing under it: that is a namespace,
/// not a folder somebody filed others into.
pub fn nest_folders(names: &[String], delimiter: Option<&str>) -> Vec<Nested> {
    let Some(delimiter) = delimiter.filter(|d| !d.is_empty()) else {
        return names
            .iter()
            .map(|name| Nested {
                name: name.clone(),
                depth: 0,
                parent: None,
                leaf: name.clone(),
            })
            .collect();
    };
    let listed: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
    let namespaced = names
        .iter()
        .filter(|n| !n.eq_ignore_ascii_case("INBOX"))
        .all(|n| {
            n.split_once(delimiter)
                .is_some_and(|(root, _)| root.eq_ignore_ascii_case("INBOX"))
        })
        && names.len() > 1;
    let parent_of = |name: &str| -> Option<String> {
        let mut end = name.len();
        while let Some(at) = name[..end].rfind(delimiter) {
            let candidate = &name[..at];
            if listed.contains(candidate) {
                if namespaced && candidate.eq_ignore_ascii_case("INBOX") {
                    return None;
                }
                return Some(candidate.to_string());
            }
            end = at;
        }
        None
    };
    let parents: Vec<Option<String>> = names.iter().map(|n| parent_of(n)).collect();
    let leaves: Vec<String> = names
        .iter()
        .zip(&parents)
        .map(|(name, parent)| match parent {
            Some(p) => name[p.len() + delimiter.len()..].to_string(),
            None if namespaced && !name.eq_ignore_ascii_case("INBOX") => {
                name[name.find(delimiter).map_or(0, |at| at + delimiter.len())..].to_string()
            }
            None => name.clone(),
        })
        .collect();
    let mut out = Vec::with_capacity(names.len());
    let mut stack: Vec<(Option<&str>, u32, usize)> = vec![(None, 0, 0)];
    // Depth first, without recursion: each frame is a parent, its depth and
    // where in `names` the scan for its next child resumes.
    while let Some((parent, depth, from)) = stack.pop() {
        let Some(at) = (from..names.len()).find(|&i| parents[i].as_deref() == parent) else {
            continue;
        };
        out.push(Nested {
            name: names[at].clone(),
            depth,
            parent: parents[at].clone(),
            leaf: leaves[at].clone(),
        });
        stack.push((parent, depth, at + 1));
        stack.push((Some(names[at].as_str()), depth + 1, 0));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn shape(nested: &[Nested]) -> Vec<(String, u32, Option<String>, String)> {
        nested
            .iter()
            .map(|n| (n.name.clone(), n.depth, n.parent.clone(), n.leaf.clone()))
            .collect()
    }

    #[test]
    fn children_follow_their_parent_whatever_the_sort_put_between() {
        // A plain sort puts `Archive 2` between `Archive` and `Archive/2025`.
        let nested = nest_folders(
            &names(&[
                "Archive",
                "Archive 2",
                "Archive/2025",
                "INBOX",
                "Projects/Orb/Design",
                "Projects/Orb",
                "Projects",
            ]),
            Some("/"),
        );
        assert_eq!(
            shape(&nested),
            vec![
                ("Archive".into(), 0, None, "Archive".into()),
                (
                    "Archive/2025".into(),
                    1,
                    Some("Archive".into()),
                    "2025".into()
                ),
                ("Archive 2".into(), 0, None, "Archive 2".into()),
                ("INBOX".into(), 0, None, "INBOX".into()),
                ("Projects".into(), 0, None, "Projects".into()),
                (
                    "Projects/Orb".into(),
                    1,
                    Some("Projects".into()),
                    "Orb".into()
                ),
                (
                    "Projects/Orb/Design".into(),
                    2,
                    Some("Projects/Orb".into()),
                    "Design".into()
                ),
            ]
        );
    }

    #[test]
    fn a_folder_whose_parent_is_not_listed_sits_under_the_nearest_ancestor_that_is() {
        let nested = nest_folders(
            &names(&["INBOX", "[Gmail]/Sent Mail", "A", "A/B/C"]),
            Some("/"),
        );
        assert_eq!(
            shape(&nested),
            vec![
                ("INBOX".into(), 0, None, "INBOX".into()),
                (
                    "[Gmail]/Sent Mail".into(),
                    0,
                    None,
                    "[Gmail]/Sent Mail".into()
                ),
                ("A".into(), 0, None, "A".into()),
                ("A/B/C".into(), 1, Some("A".into()), "B/C".into()),
            ]
        );
    }

    #[test]
    fn a_namespace_under_inbox_is_not_a_level() {
        let nested = nest_folders(
            &names(&["INBOX", "INBOX.Sent", "INBOX.Work", "INBOX.Work.2025"]),
            Some("."),
        );
        assert_eq!(
            shape(&nested),
            vec![
                ("INBOX".into(), 0, None, "INBOX".into()),
                ("INBOX.Sent".into(), 0, None, "Sent".into()),
                ("INBOX.Work".into(), 0, None, "Work".into()),
                (
                    "INBOX.Work.2025".into(),
                    1,
                    Some("INBOX.Work".into()),
                    "2025".into()
                ),
            ]
        );
        // One folder beside the inbox: then the inbox's children are its own.
        let nested = nest_folders(&names(&["INBOX", "INBOX/Later", "Sent"]), Some("/"));
        assert_eq!(nested[1].depth, 1);
    }

    #[test]
    fn without_a_delimiter_everything_is_at_the_top() {
        let nested = nest_folders(&names(&["Archive", "Archive/2025"]), None);
        assert!(nested.iter().all(|n| n.depth == 0 && n.parent.is_none()));
    }
}
