//! A configuration that points Orb at a test server.

use std::path::Path;
use std::sync::OnceLock;

use orb_core::config::Config;

use super::test_context::TestContext;

/// Every test binary logs to one file, named once.
static LOG_PATH: OnceLock<String> = OnceLock::new();

/// Where the trace lines of this test binary go. The logger is process-wide
/// and initialised once; every test in the binary shares the file.
pub fn log_path() -> &'static str {
    LOG_PATH.get_or_init(|| {
        let path = std::env::temp_dir()
            .join(format!("orb-test-{}.log", std::process::id()))
            .to_string_lossy()
            .into_owned();
        orb_core::logger::init(Some(path.clone()));
        eprintln!("orb log for this test binary: {path}");
        path
    })
}

/// A config with one account, `Test`, on `host:port`, using the container's
/// static password and a database inside `ctx`. The account accepts the
/// container's self-signed certificate.
pub fn config_for(ctx: &TestContext, host: &str, port: u16) -> Config {
    config_with(ctx, host, port, "")
}

/// [`config_for`] with further top-level YAML appended: a `filters` or a
/// `tasks` section.
pub fn config_with(ctx: &TestContext, host: &str, port: u16, extra: &str) -> Config {
    let yaml = format!(
        "db_path: {db}\n\
         log_file: {log}\n\
         accounts:\n  \
           - name: Test\n    \
             imap_server: {host}\n    \
             imap_port: {port}\n    \
             imap_username: testuser\n    \
             imap_password_command: echo supersecret\n    \
             from: Test <test@example.com>\n\
         {extra}",
        db = ctx.db_path.display(),
        log = log_path(),
    );
    std::fs::write(&ctx.config_path, yaml).expect("write the test config");
    let mut config = orb_core::config::read_from_path(path_str(&ctx.config_path))
        .expect("the test config must be readable");
    for account in &mut config.accounts {
        account.accept_invalid_certs = true;
    }
    config
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 path")
}
