//! Mail templates: YAML files in a folder, each a name, a subject and a body
//! with placeholders in braces.
//!
//! ```yaml
//! name: Einladung zum internen Workshop
//! subject: Vorbereitung auf den Workshop
//! accounts: [Work]
//! body: |
//!   Lieber {Vorname}
//!
//!   Für den Workshop vom {Datum} …
//! ```
//!
//! A placeholder is a word in single or double braces — `{Vorname}`,
//! `{{Github URL}}` — and the word is its label; either is written as
//! `{Vorname}` in the draft. Braces around anything else are text. Orb reads
//! the folder each
//! time the picker opens, so a template written in any editor is there the
//! next time without telling Orb.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A template file as it is written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    name: String,
    #[serde(default)]
    subject: Option<String>,
    /// The accounts it is offered in; every account when empty.
    #[serde(default)]
    accounts: Vec<String>,
    body: String,
}

/// A template, as the picker lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    pub subject: Option<String>,
    pub accounts: Vec<String>,
    /// As written.
    pub body: String,
    pub path: String,
    /// Its placeholders, subject first, in the order they first appear.
    pub placeholders: Vec<String>,
}

/// A file in the folder that is not a template Orb can use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateProblem {
    pub path: String,
    pub message: String,
}

/// What the folder holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateFolder {
    pub templates: Vec<Template>,
    pub problems: Vec<TemplateProblem>,
}

/// Where a placeholder stands in expanded text: a line, and its span on that
/// line in bytes (for Neovim) and in UTF-16 units (for a text field).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub name: String,
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub start16: usize,
    pub end16: usize,
}

/// Text with its placeholders marked, each written as `{Label}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expanded {
    pub lines: Vec<String>,
    pub marks: Vec<Mark>,
}

/// One piece of template text.
#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Text(&'a str),
    Placeholder(&'a str),
}

/// Longest label a placeholder may have; a longer run in braces is text.
const MAX_LABEL: usize = 40;

/// Whether `label` names a placeholder: it starts with a letter and holds
/// letters, digits, spaces, `_`, `-` and `.` only — so code in braces, or a
/// JSON object, stays text.
fn is_label(label: &str) -> bool {
    let mut chars = label.chars();
    chars.next().is_some_and(char::is_alphabetic)
        && label.chars().count() <= MAX_LABEL
        && !label.ends_with(' ')
        && label
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'))
}

/// The label `tail` opens with in `open` … `close` braces, and how long the
/// whole placeholder is.
fn braced_label<'a>(tail: &'a str, open: &str, close: &str) -> Option<(&'a str, usize)> {
    let inner = tail.strip_prefix(open)?;
    let end = inner.find(['{', '}'])?;
    (inner[end..].starts_with(close) && is_label(&inner[..end]))
        .then(|| (&inner[..end], open.len() + end + close.len()))
}

/// `line` as text and placeholders.
fn pieces(line: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let Some(at) = rest.find('{') else {
            out.push(Piece::Text(rest));
            break;
        };
        if at > 0 {
            out.push(Piece::Text(&rest[..at]));
        }
        let tail = &rest[at..];
        if let Some((label, len)) =
            braced_label(tail, "{{", "}}").or_else(|| braced_label(tail, "{", "}"))
        {
            out.push(Piece::Placeholder(label));
            rest = &tail[len..];
            continue;
        }
        out.push(Piece::Text(&tail[..1]));
        rest = &tail[1..];
    }
    out
}

/// `text` with every placeholder marked, written as `{Label}` where it stands
/// however many braces it was written in.
pub fn expand(text: &str) -> Expanded {
    let mut expanded = Expanded::default();
    for (index, line) in text.split('\n').enumerate() {
        let mut out = String::new();
        for piece in pieces(line) {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::Placeholder(name) => {
                    let start = out.len();
                    let start16 = out.encode_utf16().count();
                    out.push('{');
                    out.push_str(name);
                    out.push('}');
                    expanded.marks.push(Mark {
                        name: name.to_string(),
                        line: index,
                        start,
                        end: out.len(),
                        start16,
                        end16: out.encode_utf16().count(),
                    });
                }
            }
        }
        expanded.lines.push(out);
    }
    expanded
}

