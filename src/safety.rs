//! Defences against what a message can do to the person reading it.
//!
//! Mail is hostile input. The HTML rendering keeps a message off the network
//! (see the reader in the app); this module covers the rest: who a message
//! claims to be from and whether its server agreed, which attachments are
//! dangerous to open, and what a link carries along to the site it opens.

use mailparse::MailHeaderMap;
use regex::Regex;
use std::sync::LazyLock;

/// The header fields [`check_sender`] reads, as asked of the server.
pub const SENDER_HEADER_FIELDS: &[&str] = &["FROM", "REPLY-TO", "AUTHENTICATION-RESULTS"];

/// What a message's headers say about its sender, for the reader's header.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SenderCheck {
    /// The verdict the receiving server recorded against the sender, as it
    /// wrote it — `dmarc=fail`, or `spf=fail` where there is no DMARC verdict
    /// and no DKIM signature passed. `None` when nothing failed, and when the
    /// server recorded nothing.
    pub failed: Option<String>,
    /// The `Reply-To` address, when it is in another domain than the sender:
    /// an answer goes somewhere the sender's name does not suggest.
    pub reply_elsewhere: Option<String>,
}

/// Read what the headers say about the sender, off a header block or a whole
/// message.
///
/// Only the topmost `Authentication-Results` counts. It is the one the
/// receiving server added last; any below it came with the message, and
/// anyone can write one.
pub fn check_sender(raw: &[u8]) -> SenderCheck {
    let Ok((headers, _)) = mailparse::parse_headers(raw) else {
        return SenderCheck::default();
    };
    let addresses = |name: &str| -> Vec<String> {
        headers
            .get_first_header(name)
            .and_then(|header| mailparse::addrparse_header(header).ok())
            .map(|list| {
                list.iter()
                    .flat_map(|entry| match entry {
                        mailparse::MailAddr::Single(single) => vec![single.addr.clone()],
                        mailparse::MailAddr::Group(group) => group
                            .addrs
                            .iter()
                            .map(|single| single.addr.clone())
                            .collect(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let failed = headers
        .get_first_value("Authentication-Results")
        .and_then(|results| failed_verdict(&results));

    let from_domain = addresses("From").first().and_then(|from| domain_of(from));
    let reply_elsewhere = from_domain.and_then(|from| {
        addresses("Reply-To")
            .into_iter()
            .find(|reply| domain_of(reply).is_some_and(|domain| !same_organisation(&domain, &from)))
    });

    SenderCheck {
        failed,
        reply_elsewhere,
    }
}

/// The verdict of an `Authentication-Results` value that says the sender is
/// not who they claim, if any.
///
/// DMARC decides when it is there. Without it, SPF failing outright with no
/// DKIM signature passing is the same verdict reached by hand; a `softfail`
/// is too common in forwarded mail to raise.
fn failed_verdict(results: &str) -> Option<String> {
    // Comments, `(p=none dis=none)` or `(google.com: domain of … designates …)`,
    // say things that look like verdicts and are not.
    static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\([^()]*\)").unwrap());
    static VERDICT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\b(dmarc|spf|dkim)\s*=\s*([a-z]+)").unwrap());

    let mut text = results.to_string();
    while COMMENT.is_match(&text) {
        text = COMMENT.replace_all(&text, " ").into_owned();
    }
    let mut dmarc = None;
    let mut spf = None;
    let mut dkim_passed = false;
    for verdict in VERDICT.captures_iter(&text) {
        let method = verdict[1].to_ascii_lowercase();
        let result = verdict[2].to_ascii_lowercase();
        match method.as_str() {
            "dmarc" => {
                dmarc.get_or_insert(result);
            }
            "spf" => {
                spf.get_or_insert(result);
            }
            _ => dkim_passed |= result == "pass",
        }
    }
    match (dmarc.as_deref(), spf.as_deref()) {
        (Some("fail"), _) => Some("dmarc=fail".to_string()),
        (None, Some("fail")) if !dkim_passed => Some("spf=fail".to_string()),
        _ => None,
    }
}

/// The domain of an address, lower-cased.
fn domain_of(address: &str) -> Option<String> {
    let (_, domain) = address.rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('>').to_ascii_lowercase();
    (!domain.is_empty()).then_some(domain)
}

/// True when two domains belong to the same organisation, as far as a mail
/// client can tell without the public suffix list: the same name, one under
/// the other, or the same registrable name — the last two labels, or the last
/// three under a short second-level one such as `co.uk`.
fn same_organisation(a: &str, b: &str) -> bool {
    let a = a.trim_start_matches("www.");
    let b = b.trim_start_matches("www.");
    a == b
        || a.ends_with(&format!(".{b}"))
        || b.ends_with(&format!(".{a}"))
        || registrable(a) == registrable(b)
}

fn registrable(domain: &str) -> &str {
    let labels: Vec<&str> = domain.split('.').collect();
    let keep = match labels.as_slice() {
        [.., second, _] if labels.len() >= 3 && second.len() <= 3 => 3,
        _ => 2,
    };
    let start = labels.len().saturating_sub(keep);
    let offset: usize = labels[..start].iter().map(|label| label.len() + 1).sum();
    &domain[offset..]
}

/// File types that are dangerous to open although macOS would not run them,
/// with why. Programs macOS would run are revealed in Finder instead of
/// opened, which the app decides on its own.
const RISKY_SUFFIXES: &[(&[&str], &str)] = &[
    (
        &[
            "docm", "dotm", "xlsm", "xltm", "xlam", "pptm", "potm", "ppam", "sldm",
        ],
        "is an Office document that can run macros",
    ),
    (
        &["html", "htm", "shtml", "xhtml", "mht", "mhtml", "svg"],
        "opens in the browser, where it can pose as a sign-in page",
    ),
    (
        &["iso", "img", "dmg", "vhd", "vhdx"],
        "is a disk image, a common way to smuggle programs past checks",
    ),
    (
        &[
            "exe", "scr", "com", "bat", "cmd", "msi", "pif", "vbs", "vbe", "js", "jse", "wsf",
            "wsh", "ps1", "lnk", "hta", "cpl", "jar", "reg",
        ],
        "is a program",
    ),
];

/// Why an attachment is dangerous to open, if it is.
///
/// A name with a direction override in it is suspect whatever it ends in:
/// `invoice\u{202E}fdp.exe` shows as `invoiceexe.pdf`.
pub fn attachment_risk(filename: &str) -> Option<&'static str> {
    let hides = filename
        .chars()
        .any(|c| matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'));
    if hides {
        return Some("has a name that hides its real file type");
    }
    let (_, suffix) = filename.trim().rsplit_once('.')?;
    let suffix = suffix.trim().to_ascii_lowercase();
    RISKY_SUFFIXES
        .iter()
        .find(|(suffixes, _)| suffixes.contains(&suffix.as_str()))
        .map(|(_, why)| *why)
}

/// Query parameters that exist to tell the sender who clicked: campaign tags
/// and the click identifiers of ad and mailing platforms.
const TRACKING_PARAMS: &[&str] = &[
    "fbclid",
    "gclid",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "yclid",
    "twclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_hsenc",
    "_hsmi",
    "__hssc",
    "__hstc",
    "__hsfp",
    "hsctatracking",
    "mkt_tok",
    "vero_id",
    "vero_conv",
    "oly_anon_id",
    "oly_enc_id",
    "rb_clickid",
    "_kx",
    "ck_subscriber_id",
];

/// A web address without its tracking parameters: `utm_*` and the click
/// identifiers in [`TRACKING_PARAMS`]. Everything else, encoding and order
/// included, is left as it was; anything that is not `http` or `https`
/// comes back unchanged.
pub fn without_tracking_params(url: &str) -> String {
    let lowered = url.trim_start().to_ascii_lowercase();
    if !(lowered.starts_with("http://") || lowered.starts_with("https://")) {
        return url.to_string();
    }
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (url, None),
    };
    let Some((base, query)) = rest.split_once('?') else {
        return url.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
            !(key.starts_with("utm_") || TRACKING_PARAMS.contains(&key.as_str()))
        })
        .collect();
    let mut out = base.to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(headers: &str) -> SenderCheck {
        check_sender(headers.replace('\n', "\r\n").as_bytes())
    }

    #[test]
    fn a_failed_dmarc_verdict_is_reported() {
        let found = check(
            "Authentication-Results: mx.example.net; dkim=pass header.d=other.example; \
             spf=pass smtp.mailfrom=other.example; dmarc=fail (p=reject dis=none) header.from=bank.example\n\
             From: Bank <alerts@bank.example>\n\n",
        );
        assert_eq!(found.failed.as_deref(), Some("dmarc=fail"));
    }

    #[test]
    fn spf_failing_counts_only_without_dmarc_and_a_passing_signature() {
        let failed =
            check("Authentication-Results: mx.example.net; spf=fail smtp.mailfrom=a.example\n\n");
        assert_eq!(failed.failed.as_deref(), Some("spf=fail"));
        let signed = check(
            "Authentication-Results: mx.example.net; spf=fail smtp.mailfrom=a.example; dkim=pass header.d=a.example\n\n",
        );
        assert_eq!(signed.failed, None);
        let passed = check(
            "Authentication-Results: mx.example.net; spf=fail smtp.mailfrom=a.example; dmarc=pass\n\n",
        );
        assert_eq!(passed.failed, None);
    }

    #[test]
    fn a_softfail_and_a_verdict_inside_a_comment_are_not_failures() {
        let found = check(
            "Authentication-Results: mx.example.net; spf=softfail (domain of a.example: dmarc=fail) \
             smtp.mailfrom=a.example\n\n",
        );
        assert_eq!(found.failed, None);
    }

    #[test]
    fn only_the_topmost_authentication_results_counts() {
        let found = check(
            "Authentication-Results: mx.example.net; dmarc=fail header.from=bank.example\n\
             Authentication-Results: forged.example; dmarc=pass header.from=bank.example\n\n",
        );
        assert_eq!(found.failed.as_deref(), Some("dmarc=fail"));
        let forged = check(
            "Authentication-Results: mx.example.net; dmarc=pass header.from=bank.example\n\
             Authentication-Results: forged.example; dmarc=fail header.from=bank.example\n\n",
        );
        assert_eq!(forged.failed, None);
    }

    #[test]
    fn a_reply_to_in_another_domain_is_reported() {
        let found = check(
            "From: Bank <alerts@bank.example>\nReply-To: Support <help@bank-support.example>\n\n",
        );
        assert_eq!(
            found.reply_elsewhere.as_deref(),
            Some("help@bank-support.example")
        );
    }

    #[test]
    fn a_reply_to_in_the_same_organisation_is_not() {
        for reply in [
            "news@bank.example",
            "news@mail.bank.example",
            "a@lists.bank.example",
        ] {
            let found = check(&format!(
                "From: <alerts@www.bank.example>\nReply-To: {reply}\n\n"
            ));
            assert_eq!(found.reply_elsewhere, None, "{reply}");
        }
        let uk = check("From: a@mail.shop.co.uk\nReply-To: b@help.shop.co.uk\n\n");
        assert_eq!(uk.reply_elsewhere, None);
        let other_uk = check("From: a@shop.co.uk\nReply-To: b@thief.co.uk\n\n");
        assert_eq!(other_uk.reply_elsewhere.as_deref(), Some("b@thief.co.uk"));
    }

    #[test]
    fn headers_without_any_of_it_report_nothing() {
        assert_eq!(check("Subject: hi\n\n"), SenderCheck::default());
        assert_eq!(check_sender(b""), SenderCheck::default());
    }

    #[test]
    fn risky_attachments_say_why() {
        assert_eq!(attachment_risk("report.pdf"), None);
        assert_eq!(attachment_risk("photo.JPG"), None);
        assert_eq!(attachment_risk("noextension"), None);
        assert_eq!(
            attachment_risk("Budget.XLSM"),
            Some("is an Office document that can run macros")
        );
        assert_eq!(
            attachment_risk("invoice.html"),
            Some("opens in the browser, where it can pose as a sign-in page")
        );
        assert_eq!(
            attachment_risk("setup.iso"),
            Some("is a disk image, a common way to smuggle programs past checks")
        );
        assert_eq!(attachment_risk("invoice.pdf.exe"), Some("is a program"));
        assert_eq!(attachment_risk("invoice.pdf   .exe"), Some("is a program"));
        assert_eq!(
            attachment_risk("invoice\u{202E}fdp.exe"),
            Some("has a name that hides its real file type")
        );
    }

    #[test]
    fn tracking_parameters_are_removed_and_the_rest_kept() {
        assert_eq!(
            without_tracking_params(
                "https://shop.example/item?id=7&utm_source=news&UTM_Medium=mail&fbclid=x#reviews"
            ),
            "https://shop.example/item?id=7#reviews"
        );
        assert_eq!(
            without_tracking_params("https://shop.example/?utm_campaign=a&mc_eid=b"),
            "https://shop.example/"
        );
        assert_eq!(
            without_tracking_params("https://shop.example/search?q=a%26b&gclid=1&page=2"),
            "https://shop.example/search?q=a%26b&page=2"
        );
    }

    #[test]
    fn addresses_without_tracking_parameters_are_unchanged() {
        for url in [
            "https://shop.example/item?id=7",
            "https://shop.example/",
            "https://shop.example/#utm_source=x",
            "mailto:a@b.example?subject=utm_source",
        ] {
            assert_eq!(without_tracking_params(url), url);
        }
    }
}
