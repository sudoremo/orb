//! Building the draft a reply or forward starts from.
//!
//! This lives in the core rather than in the window because everything it needs
//! is here: the reply settings, the identity's signature, and the message being
//! answered. Assembling it in the interface would mean shipping the
//! configuration there and keeping the two spellings of "how a reply looks" in
//! step.

use crate::config::{IdentityConfig, ReplyConfig, SignaturePlacement, bare_address};

/// A draft, ready to be opened in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DraftTemplate {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    /// The Message-ID of the message a forward forwards.
    #[serde(default)]
    pub forwards: Option<String>,
    /// Files to go out with it, as paths — a forward carries the original's.
    pub attachments: Vec<String>,
    /// The name of the identity it is written as.
    pub identity: String,
    /// A saved draft that could not be read in full — its Bcc or its
    /// attachments were out of reach — and so must not replace the copy on
    /// the server, which still has them.
    #[serde(default)]
    pub incomplete: bool,
    /// For a reply or forward, every address the original names, as
    /// [`WarningFacts::addressed_to`] takes them.
    #[serde(default)]
    pub addressed_to: Vec<String>,
    /// For a reply or forward, the original's Delivered-To.
    #[serde(default)]
    pub delivered_to: Vec<String>,
}

/// The text of a message, whatever form it arrived in.
///
/// A message with no plain-text part is read from its HTML. Quoting the markup
/// instead would fill a reply with tags, and refusing to quote at all would
/// leave the reader wondering what was being answered — and HTML-only mail is
/// common enough that either would be a daily annoyance.
pub fn readable_text(body_text: Option<&str>, body_html: Option<&str>) -> String {
    if let Some(text) = body_text.map(str::trim).filter(|t| !t.is_empty()) {
        return text.to_string();
    }
    let Some(html) = body_html.filter(|h| !h.trim().is_empty()) else {
        return String::new();
    };

    // Rendered at a width a quote can live inside: what comes back is wrapped,
    // and quoting adds a prefix to every line, so rendering to the full width
    // would push the result past where anything else wraps.
    html2text::from_read(html.as_bytes(), QUOTE_WIDTH)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Width the HTML of a message is rendered to before quoting.
const QUOTE_WIDTH: usize = 72;

/// Prefix every line, the way a reply quotes.
///
/// A line that is already quoted gets the prefix without the space, so nesting
/// reads as `>> ` rather than `> > ` and stays legible several replies deep.
pub fn quote(text: &str, prefix: &str) -> String {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(|line| {
            if line.starts_with('>') {
                format!("{}{line}", prefix.trim_end())
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fill in the attribution line.
pub fn attribution(template: &str, sender: Option<&str>, date: Option<&str>) -> String {
    template
        .replace("{sender}", sender.unwrap_or("someone"))
        .replace("{date}", date.unwrap_or("an earlier date"))
}

/// The signature, as it follows a line of a draft: one blank line, then the
/// signature.
///
/// Separated by the standard `-- ` marker unless the file already provides one,
/// so that a reader's client can recognise and fold it. The trailing space is
/// part of the convention and is easy to lose, which is why it is added here
/// rather than left to whoever writes the file.
pub fn signature_block(identity: &IdentityConfig) -> String {
    let Some(signature) = identity.get_signature() else {
        return String::new();
    };
    let signature = signature.trim_end_matches('\n');
    if signature.trim().is_empty() {
        return String::new();
    }
    if signature.starts_with("-- \n") {
        format!("\n{signature}\n")
    } else if let Some(rest) = signature.strip_prefix("--\n") {
        format!("\n-- \n{rest}\n")
    } else {
        format!("\n-- \n{signature}\n")
    }
}

/// The body a new message starts with: the line to write on, and the
/// signature.
pub fn new_body(identity: &IdentityConfig) -> String {
    format!("\n{}", signature_block(identity))
}

/// The body a reply starts with: room to write, then the quote and the
/// signature, in the order `reply.signature` asks for.
///
/// The cursor belongs at the top, which is why the blank lines come first: a
/// reply is written above what it answers.
pub fn reply_body(
    reply: &ReplyConfig,
    identity: &IdentityConfig,
    sender: Option<&str>,
    date: Option<&str>,
    body_text: Option<&str>,
    body_html: Option<&str>,
) -> String {
    let mut quoted = String::new();
    if reply.quote {
        let text = readable_text(body_text, body_html);
        if !text.is_empty() {
            let line = attribution(&reply.attribution, sender, date);
            if !line.trim().is_empty() {
                quoted.push_str(&format!("\n{line}\n"));
            } else {
                quoted.push('\n');
            }
            quoted.push_str(&quote(&text, &reply.prefix));
            quoted.push('\n');
        }
    }

    let signature = signature_block(identity);
    match reply.signature {
        SignaturePlacement::Below => format!("\n{quoted}{signature}"),
        SignaturePlacement::Above => format!("\n{signature}{quoted}"),
    }
}

/// The body a forward starts with.
///
/// The original is reproduced with its headers rather than quoted: a forward
/// passes a message on, so what it looked like is part of what is being sent.
pub fn forward_body(
    identity: &IdentityConfig,
    from: Option<&str>,
    date: Option<&str>,
    subject: Option<&str>,
    to: Option<&str>,
    body_text: Option<&str>,
    body_html: Option<&str>,
) -> String {
    let text = readable_text(body_text, body_html);
    let mut out = String::from("\n");
    out.push_str(&signature_block(identity));
    out.push_str(&format!("\n{FORWARDED}\n"));
    out.push_str(&format!("From: {}\n", from.unwrap_or("")));
    out.push_str(&format!("Date: {}\n", date.unwrap_or("")));
    out.push_str(&format!("Subject: {}\n", subject.unwrap_or("")));
    out.push_str(&format!("To: {}\n\n", to.unwrap_or("")));
    out.push_str(&text);
    out.push('\n');
    out
}

/// The line a forwarded message starts below.
const FORWARDED: &str = "---------- Forwarded message ----------";

/// Put `new`'s signature where `old`'s is, in a body written as `old`.
///
/// The old signature is found by its text, so whatever was written around it
/// stays as it is. A body without it gets the new one in the place a fresh
/// draft would have had it — unless it already has a `-- ` line of its own,
/// which is a signature somebody wrote or kept on purpose and which a second
/// one would only duplicate.
pub fn swap_signature(
    body: &str,
    old: &IdentityConfig,
    new: &IdentityConfig,
    placement: SignaturePlacement,
) -> String {
    let old_block = signature_block(old);
    let new_block = signature_block(new);
    let old_text = old_block.trim_matches('\n');
    let new_text = new_block.trim_matches('\n');

    if !old_text.is_empty()
        && let Some(at) = body.find(old_text)
    {
        let (before, after) = (&body[..at], &body[at + old_text.len()..]);
        if !new_text.is_empty() {
            return format!("{before}{new_text}{after}");
        }
        // No signature to put in its place: the blank lines that set the old
        // one off go with it.
        return format!(
            "{}\n{}",
            before.trim_end_matches('\n'),
            after.trim_start_matches('\n')
        );
    }

    if new_text.is_empty() || has_signature_line(body) {
        return body.to_string();
    }

    let lines: Vec<&str> = body.split('\n').collect();
    // A forward's signature precedes the forwarded message wherever a
    // reply's goes, as it does in a fresh forward.
    let anchor = match placement {
        SignaturePlacement::Below => lines.iter().position(|line| *line == FORWARDED),
        SignaturePlacement::Above => quote_start(&lines),
    };
    match anchor {
        Some(index) => {
            let before = lines[..index].join("\n");
            let after = lines[index..].join("\n");
            format!("{}\n{new_block}\n{after}", before.trim_end_matches('\n'))
        }
        None => format!("{}\n{new_block}", body.trim_end_matches('\n')),
    }
}

/// Whether the body has a signature marker outside the quote.
fn has_signature_line(body: &str) -> bool {
    body.split('\n').any(|line| line == "-- " || line == "--")
}

/// The line a reply's quote begins on: its attribution when it has one, or a
/// forwarded message's header line.
fn quote_start(lines: &[&str]) -> Option<usize> {
    let first = lines
        .iter()
        .position(|line| line.starts_with('>') || *line == FORWARDED)?;
    match first.checked_sub(1) {
        Some(previous) if lines[first] != FORWARDED && !lines[previous].trim().is_empty() => {
            Some(previous)
        }
        _ => Some(first),
    }
}

/// The identity a reply or forward is written as: the first one among the
/// lists in `addressed_to`, and the account's default identity (the first)
/// when none of them names one.
pub fn choose_identity<'a>(
    identities: &'a [IdentityConfig],
    addressed_to: &[Option<&str>],
) -> Option<&'a IdentityConfig> {
    addressed_to
        .iter()
        .flat_map(|list| split_addresses(*list))
        .find_map(|address| identities.iter().find(|i| i.owns(&address)))
        .or_else(|| identities.first())
}

/// Something about a draft worth a second look before it is sent, but no
/// reason to refuse sending it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct DraftWarning {
    /// `identity`, `unowned`, `domain`, `attachment` or `placeholder`.
    pub kind: &'static str,
    pub message: String,
    /// The placeholder still in braces, for a `placeholder` warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// The line of the body it stands on, from 1; absent for the subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

/// An identity and the account it goes out of.
#[derive(Debug, Clone)]
pub struct Sender {
    pub account: String,
    pub identity: IdentityConfig,
}

impl Sender {
    fn is(&self, other: &Sender) -> bool {
        self.account == other.account && self.identity.name == other.identity.name
    }

    /// The address alone, which is what tells two senders apart on screen.
    fn address(&self) -> String {
        bare_address(&self.identity.from)
    }
}

/// What a draft is checked against: where the original went, and who it is
/// for now.
#[derive(Debug, Clone, Default)]
pub struct WarningFacts<'a> {
    /// Every address the original names — From, To, Cc and Delivered-To —
    /// in the order a reply's identity is chosen by.
    pub addressed_to: &'a [String],
    /// The original's Delivered-To alone: the addresses it was delivered to
    /// on this side, which To and Cc may not name.
    pub delivered_to: &'a [String],
    pub recipients: &'a [String],
}

/// The warnings about who a draft is from.
///
/// A reply is checked against the original: written as a sender that was
/// not addressed while another one was, or answering mail delivered to an
/// address no sender owns, which a reply then goes out from the default for.
/// Every draft is checked against the `domains` of the senders.
pub fn sender_warnings(
    senders: &[Sender],
    current: &Sender,
    facts: &WarningFacts,
) -> Vec<DraftWarning> {
    let mut warnings = Vec::new();

    // Against the original
    let owner = |address: &str| senders.iter().find(|s| s.identity.owns(address));
    let addressed_current = facts.addressed_to.iter().any(|a| current.identity.owns(a));
    if !addressed_current {
        let addressed = facts
            .addressed_to
            .iter()
            .find_map(|address| owner(address).map(|sender| (address, sender)));
        match addressed {
            Some((address, sender)) => warnings.push(DraftWarning {
                kind: "identity",
                message: format!(
                    "The original went to {} ({}), but this is from {}",
                    bare_address(address),
                    sender.identity.name,
                    current.address()
                ),
                ..Default::default()
            }),
            None => {
                let unowned: Vec<String> =
                    facts.delivered_to.iter().map(|a| bare_address(a)).collect();
                if !unowned.is_empty() {
                    warnings.push(DraftWarning {
                        kind: "unowned",
                        message: format!(
                            "The original went to {}, which no identity sends as; this is from {}",
                            unowned.join(", "),
                            current.address()
                        ),
                        ..Default::default()
                    });
                }
            }
        }
    }

    // Against the recipients' domains
    let mut elsewhere: Vec<(&Sender, Vec<String>)> = Vec::new();
    for recipient in facts.recipients {
        if current.identity.writes_to(recipient) {
            continue;
        }
        let Some(sender) = senders
            .iter()
            .find(|s| !s.is(current) && s.identity.writes_to(recipient))
        else {
            continue;
        };
        let address = bare_address(recipient);
        match elsewhere.iter_mut().find(|(s, _)| s.is(sender)) {
            Some((_, list)) => list.push(address),
            None => elsewhere.push((sender, vec![address])),
        }
    }
    for (sender, recipients) in elsewhere {
        warnings.push(DraftWarning {
            kind: "domain",
            message: format!(
                "{} {} written to as {} ({}), but this is from {}",
                recipients.join(", "),
                if recipients.len() == 1 { "is" } else { "are" },
                sender.identity.name,
                sender.address(),
                current.address()
            ),
            ..Default::default()
        });
    }
    warnings
}

/// The warning about a draft that mentions an attachment but has none.
pub fn attachment_warning(
    body: &str,
    has_attachments: bool,
    words: &[String],
) -> Option<DraftWarning> {
    if has_attachments {
        return None;
    }
    let word = mentioned_word(&own_text(body), words)?;
    Some(DraftWarning {
        kind: "attachment",
        message: format!("Mentions an attachment (“{word}”) but has none"),
        ..Default::default()
    })
}

/// A warning for each placeholder of the draft's template still written in
/// braces, in the subject or in the body outside the quote.
pub fn placeholder_warnings(subject: &str, body: &str, names: &[String]) -> Vec<DraftWarning> {
    crate::templates::open_placeholders(subject, body, names)
        .into_iter()
        .map(|open| DraftWarning {
            kind: "placeholder",
            message: match open.line {
                Some(line) => format!("{{{}}} is not filled in (line {line})", open.name),
                None => format!("{{{}}} is not filled in (subject)", open.name),
            },
            placeholder: Some(open.name),
            line: open.line,
        })
        .collect()
}

/// What the writer wrote: without quoted lines and their attribution,
/// without a signature, and without a forwarded message.
fn own_text(body: &str) -> String {
    let lines: Vec<&str> = body.split('\n').collect();
    let quoted = |index: usize| lines.get(index).is_some_and(|line| line.starts_with('>'));
    let mut own = Vec::new();
    let mut in_signature = false;
    for (index, line) in lines.iter().enumerate() {
        if *line == FORWARDED {
            break;
        }
        if *line == "-- " || *line == "--" {
            in_signature = true;
            continue;
        }
        if quoted(index) {
            // A signature above the quote ends where the quote begins.
            in_signature = false;
            continue;
        }
        let attribution = quoted(index + 1) && !line.trim().is_empty();
        if !in_signature && !attribution {
            own.push(*line);
        }
    }
    own.join("\n")
}

/// The first of `words` that starts a word in `text`, as the text has it
/// up to the end of that word.
fn mentioned_word(text: &str, words: &[String]) -> Option<String> {
    let text = text.to_lowercase();
    words
        .iter()
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty())
        .find_map(|word| {
            text.match_indices(&word).find_map(|(at, _)| {
                let starts_word = text[..at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric());
                if !starts_word {
                    return None;
                }
                let rest = &text[at + word.len()..];
                let end = rest
                    .find(|c: char| !c.is_alphanumeric())
                    .unwrap_or(rest.len());
                Some(format!("{word}{}", &rest[..end]))
            })
        })
}

/// The headers of the message being answered that decide where the reply goes
/// and how it threads.
///
/// None of them is synced — the envelope the sync stores carries no
/// `References` — so they are read from the source, or from a header block
/// the server was asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParentHeaders {
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    /// Where the sender asked for answers, as `Name <address>` entries joined
    /// by `, ` — the form every stored address takes.
    pub reply_to: Option<String>,
    /// Every `Delivered-To`, joined like `reply_to`: the address the server
    /// delivered to, which names the identity when `To` and `Cc` do not — a
    /// list, a Bcc, a forwarding alias.
    pub delivered_to: Option<String>,
    /// The `Bcc` a saved draft carries, joined like `reply_to`. Sent mail
    /// has none: the header stays with the sender.
    pub bcc: Option<String>,
    /// The message a saved forward forwards.
    pub forwards: Option<String>,
}