/// The placeholders of `texts`, in the order they first appear.
pub fn placeholders<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for text in texts {
        for line in text.split('\n') {
            for piece in pieces(line) {
                if let Piece::Placeholder(name) = piece
                    && !names.iter().any(|n| n == name)
                {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// `body` without the blank lines it starts and ends with: it goes where the
/// cursor is, and the message around it has its own spacing.
fn trim_blank_lines(body: &str) -> &str {
    let lines: Vec<&str> = body.split('\n').collect();
    let first = lines.iter().position(|l| !l.trim().is_empty());
    let last = lines.iter().rposition(|l| !l.trim().is_empty());
    match (first, last) {
        (Some(first), Some(last)) => {
            let start: usize = lines[..first].iter().map(|l| l.len() + 1).sum();
            let end: usize = start
                + lines[first..=last]
                    .iter()
                    .map(|l| l.len() + 1)
                    .sum::<usize>()
                - 1;
            &body[start..end]
        }
        _ => "",
    }
}

/// The folder templates are read from: `templates_dir` when the config sets
/// it, otherwise `templates` beside the config file.
pub fn folder(templates_dir: Option<&str>, config_path: &str) -> PathBuf {
    match templates_dir {
        Some(dir) => PathBuf::from(shellexpand::tilde(dir).as_ref()),
        None => PathBuf::from(shellexpand::tilde(config_path).as_ref())
            .parent()
            .map(|p| p.join("templates"))
            .unwrap_or_else(|| PathBuf::from("templates")),
    }
}

fn read(path: &Path) -> Result<Template, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file: TemplateFile = serde_saphyr::from_str(&text).map_err(|e| e.to_string())?;
    if file.name.trim().is_empty() {
        return Err("`name` is empty".into());
    }
    let body = trim_blank_lines(&file.body).to_string();
    let subject = file.subject.filter(|s| !s.trim().is_empty());
    let placeholders = placeholders(subject.iter().map(String::as_str).chain([body.as_str()]));
    Ok(Template {
        name: file.name,
        subject,
        accounts: file.accounts,
        body,
        path: path.display().to_string(),
        placeholders,
    })
}

/// One template, read again from its file.
pub fn load_one(path: &Path) -> Result<Template, String> {
    read(path)
}

/// Every template in `dir` — the `.yml` and `.yaml` files directly in it —
/// by name, and what kept the others from being read. A folder that does not
/// exist holds none.
pub fn load(dir: &Path) -> TemplateFolder {
    let mut folder = TemplateFolder::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return folder;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    e.eq_ignore_ascii_case("yml") || e.eq_ignore_ascii_case("yaml")
                })
        })
        .collect();
    paths.sort();
    for path in paths {
        match read(&path) {
            Ok(template) => folder.templates.push(template),
            Err(message) => folder.problems.push(TemplateProblem {
                path: path.display().to_string(),
                message,
            }),
        }
    }
    folder.templates.sort_by_key(|t| t.name.to_lowercase());
    folder
}

/// The templates `account` may use: its own first, then those for every
/// account, each group by name.
pub fn for_account(folder: TemplateFolder, account: &str) -> TemplateFolder {
    let (mut own, mut every): (Vec<Template>, Vec<Template>) = folder
        .templates
        .into_iter()
        .filter(|t| t.accounts.is_empty() || t.accounts.iter().any(|a| a == account))
        .partition(|t| !t.accounts.is_empty());
    own.append(&mut every);
    TemplateFolder {
        templates: own,
        problems: folder.problems,
    }
}

/// Labels Orb fills with the first recipient's first name.
const FIRST_NAME: &[&str] = &[
    "vorname",
    "firstname",
    "first name",
    "first_name",
    "prénom",
    "prenom",
    "nome",
];

/// Labels Orb fills with the first recipient's whole name.
const FULL_NAME: &[&str] = &["name", "full name", "fullname"];

/// The display name of an address as written in a To line, when it has one.
fn display_name(address: &str) -> Option<String> {
    let name = address[..address.rfind('<')?].trim();
    let name = name.trim_matches('"').trim();
    (!name.is_empty() && !name.contains('@')).then(|| name.to_string())
}

/// What Orb knows for the placeholders `names` of a message to `to`: the
/// first recipient's names, for the labels that ask for them.
pub fn suggestions(names: &[String], to: &[String]) -> HashMap<String, String> {
    let Some(full) = to.first().and_then(|a| display_name(a)) else {
        return HashMap::new();
    };
    let first = full.split_whitespace().next().unwrap_or(&full).to_string();
    names
        .iter()
        .filter_map(|name| {
            let label = name.to_lowercase();
            if FIRST_NAME.contains(&label.as_str()) {
                Some((name.clone(), first.clone()))
            } else if FULL_NAME.contains(&label.as_str()) {
                Some((name.clone(), full.clone()))
            } else {
                None
            }
        })
        .collect()
}

/// A placeholder still in braces, and the line of the body it stands on
/// (counted from 1); `None` for the subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPlaceholder {
    pub name: String,
    pub line: Option<usize>,
}

