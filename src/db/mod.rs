mod connection;

pub use connection::{
    Contact, Database, FeedState, Headline, LockAttempt, NewMessage, OutboxItem, ReconcileReport,
};

#[cfg(test)]
pub(crate) use connection::{CHAT_MENTIONS_ALL_SCHEMA, CHAT_QUOTES_SCHEMA, CHAT_SCHEMA};

/// `value` as a LIKE pattern that matches itself: `%`, `_` and `\` escaped
/// with `\`, for a query that says `ESCAPE '\'`. A folder or tag name with
/// an underscore in it would otherwise match any character there.
pub fn like_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}