impl ParentHeaders {
    /// Read them off a header block, or off a whole message.
    ///
    /// Anything unreadable counts as absent: a reply with a shorter thread
    /// beats no reply at all.
    pub fn parse(raw: &[u8]) -> Self {
        use mailparse::MailHeaderMap;

        let Ok((headers, _)) = mailparse::parse_headers(raw) else {
            return Self::default();
        };
        let identifiers = |name: &str| {
            headers
                .get_first_value(name)
                .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|value| !value.is_empty())
        };
        let addresses = |name: &str| {
            headers
                .get_first_header(name)
                .and_then(|header| mailparse::addrparse_header(header).ok())
                .map(|list| {
                    list.iter()
                        .flat_map(|address| match address {
                            mailparse::MailAddr::Single(single) => vec![single.clone()],
                            mailparse::MailAddr::Group(group) => group.addrs.clone(),
                        })
                        .map(|single| {
                            mailbox_display(
                                single.display_name.as_deref().unwrap_or(""),
                                &single.addr,
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|value| !value.is_empty())
        };
        let reply_to = addresses("Reply-To");
        let bcc = addresses("Bcc");
        let delivered_to = Some(
            headers
                .get_all_values("Delivered-To")
                .iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join(", "),
        )
        .filter(|value| !value.is_empty());

        ParentHeaders {
            message_id: identifiers("Message-ID"),
            in_reply_to: identifiers("In-Reply-To"),
            references: identifiers("References"),
            reply_to,
            delivered_to,
            bcc,
            forwards: identifiers(crate::smtp::FORWARDED_HEADER),
        }
    }
}

/// The `References` of a reply, as RFC 5322 §3.6.4 has it: the parent's
/// `References` followed by the parent's Message-ID. A parent with no
/// `References` but a single `In-Reply-To` contributes that instead, so a
/// thread started by a client that only sets `In-Reply-To` still holds
/// together.
pub fn reply_references(parent: &ParentHeaders) -> Option<String> {
    let mut ids: Vec<&str> = match parent.references.as_deref() {
        Some(references) => references.split_whitespace().collect(),
        None => parent
            .in_reply_to
            .as_deref()
            .map(|id| id.split_whitespace().collect::<Vec<_>>())
            .filter(|ids| ids.len() == 1)
            .unwrap_or_default(),
    };
    if let Some(id) = parent.message_id.as_deref().map(str::trim)
        && !id.is_empty()
        && !ids.contains(&id)
    {
        ids.push(id);
    }
    if ids.is_empty() {
        None
    } else {
        Some(ids.join(" "))
    }
}

/// Who a reply goes to, as `(to, cc)`.
///
/// `Reply-To` is where the sender asked for answers, so it stands in for
/// `From`. Replying to everyone adds the original sender and recipients to
/// the Cc, minus the user's own addresses (`mine`) and minus whoever is
/// already in the To.
///
/// A message the user sent themselves is answered the way other clients do:
/// the reply goes to its `To` again, not back to the user. Only a message
/// sent to nobody but the user goes back to its sender.
pub fn reply_recipients(
    parent_from: Option<&str>,
    parent_reply_to: Option<&str>,
    parent_to: Option<&str>,
    parent_cc: Option<&str>,
    mine: &[String],
    reply_all: bool,
) -> (Vec<String>, Vec<String>) {
    let is_mine = |address: &str| mine.iter().any(|m| same_address(m, address));
    let sent_by_me = split_addresses(parent_from)
        .first()
        .is_some_and(|from| is_mine(from));
    let resent_to: Vec<String> = if sent_by_me {
        split_addresses(parent_to)
            .into_iter()
            .filter(|address| !is_mine(address))
            .collect()
    } else {
        Vec::new()
    };
    let to: Vec<String> = if resent_to.is_empty() {
        [parent_reply_to, parent_from]
            .into_iter()
            .map(split_addresses)
            .find(|list| !list.is_empty())
            .unwrap_or_default()
    } else {
        resent_to
    };

    let mut cc = Vec::new();
    if reply_all {
        for address in [parent_from, parent_to, parent_cc]
            .into_iter()
            .flat_map(split_addresses)
        {
            let already = to
                .iter()
                .chain(cc.iter())
                .any(|a| same_address(a, &address));
            if !is_mine(&address) && !already {
                cc.push(address);
            }
        }
    }
    (to, cc)
}

/// The entries of a stored address list.
///
/// A comma inside quotes or angle brackets is part of an entry, and so is
/// one before the entry has an address at all: stored lists carry display
/// names unquoted, and `Doe, John <j@example.com>` is one recipient, not a
/// recipient `Doe` and another.
pub fn split_addresses(list: Option<&str>) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut angled) = (false, false);
    for c in list.unwrap_or("").chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angled = true,
            '>' if !quoted => angled = false,
            ',' if !quoted && !angled && current.contains('@') => {
                entries.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    entries.push(current);
    entries
        .into_iter()
        .map(|a| {
            a.trim_matches(|c: char| c == ',' || c.is_whitespace())
                .to_string()
        })
        .filter(|a| !a.is_empty())
        .collect()
}

/// `Name <address>`, with the name quoted when it holds a character that
/// would otherwise split or end it.
pub fn mailbox_display(name: &str, address: &str) -> String {
    let name = name.trim();
    if name.is_empty() || name == address {
        return address.to_string();
    }
    if name.contains([',', ';', '<', '>', '"', ':', '@']) {
        format!(
            "\"{}\" <{address}>",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        )
    } else {
        format!("{name} <{address}>")
    }
}

/// Whether two `Name <address>` entries name the same mailbox.
fn same_address(a: &str, b: &str) -> bool {
    bare_address(a).eq_ignore_ascii_case(&bare_address(b))
}

/// The subject of a reply, without stacking `Re:` on every round.
pub fn reply_subject(subject: Option<&str>) -> String {
    let subject = subject.unwrap_or("").trim();
    // `get` rather than an index: a subject that starts with a character of
    // several bytes has no boundary at 3, and slicing there would panic.
    if subject
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("re:"))
    {
        subject.to_string()
    } else if subject.is_empty() {
        "Re:".to_string()
    } else {
        format!("Re: {subject}")
    }
}

/// The subject of a forward, likewise.
pub fn forward_subject(subject: Option<&str>) -> String {
    let subject = subject.unwrap_or("").trim();
    if subject
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("fwd:"))
    {
        subject.to_string()
    } else if subject.is_empty() {
        "Fwd:".to_string()
    } else {
        format!("Fwd: {subject}")
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_name_with_a_comma_stays_one_recipient() {
        assert_eq!(
            split_addresses(Some("Doe, John <j@example.com>, b@example.com")),
            vec!["Doe, John <j@example.com>", "b@example.com"]
        );
        assert_eq!(
            split_addresses(Some("\"Doe, John\" <j@example.com>,<x,y@example.com>")),
            vec!["\"Doe, John\" <j@example.com>", "<x,y@example.com>"]
        );
        assert!(split_addresses(Some(" , ")).is_empty());
        assert!(split_addresses(None).is_empty());
    }

    #[test]
    fn a_name_that_would_split_is_quoted() {
        assert_eq!(
            mailbox_display("Doe, John", "j@x.ch"),
            "\"Doe, John\" <j@x.ch>"
        );
        assert_eq!(mailbox_display("Ann", "a@x.ch"), "Ann <a@x.ch>");
        assert_eq!(mailbox_display("", "a@x.ch"), "a@x.ch");
        let parsed = ParentHeaders::parse(b"Bcc: \"Doe, John\" <j@x.ch>, b@x.ch\r\n\r\n");
        assert_eq!(
            split_addresses(parsed.bcc.as_deref()),
            vec!["\"Doe, John\" <j@x.ch>", "b@x.ch"]
        );
    }
    use super::*;
    use crate::config::ComposeConfig;

    fn identity(signature: Option<&str>) -> IdentityConfig {
        IdentityConfig {
            name: "Work".to_string(),
            from: "Me <me@example.com>".to_string(),
            signature_file: signature.map(str::to_string),
            ..IdentityConfig::default()
        }
    }

    fn inline(name: &str, from: &str, signature: Option<&str>) -> IdentityConfig {
        IdentityConfig {
            name: name.to_string(),
            from: from.to_string(),
            signature: signature.map(str::to_string),
            ..IdentityConfig::default()
        }
    }

    #[test]
    fn quoting_prefixes_every_line() {
        assert_eq!(quote("one\ntwo", "> "), "> one\n> two");
        // Including the empty ones: a gap left unquoted reads as the end of the
        // quote and the start of the reply.
        assert_eq!(quote("one\n\ntwo", "> "), "> one\n> \n> two");
    }

    #[test]
    fn quoting_nests_without_gaps() {
        // `>> ` rather than `> > `, which is what stays legible several rounds
        // deep and what every other client produces.
        assert_eq!(quote("> already", "> "), ">> already");
        assert_eq!(quote(">> twice", "> "), ">>> twice");
    }

    #[test]
    fn quoting_honours_the_configured_prefix() {
        assert_eq!(quote("one", "| "), "| one");
        assert_eq!(quote("one", ""), "one");
    }

    #[test]
    fn carriage_returns_do_not_survive_into_the_quote() {
        // Mail arrives with CRLF; leaving it in shows as a stray character in
        // an editor that expects LF.
        assert_eq!(quote("one\r\ntwo", "> "), "> one\n> two");
    }

    #[test]
    fn html_is_read_when_there_is_no_plain_text() {
        let html = "<p>Hello <b>there</b></p><p>Second paragraph</p>";
        let text = readable_text(None, Some(html));
        assert!(text.contains("Hello"), "got: {text}");
        assert!(text.contains("Second paragraph"), "got: {text}");
        // The markup itself must not survive, or the reply quotes tags.
        assert!(!text.contains("<p>"), "got: {text}");
        assert!(!text.contains("<b>"), "got: {text}");
    }

    #[test]
    fn plain_text_is_preferred_when_the_message_has_both() {
        // The sender wrote the plain part for exactly this purpose; rendering
        // their HTML instead would second-guess it.
        let text = readable_text(Some("the plain one"), Some("<p>the html one</p>"));
        assert_eq!(text, "the plain one");
    }

    #[test]
    fn an_empty_plain_part_falls_through_to_the_html() {
        // Some senders include an empty text part as a formality.
        let text = readable_text(Some("   \n  "), Some("<p>real content</p>"));
        assert!(text.contains("real content"), "got: {text}");
    }

    #[test]
    fn a_message_with_neither_quotes_nothing() {
        assert_eq!(readable_text(None, None), "");
        assert_eq!(readable_text(Some(""), Some("")), "");
    }

    #[test]
    fn the_attribution_fills_in_who_and_when() {
        assert_eq!(
            attribution("On {date}, {sender} wrote:", Some("Thomas"), Some("Monday")),
            "On Monday, Thomas wrote:"
        );
        // A message missing either still produces a sentence.
        assert_eq!(
            attribution("On {date}, {sender} wrote:", None, None),
            "On an earlier date, someone wrote:"
        );
        // And a template that mentions neither is left alone.
        assert_eq!(
            attribution("Previously:", Some("Thomas"), None),
            "Previously:"
        );
    }

    #[test]
    fn a_reply_leaves_room_to_write_above_the_quote() {
        let reply = ReplyConfig::default();
        let body = reply_body(
            &reply,
            &identity(None),
            Some("Thomas"),
            Some("Monday"),
            Some("the original"),
            None,
        );
        assert!(body.starts_with('\n'), "the cursor belongs above the quote");
        assert!(body.contains("On Monday, Thomas wrote:"));
        assert!(body.contains("> the original"));
    }

    #[test]
    fn quoting_can_be_turned_off_entirely() {
        let reply = ReplyConfig {
            quote: false,
            ..ReplyConfig::default()
        };
        let body = reply_body(
            &reply,
            &identity(None),
            Some("Thomas"),
            Some("Monday"),
            Some("the original"),
            None,
        );
        assert!(!body.contains("the original"));
        assert!(!body.contains("wrote:"));
    }

    #[test]
    fn an_empty_attribution_leaves_the_quote_to_speak_for_itself() {
        let reply = ReplyConfig {
            attribution: String::new(),
            ..ReplyConfig::default()
        };
        let body = reply_body(
            &reply,
            &identity(None),
            Some("Thomas"),
            Some("Monday"),
            Some("hello"),
            None,
        );
        assert!(!body.contains("wrote:"));
        assert!(body.contains("> hello"));
    }

    #[test]
    fn a_subject_does_not_collect_prefixes() {
        assert_eq!(reply_subject(Some("Invoice")), "Re: Invoice");
        assert_eq!(reply_subject(Some("Re: Invoice")), "Re: Invoice");
        assert_eq!(reply_subject(Some("RE: Invoice")), "RE: Invoice");
        assert_eq!(reply_subject(None), "Re:");

        assert_eq!(forward_subject(Some("Specs")), "Fwd: Specs");
        assert_eq!(forward_subject(Some("Fwd: Specs")), "Fwd: Specs");
        assert_eq!(forward_subject(Some("FWD: Specs")), "FWD: Specs");
        // A first character of several bytes ends past where the prefix would.
        assert_eq!(reply_subject(Some("Ärger im Büro")), "Re: Ärger im Büro");
        assert_eq!(reply_subject(Some("🎉 Launch")), "Re: 🎉 Launch");
        assert_eq!(forward_subject(Some("€ Rechnung")), "Fwd: € Rechnung");
        assert_eq!(forward_subject(Some("Fü")), "Fwd: Fü");
    }

    #[test]
    fn a_forward_reproduces_the_original_rather_than_quoting_it() {
        let body = forward_body(
            &identity(None),
            Some("Thomas <t@example.com>"),
            Some("Monday"),
            Some("Specs"),
            Some("me@example.com"),
            Some("the contents"),
            None,
        );
        assert!(body.contains("---------- Forwarded message ----------"));
        assert!(body.contains("From: Thomas <t@example.com>"));
        assert!(body.contains("the contents"));
        // Not quoted: a forward passes the message on rather than answering it.
        assert!(!body.contains("> the contents"));
    }

    #[test]
    fn a_signature_is_marked_off_the_way_readers_expect() {
        let file = std::env::temp_dir().join("orb-test-signature.txt");
        std::fs::write(&file, "Sam\nExample\n").unwrap();
        let block = signature_block(&identity(Some(file.to_str().unwrap())));
        // The trailing space after `--` is the convention that lets a reader's
        // client fold it, and is easy to lose.
        assert!(block.contains("\n-- \nSam\nExample"), "got: {block:?}");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_signature_that_marks_itself_off_is_not_marked_twice() {
        let file = std::env::temp_dir().join("orb-test-signature-marked.txt");
        std::fs::write(&file, "-- \nSam\n").unwrap();
        let block = signature_block(&identity(Some(file.to_str().unwrap())));
        assert_eq!(block.matches("--").count(), 1, "got: {block:?}");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_signature_marked_off_without_the_space_gets_it() {
        let file = std::env::temp_dir().join("orb-test-signature-trimmed.txt");
        std::fs::write(&file, "--\nSam\n").unwrap();
        let block = signature_block(&identity(Some(file.to_str().unwrap())));
        assert_eq!(block, "\n-- \nSam\n");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn no_signature_configured_adds_nothing() {
        assert_eq!(signature_block(&identity(None)), "");
        assert_eq!(
            signature_block(&identity(Some("/nonexistent/signature"))),
            ""
        );
    }

    #[test]
    fn a_reply_ends_with_the_signature() {
        let file = std::env::temp_dir().join("orb-test-signature-reply.txt");
        std::fs::write(&file, "Sam\n").unwrap();
        let body = reply_body(
            &ReplyConfig::default(),
            &identity(Some(file.to_str().unwrap())),
            Some("Thomas"),
            Some("Monday"),
            Some("hello"),
            None,
        );
        assert!(body.trim_end().ends_with("Sam"), "got: {body:?}");
        let _ = std::fs::remove_file(&file);
    }

    fn parent_with(
        message_id: &str,
        references: Option<&str>,
        in_reply_to: Option<&str>,
    ) -> ParentHeaders {
        ParentHeaders {
            message_id: Some(message_id.to_string()),
            in_reply_to: in_reply_to.map(str::to_string),
            references: references.map(str::to_string),
            reply_to: None,
            delivered_to: None,
            bcc: None,
            forwards: None,
        }
    }

    #[test]
    fn the_references_carry_the_whole_thread() {
        let parent = parent_with(
            "<third@example.com>",
            Some("<first@example.com> <second@example.com>"),
            Some("<second@example.com>"),
        );
        assert_eq!(
            reply_references(&parent).as_deref(),
            Some("<first@example.com> <second@example.com> <third@example.com>")
        );
    }

    #[test]
    fn a_lone_in_reply_to_stands_in_for_missing_references() {
        let parent = parent_with("<second@example.com>", None, Some("<first@example.com>"));
        assert_eq!(
            reply_references(&parent).as_deref(),
            Some("<first@example.com> <second@example.com>")
        );

        // Several identifiers in In-Reply-To say nothing about the order of
        // the thread, so RFC 5322 §3.6.4 starts the chain afresh.
        let ambiguous = parent_with(
            "<second@example.com>",
            None,
            Some("<a@example.com> <b@example.com>"),
        );
        assert_eq!(
            reply_references(&ambiguous).as_deref(),
            Some("<second@example.com>")
        );
    }

    #[test]
    fn a_thread_starter_references_only_itself() {
        assert_eq!(
            reply_references(&parent_with("<first@example.com>", None, None)).as_deref(),
            Some("<first@example.com>")
        );
        assert_eq!(reply_references(&ParentHeaders::default()), None);
    }

    #[test]
    fn the_parent_is_not_listed_twice() {
        let parent = parent_with(
            "<x@example.com>",
            Some("<a@example.com> <x@example.com>"),
            None,
        );
        assert_eq!(
            reply_references(&parent).as_deref(),
            Some("<a@example.com> <x@example.com>")
        );
    }

    #[test]
    fn headers_are_read_off_the_source_folded_or_not() {
        let source = b"Message-ID: <third@example.com>\r\n\
            In-Reply-To: <second@example.com>\r\n\
            References: <first@example.com>\r\n\
            \t<second@example.com>\r\n\
            Reply-To: =?utf-8?Q?Ren=C3=A9_M=C3=BCller?= <rene@example.com>, list@example.com\r\n\
            Subject: Hello\r\n\
            \r\n\
            Body\r\n";
        let parsed = ParentHeaders::parse(source);
        assert_eq!(parsed.message_id.as_deref(), Some("<third@example.com>"));
        assert_eq!(parsed.in_reply_to.as_deref(), Some("<second@example.com>"));
        assert_eq!(
            parsed.references.as_deref(),
            Some("<first@example.com> <second@example.com>")
        );
        assert_eq!(
            parsed.reply_to.as_deref(),
            Some("René Müller <rene@example.com>, list@example.com")
        );
    }

    #[test]
    fn a_header_block_without_the_headers_is_empty_rather_than_an_error() {
        assert_eq!(
            ParentHeaders::parse(b"Subject: Hello\r\n\r\n"),
            ParentHeaders::default()
        );
        assert_eq!(ParentHeaders::parse(b""), ParentHeaders::default());
    }

    #[test]
    fn a_reply_goes_to_the_reply_to_address() {
        let mine = vec!["Me <me@example.com>".to_string()];
        let (to, cc) = reply_recipients(
            Some("Alice <alice@example.com>"),
            Some("Alice at work <alice@work.example>"),
            Some("Me <me@example.com>, Bob <bob@example.com>"),
            None,
            &mine,
            false,
        );
        assert_eq!(to, vec!["Alice at work <alice@work.example>"]);
        assert!(cc.is_empty());

        let (to, _) = reply_recipients(
            Some("Alice <alice@example.com>"),
            None,
            None,
            None,
            &mine,
            false,
        );
        assert_eq!(to, vec!["Alice <alice@example.com>"]);
    }

    #[test]
    fn replying_to_everyone_keeps_everyone_but_me_and_lists_nobody_twice() {
        let mine = vec!["Me <me@example.com>".to_string()];
        let (to, cc) = reply_recipients(
            Some("Alice <alice@example.com>"),
            Some("list@example.com"),
            Some("ME@EXAMPLE.COM, Bob <bob@example.com>"),
            Some("list@example.com, Carol <carol@example.com>, bob@example.com"),
            &mine,
            true,
        );
        assert_eq!(to, vec!["list@example.com"]);
        assert_eq!(
            cc,
            vec![
                "Alice <alice@example.com>",
                "Bob <bob@example.com>",
                "Carol <carol@example.com>"
            ]
        );
    }

    #[test]
    fn a_reply_to_my_own_message_goes_to_its_recipients_again() {
        let mine = vec!["Me <me@example.com>".to_string()];
        let (to, cc) = reply_recipients(
            Some("Me <me@example.com>"),
            None,
            Some("Alice <alice@example.com>, me@example.com, Bob <bob@example.com>"),
            Some("Carol <carol@example.com>"),
            &mine,
            false,
        );
        assert_eq!(
            to,
            vec!["Alice <alice@example.com>", "Bob <bob@example.com>"]
        );
        assert!(cc.is_empty());

        let (to, cc) = reply_recipients(
            Some("Me <me@example.com>"),
            Some("Me <me@example.com>"),
            Some("Alice <alice@example.com>"),
            Some("Carol <carol@example.com>, ME@example.com, alice@example.com"),
            &mine,
            true,
        );
        assert_eq!(to, vec!["Alice <alice@example.com>"]);
        assert_eq!(cc, vec!["Carol <carol@example.com>"]);
    }

    #[test]
    fn a_reply_to_a_message_i_sent_only_myself_goes_back_to_me() {
        let mine = vec!["Me <me@example.com>".to_string()];
        let (to, cc) = reply_recipients(
            Some("Me <me@example.com>"),
            None,
            Some("me@example.com"),
            None,
            &mine,
            true,
        );
        assert_eq!(to, vec!["Me <me@example.com>"]);
        assert!(cc.is_empty());
    }

    fn support() -> IdentityConfig {
        IdentityConfig {
            addresses: vec!["support@example.com".into(), "help@example.com".into()],
            domains: Vec::new(),
            ..inline(
                "Support",
                "Example Support <support@example.com>",
                Some("Support"),
            )
        }
    }

    #[test]
    fn a_reply_is_written_as_the_identity_it_was_addressed_to() {
        let identities = [inline("Work", "Me <me@example.com>", None), support()];
        let chosen = |to: Option<&str>, cc: Option<&str>, delivered: Option<&str>| {
            choose_identity(&identities, &[to, cc, delivered])
                .map(|i| i.name.clone())
                .unwrap()
        };

        assert_eq!(
            chosen(Some("Help <HELP@example.com>"), None, None),
            "Support"
        );
        assert_eq!(
            chosen(Some("someone@else.com"), Some("support@example.com"), None),
            "Support"
        );
        assert_eq!(
            chosen(
                Some("list@lists.example.org"),
                None,
                Some("help@example.com")
            ),
            "Support",
            "a list or a Bcc names the identity only in Delivered-To"
        );
        assert_eq!(
            chosen(Some("me@example.com"), Some("support@example.com"), None),
            "Work",
            "To is asked before Cc"
        );
        assert_eq!(chosen(Some("someone@else.com"), None, None), "Work");
        assert_eq!(chosen(None, None, None), "Work");
    }

    #[test]
    fn the_delivered_to_headers_are_read_all_of_them() {
        let parsed = ParentHeaders::parse(
            b"Delivered-To: me@example.com\r\nDelivered-To: help@example.com\r\n\r\n",
        );
        assert_eq!(
            parsed.delivered_to.as_deref(),
            Some("me@example.com, help@example.com")
        );
    }

    #[test]
    fn the_signature_can_precede_the_quote() {
        let reply = ReplyConfig {
            signature: SignaturePlacement::Above,
            ..ReplyConfig::default()
        };
        let body = reply_body(
            &reply,
            &inline("Work", "Me <me@example.com>", Some("Sam")),
            Some("Thomas"),
            Some("Monday"),
            Some("hello"),
            None,
        );
        assert_eq!(body, "\n\n-- \nSam\n\nOn Monday, Thomas wrote:\n> hello\n");
    }

    /// The line written on, one blank line, then the signature, as
    /// `swap_signature` leaves it too.
    #[test]
    fn one_blank_line_sets_the_signature_off_in_every_fresh_draft() {
        let work = inline("Work", "Me <me@example.com>", Some("Sam"));
        assert_eq!(new_body(&work), "\n\n-- \nSam\n");
        let below = reply_body(
            &ReplyConfig::default(),
            &work,
            Some("Thomas"),
            Some("Monday"),
            Some("hello"),
            None,
        );
        assert_eq!(below, "\n\nOn Monday, Thomas wrote:\n> hello\n\n-- \nSam\n");
        let unquoted = ReplyConfig {
            quote: false,
            ..ReplyConfig::default()
        };
        assert_eq!(
            reply_body(&unquoted, &work, None, None, Some("hello"), None),
            "\n\n-- \nSam\n"
        );
        assert_eq!(
            forward_body(&work, Some("Thomas"), None, None, None, Some("hello"), None),
            format!(
                "\n\n-- \nSam\n\n{FORWARDED}\nFrom: Thomas\nDate: \nSubject: \nTo: \n\nhello\n"
            )
        );
    }

    #[test]
    fn switching_identity_replaces_the_signature_rather_than_adding_one() {
        let work = inline("Work", "Me <me@example.com>", Some("Sam"));
        let body = "Thanks!\n\n-- \nSam\n";

        let swapped = swap_signature(body, &work, &support(), SignaturePlacement::Below);
        assert_eq!(swapped, "Thanks!\n\n-- \nSupport\n");

        let back = swap_signature(&swapped, &support(), &work, SignaturePlacement::Below);
        assert_eq!(back, body, "and back again, leaving the text as it was");
    }

    #[test]
    fn a_signature_already_in_a_reopened_draft_is_not_added_twice() {
        let work = inline("Work", "Me <me@example.com>", Some("Sam"));
        let body = "Thanks!\n\n-- \nSam\n";
        assert_eq!(
            swap_signature(body, &work, &work, SignaturePlacement::Below),
            body
        );

        // A signature the identity did not write — edited, or from elsewhere —
        // is left alone rather than joined by a second one.
        let edited = "Thanks!\n\n-- \nSam, on holiday\n";
        assert_eq!(
            swap_signature(edited, &support(), &work, SignaturePlacement::Below),
            edited
        );
    }

    #[test]
    fn a_body_without_a_signature_gets_one_where_a_fresh_draft_has_it() {
        let none = inline("Plain", "Me <me@example.com>", None);
        let work = inline("Work", "Me <me@example.com>", Some("Sam"));
        let reply = "Thanks!\n\nOn Monday, Thomas wrote:\n> hello\n";

        assert_eq!(
            swap_signature(reply, &none, &work, SignaturePlacement::Below),
            "Thanks!\n\nOn Monday, Thomas wrote:\n> hello\n\n-- \nSam\n"
        );
        assert_eq!(
            swap_signature(reply, &none, &work, SignaturePlacement::Above),
            "Thanks!\n\n-- \nSam\n\nOn Monday, Thomas wrote:\n> hello\n"
        );

        let forward = format!("Have a look.\n\n{FORWARDED}\nFrom: Thomas\n");
        assert_eq!(
            swap_signature(&forward, &none, &work, SignaturePlacement::Below),
            format!("Have a look.\n\n-- \nSam\n\n{FORWARDED}\nFrom: Thomas\n")
        );
    }

    #[test]
    fn switching_to_an_identity_without_a_signature_removes_the_old_one() {
        let work = inline("Work", "Me <me@example.com>", Some("Sam"));
        let none = inline("Plain", "Me <me@example.com>", None);
        assert_eq!(
            swap_signature(
                "Thanks!\n\n-- \nSam\n",
                &work,
                &none,
                SignaturePlacement::Below
            ),
            "Thanks!\n"
        );
    }

    #[test]
    fn an_identity_owns_its_from_and_its_addresses() {
        let support = support();
        assert!(support.owns("SUPPORT@example.com"));
        assert!(support.owns("Help Desk <help@example.com>"));
        assert!(!support.owns("me@example.com"));
    }

    fn sender(account: &str, name: &str, from: &str, domains: &[&str]) -> Sender {
        Sender {
            account: account.to_string(),
            identity: IdentityConfig {
                domains: domains.iter().map(|d| d.to_string()).collect(),
                ..inline(name, from, None)
            },
        }
    }

    fn senders() -> Vec<Sender> {
        let mut support = sender("Work", "Support", "Support <support@example.com>", &[]);
        support.identity.addresses = vec!["help@example.com".into()];
        vec![
            sender("Work", "Work", "Me <me@example.com>", &["example.com"]),
            support,
            sender("Private", "Private", "Me <me@private.example>", &[]),
        ]
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn kinds(warnings: &[DraftWarning]) -> Vec<&'static str> {
        warnings.iter().map(|w| w.kind).collect()
    }

    #[test]
    fn a_reply_written_as_another_sender_than_the_one_addressed_is_warned_about() {
        let senders = senders();
        let addressed = strings(&["Alice <alice@else.example>", "help@example.com"]);
        let facts = WarningFacts {
            addressed_to: &addressed,
            ..Default::default()
        };

        let as_support = sender_warnings(&senders, &senders[1], &facts);
        assert!(as_support.is_empty(), "{as_support:?}");

        let as_private = sender_warnings(&senders, &senders[2], &facts);
        assert_eq!(kinds(&as_private), ["identity"]);
        assert_eq!(
            as_private[0].message,
            "The original went to help@example.com (Support), but this is from me@private.example"
        );
    }

    #[test]
    fn mail_delivered_to_an_address_no_sender_owns_is_warned_about() {
        let senders = senders();
        let addressed = strings(&["alice@else.example", "orders@example.com"]);
        let delivered = strings(&["orders@example.com"]);
        let warnings = sender_warnings(
            &senders,
            &senders[0],
            &WarningFacts {
                addressed_to: &addressed,
                delivered_to: &delivered,
                ..Default::default()
            },
        );
        assert_eq!(kinds(&warnings), ["unowned"]);
        assert!(
            warnings[0]
                .message
                .contains("orders@example.com, which no identity sends as")
        );

        // Without a Delivered-To, To and Cc are somebody's colleagues as
        // often as they are an alias.
        let quiet = sender_warnings(
            &senders,
            &senders[0],
            &WarningFacts {
                addressed_to: &addressed,
                ..Default::default()
            },
        );
        assert!(quiet.is_empty(), "{quiet:?}");
    }

    #[test]
    fn a_recipient_in_another_senders_domains_is_warned_about() {
        let senders = senders();
        let recipients = strings(&[
            "Anna <anna@example.com>",
            "bob@sub.example.com",
            "carol@notexample.com",
        ]);
        let facts = WarningFacts {
            recipients: &recipients,
            ..Default::default()
        };

        let warnings = sender_warnings(&senders, &senders[2], &facts);
        assert_eq!(kinds(&warnings), ["domain"]);
        assert_eq!(
            warnings[0].message,
            "anna@example.com, bob@sub.example.com are written to as Work (me@example.com), \
             but this is from me@private.example"
        );
        assert!(sender_warnings(&senders, &senders[0], &facts).is_empty());
    }

    #[test]
    fn an_attachment_mentioned_in_the_own_text_without_one_is_warned_about() {
        let words = ComposeConfig::default().attachment_words;
        let warning =
            attachment_warning("Hi,\n\nanbei die Offerte.\n", false, &words).expect("mentioned");
        assert_eq!(
            warning.message,
            "Mentions an attachment (“anbei”) but has none"
        );
        assert_eq!(
            mentioned_word("Please see the Attached file", &words).as_deref(),
            Some("attached")
        );
        assert!(attachment_warning("anbei die Offerte.", true, &words).is_none());
        assert!(attachment_warning("anbei.", false, &[]).is_none());
        assert!(
            mentioned_word("reattach nothing", &words).is_none(),
            "only at the start of a word"
        );
    }

    #[test]
    fn the_quote_the_signature_and_a_forward_mention_nothing() {
        let words = ComposeConfig::default().attachment_words;
        let reply =
            "Thanks!\n\nOn Monday, Bob wrote:\n> see the attachment\n\n-- \nAttachments Ltd\n";
        assert!(attachment_warning(reply, false, &words).is_none());
        let above =
            "Thanks!\n\n-- \nAttachments Ltd\n\nOn Monday, Bob wrote:\n> hi\n\nI attached it.\n";
        assert!(
            attachment_warning(above, false, &words).is_some(),
            "an inline answer below the quote is the writer's own"
        );
        let forward = format!("FYI\n\n{FORWARDED}\nSubject: the attached plan\n");
        assert!(attachment_warning(&forward, false, &words).is_none());
    }
}