/// The placeholders of `names` still written in braces: in the subject, or in
/// the body outside its quoted lines, each once, where it first stands.
pub fn open_placeholders(subject: &str, body: &str, names: &[String]) -> Vec<OpenPlaceholder> {
    names
        .iter()
        .filter_map(|name| {
            let written = format!("{{{name}}}");
            if subject.contains(&written) {
                return Some(OpenPlaceholder {
                    name: name.clone(),
                    line: None,
                });
            }
            body.split('\n')
                .position(|line| !line.starts_with('>') && line.contains(&written))
                .map(|index| OpenPlaceholder {
                    name: name.clone(),
                    line: Some(index + 1),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_in_single_or_double_braces_is_a_placeholder() {
        let line = "{{Anrede}} {Vorname}, {not a } {} {{a: 1}} {{{x}}} }}";
        let found: Vec<&str> = pieces(line)
            .into_iter()
            .filter_map(|p| match p {
                Piece::Placeholder(name) => Some(name),
                Piece::Text(_) => None,
            })
            .collect();
        assert_eq!(found, vec!["Anrede", "Vorname", "x"]);
        assert_eq!(
            expand(line).lines,
            vec!["{Anrede} {Vorname}, {not a } {} {{a: 1}} {{x}} }}"]
        );
        assert_eq!(
            placeholders(["{Github URL} and {a: 1} and {1st}", "{Datum} {Github URL}"]),
            vec!["Github URL", "Datum"]
        );
    }

    #[test]
    fn expanded_text_marks_each_placeholder_in_bytes_and_utf16() {
        let expanded = expand("Grüsse {Vorname} 👋 {{Datum}}\n{{a: 1}}");
        assert_eq!(
            expanded.lines,
            vec!["Grüsse {Vorname} 👋 {Datum}", "{{a: 1}}"]
        );
        let datum = &expanded.marks[1];
        assert_eq!(&expanded.lines[0][datum.start..datum.end], "{Datum}");
        let utf16: Vec<u16> = expanded.lines[0].encode_utf16().collect();
        assert_eq!(
            String::from_utf16(&utf16[datum.start16..datum.end16]).unwrap(),
            "{Datum}"
        );
        assert_eq!(expanded.marks.len(), 2);
    }

    #[test]
    fn a_template_is_read_with_its_blank_lines_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("workshop.yml"),
            "name: Workshop\nsubject: Am {Datum}\naccounts: [Work]\nbody: |\n\n  Lieber {Vorname}\n\n  Bis {Datum}\n\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("broken.yml"), "name: [\n").unwrap();
        std::fs::write(dir.path().join("all.yaml"), "name: Thanks\nbody: Thanks!\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a template").unwrap();

        let folder = load(dir.path());
        assert_eq!(folder.problems.len(), 1);
        assert!(folder.problems[0].path.ends_with("broken.yml"));
        let names: Vec<&str> = folder.templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Thanks", "Workshop"]);
        let workshop = &folder.templates[1];
        assert_eq!(workshop.body, "Lieber {Vorname}\n\nBis {Datum}");
        assert_eq!(workshop.placeholders, vec!["Datum", "Vorname"]);

        let personal = for_account(folder.clone(), "Personal");
        assert_eq!(personal.templates.len(), 1);
        let work = for_account(folder, "Work");
        let names: Vec<&str> = work.templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Workshop", "Thanks"]);
        assert_eq!(load(&dir.path().join("missing")), TemplateFolder::default());
    }

    #[test]
    fn the_folder_is_beside_the_config_unless_set() {
        assert_eq!(
            folder(None, "/home/me/.config/orb/config.yml"),
            PathBuf::from("/home/me/.config/orb/templates")
        );
        assert_eq!(
            folder(Some("/srv/t"), "/x/config.yml"),
            PathBuf::from("/srv/t")
        );
    }

    #[test]
    fn names_come_from_the_first_recipient() {
        let names: Vec<String> = ["Vorname", "Name", "Datum"].map(String::from).to_vec();
        let to = vec!["\"Jonas Keller\" <jonas@example.com>".to_string()];
        let suggested = suggestions(&names, &to);
        assert_eq!(suggested.get("Vorname").map(String::as_str), Some("Jonas"));
        assert_eq!(
            suggested.get("Name").map(String::as_str),
            Some("Jonas Keller")
        );
        assert_eq!(suggested.get("Datum"), None);
        assert!(suggestions(&names, &["jonas@example.com".to_string()]).is_empty());
        assert!(suggestions(&names, &[]).is_empty());
    }

    #[test]
    fn open_placeholders_are_found_outside_the_quote() {
        let names: Vec<String> = ["Datum", "Url", "Vorname"].map(String::from).to_vec();
        let body = "Lieber Jonas\n\nAm {Datum}\n\n> {Url}\n";
        assert_eq!(
            open_placeholders("Re: Workshop", body, &names),
            vec![OpenPlaceholder {
                name: "Datum".into(),
                line: Some(3)
            }]
        );
        assert_eq!(open_placeholders("Am {Datum}", body, &names)[0].line, None);
    }
}
