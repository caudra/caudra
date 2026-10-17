//! Provider accounts on the command line: where each provider's accounts
//! stand, which one it uses, the name a login lands under, and signing out.

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use caudra_providers::Timeouts;
use caudra_providers::accounts::{
    AccountKind, AccountStatus, AccountTier, LoginCredential, LoginTarget, PoolStatus, SavedLogin,
    fetch_account_usage, login_target, pool_status, rename_account, save_login, select_account,
    sign_out,
};
use caudra_storage::StateDir;
use caudra_storage::accounts::{AccountIdentity, AccountName, AccountSelection, StoredSecret};
use caudra_storage::auth::{ProviderCredentials, now_millis};

use super::subcmd::PROVIDER_SLUG_WIDTH;

const STATUS_SIGNED_IN: &str = "\x1b[32m✓\x1b[0m";
const STATUS_SIGNED_OUT: &str = "\x1b[31m✗\x1b[0m";
const PINNED_MARKER: &str = "●";
const UNPINNED_MARKER: &str = " ";
const DETAIL_SEPARATOR: &str = " · ";
const MILLIS_PER_MINUTE: u64 = 60_000;
const MINUTES_PER_HOUR: u64 = 60;
const FULL_PERCENT: f32 = 100.0;
const INPUT_CLOSED: &str = "input closed before an answer was entered";

/// A provider's header line and one line per account, or `None` when it has
/// no account to show: none at all, or only the one a keyless provider
/// stands up by itself.
pub(super) fn provider_status(slug: &str, display: &str, refresh: bool) -> Result<Option<String>> {
    if refresh {
        refresh_quotas(slug)?;
    }
    let pool = pool_status(slug, None)?;
    let listed = pool
        .accounts
        .iter()
        .any(|account| account.kind != AccountKind::Keyless || account.configured);
    Ok(listed.then(|| render_pool(slug, display, &pool, now_millis())))
}

/// Asks every subscription of the slug for its quota, so the listing shows
/// today's numbers rather than the last response's.
fn refresh_quotas(slug: &str) -> Result<()> {
    let pool = pool_status(slug, None)?;
    let subscriptions = pool
        .accounts
        .iter()
        .filter(|account| account.kind == AccountKind::Subscription);
    for account in subscriptions {
        let fetched = smol::block_on(fetch_account_usage(
            slug,
            &account.name,
            Timeouts::default(),
        ));
        if let Err(error) = fetched {
            eprintln!("  {slug}:{}: quota refresh failed: {error}", account.name);
        }
    }
    Ok(())
}

fn render_pool(slug: &str, display: &str, pool: &PoolStatus, now_ms: u64) -> String {
    let signed_in = pool
        .accounts
        .iter()
        .any(|account| account.kind != AccountKind::SignedOut);
    let badge = if signed_in {
        STATUS_SIGNED_IN
    } else {
        STATUS_SIGNED_OUT
    };
    let mut output = format!("  {badge} {slug:<PROVIDER_SLUG_WIDTH$} {display}");
    if let Some(selection) = selection_label(pool) {
        let _ = write!(output, "{DETAIL_SEPARATOR}{selection}");
    }
    output.push('\n');
    let width = pool
        .accounts
        .iter()
        .map(|account| account.name.len())
        .max()
        .unwrap_or_default();
    for account in &pool.accounts {
        let pinned = matches!(&pool.selection, AccountSelection::Pinned(name) if name.as_str() == account.name);
        let marker = if pinned {
            PINNED_MARKER
        } else {
            UNPINNED_MARKER
        };
        let _ = writeln!(
            output,
            "    {marker} {:<width$}  {}",
            account.name,
            account_detail(slug, account, now_ms)
        );
    }
    output
}

/// Auto with a single account has nothing to choose between, so it goes
/// unsaid.
fn selection_label(pool: &PoolStatus) -> Option<String> {
    let label = match &pool.selection {
        AccountSelection::Auto if pool.accounts.len() < 2 => return None,
        AccountSelection::Auto => format!("auto, {}", pool.strategy.as_str()),
        AccountSelection::Pinned(name) => format!("pinned to {name}"),
    };
    Some(if pool.overridden {
        format!("{label} for this run")
    } else {
        label
    })
}

fn account_detail(slug: &str, account: &AccountStatus, now_ms: u64) -> String {
    let masked = account.masked_key.as_deref().unwrap_or_default();
    let mut details = vec![match account.kind {
        AccountKind::Subscription => "subscription".to_owned(),
        AccountKind::StoredKey => format!("key {masked}"),
        AccountKind::ConfigKey => format!("key {masked} from providers.toml"),
        AccountKind::EnvKey => format!("via {}", account.env_var.as_deref().unwrap_or_default()),
        AccountKind::Keyless => "no key needed".to_owned(),
        AccountKind::SignedOut => format!(
            "signed out (run: caudra auth login {slug} --name {})",
            account.name
        ),
    }];
    let AccountIdentity {
        email,
        organization,
        plan,
        ..
    } = &account.identity;
    details.extend(email.as_ref().or(organization.as_ref()).cloned());
    details.extend(plan.clone());
    if account.fallback {
        details.push("fallback".to_owned());
    }
    details.extend(account.windows.iter().map(|window| {
        let left = (FULL_PERCENT - window.used_percent)
            .round()
            .clamp(0.0, FULL_PERCENT);
        format!("{} {left}% left", window.kind.label())
    }));
    match account.blocked_until {
        Some(until_ms) => details.push(format!(
            "blocked for {}",
            format_wait(until_ms.saturating_sub(now_ms))
        )),
        None if account.tier == AccountTier::Spent && account.kind != AccountKind::SignedOut => {
            details.push("spent".to_owned());
        }
        None => {}
    }
    details.join(DETAIL_SEPARATOR)
}

fn format_wait(millis: u64) -> String {
    let minutes = millis.div_ceil(MILLIS_PER_MINUTE);
    match (minutes / MINUTES_PER_HOUR, minutes % MINUTES_PER_HOUR) {
        (0, minutes) => format!("{minutes}m"),
        (hours, minutes) => format!("{hours}h{minutes:02}m"),
    }
}

/// `caudra auth use`: a pin must name an account that is signed in, since
/// every request would fail on one that is not.
pub(super) fn use_account(
    storage: &StateDir,
    slug: &str,
    selection: &AccountSelection,
) -> Result<()> {
    let pool = pool_status(slug, None)?;
    if let AccountSelection::Pinned(name) = selection {
        match pool
            .accounts
            .iter()
            .find(|account| account.name == name.as_str())
        {
            None => bail!("{slug} has no account named `{name}`; see caudra auth status"),
            Some(account) if account.kind == AccountKind::SignedOut => {
                bail!(
                    "{slug} account `{name}` is signed out; run: caudra auth login {slug} --name {name}"
                )
            }
            Some(_) => {}
        }
    }
    select_account(storage, slug, selection)?;
    match selection {
        AccountSelection::Auto => println!("{slug} now switches accounts automatically."),
        AccountSelection::Pinned(name) => println!("{slug} now uses account `{name}`."),
    }
    if pool.overridden {
        println!("--account or CAUDRA_ACCOUNT still decides for the run that sets it.");
    }
    Ok(())
}

pub(super) fn rename(
    storage: &StateDir,
    slug: &str,
    from: &AccountName,
    to: &AccountName,
) -> Result<()> {
    rename_account(storage, slug, from, to)?;
    println!("Renamed {slug} account `{from}` to `{to}`.");
    Ok(())
}

/// Signs out of the named account, of every one with `all`, or of the only
/// one. An account read from the environment cannot be signed out of here.
pub(super) fn logout(
    storage: &StateDir,
    slug: &str,
    name: Option<AccountName>,
    all: bool,
) -> Result<()> {
    let pool = pool_status(slug, None)?;
    let stored: Vec<&AccountStatus> = pool
        .accounts
        .iter()
        .filter(|account| {
            matches!(
                account.kind,
                AccountKind::Subscription | AccountKind::StoredKey
            )
        })
        .collect();
    let targets: Vec<AccountName> = match name {
        Some(name) => {
            if let Some(account) = pool.accounts.iter().find(|account| {
                account.name == name.as_str() && account.kind == AccountKind::EnvKey
            }) {
                bail!(
                    "{slug} account `{name}` comes from {}; unset it instead",
                    account.env_var.as_deref().unwrap_or_default()
                );
            }
            vec![name]
        }
        None => match stored.as_slice() {
            [] => {
                println!("Not currently logged in to '{slug}'.");
                return Ok(());
            }
            [only] => vec![only.name.parse()?],
            _ if all => stored
                .iter()
                .map(|account| account.name.parse::<AccountName>())
                .collect::<Result<Vec<_>, _>>()?,
            many => {
                let names: Vec<&str> = many.iter().map(|account| account.name.as_str()).collect();
                bail!(
                    "{slug} has {} signed-in accounts ({}); name one or pass --all",
                    names.len(),
                    names.join(", ")
                );
            }
        },
    };
    for name in targets {
        let signed_out = sign_out(storage, slug, &name)?;
        if signed_out.had_credential {
            println!("Signed out of {slug} account `{name}`.");
        } else {
            println!("{slug} account `{name}` was not signed in.");
        }
        if signed_out.unpinned {
            println!("It was pinned, so {slug} switches accounts automatically again.");
        }
    }
    Ok(())
}

/// Stores a key a user entered as an account of the slug.
pub(super) fn save_key(
    storage: &StateDir,
    slug: &str,
    api_key: String,
    name: Option<AccountName>,
) -> Result<SavedLogin> {
    let login = LoginCredential {
        secret: StoredSecret::ApiKey(ProviderCredentials {
            api_key,
            host: None,
        }),
        identity: AccountIdentity::default(),
    };
    save(storage, slug, login, name)
}

/// Stores `login` under `name`, or where [`login_name`] puts it, and says
/// which account it landed in.
pub(super) fn save(
    storage: &StateDir,
    slug: &str,
    login: LoginCredential,
    name: Option<AccountName>,
) -> Result<SavedLogin> {
    let name = match name {
        Some(name) => Some(name),
        None => login_name(storage, slug, &login)?,
    };
    let saved = save_login(storage, slug, login, name.as_ref())?;
    let verb = if saved.refreshed {
        "Signed in again as"
    } else {
        "Added"
    };
    println!(
        "  {STATUS_SIGNED_IN} {verb} {} account `{}`",
        saved.id.provider, saved.id.name
    );
    Ok(saved)
}

/// A login that signs in as an account already stored refreshes it. Any
/// other is new: it fills the first account `providers.toml` lists as signed
/// out, or else takes a name drawn from who it signs in as. The user confirms
/// that name at a terminal once the provider has accounts to tell apart.
fn login_name(
    storage: &StateDir,
    slug: &str,
    login: &LoginCredential,
) -> Result<Option<AccountName>> {
    let LoginTarget::New(suggested) = login_target(storage, slug, login)? else {
        return Ok(None);
    };
    let accounts = pool_status(slug, None)
        .map(|pool| pool.accounts)
        .unwrap_or_default();
    let default = accounts
        .iter()
        .find(|account| account.kind == AccountKind::SignedOut)
        .and_then(|account| account.name.parse().ok())
        .unwrap_or(suggested);
    if accounts.is_empty() || !io::stdin().is_terminal() {
        return Ok(Some(default));
    }
    print!("  Account name [{default}]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        bail!(INPUT_CLOSED);
    }
    let typed = input.trim();
    if typed.is_empty() {
        return Ok(Some(default));
    }
    let name: AccountName = typed.parse().context("account name")?;
    let taken = accounts
        .iter()
        .any(|account| account.name == name.as_str() && account.kind != AccountKind::SignedOut);
    if taken {
        bail!(
            "{slug} already has an account named `{name}`; pass --name {name} to replace its credential"
        );
    }
    Ok(Some(name))
}

/// Hidden at a terminal; read as a line otherwise, so a key can be piped in.
pub(super) fn read_key(prompt: &str) -> Result<String> {
    let key = if io::stdin().is_terminal() {
        rpassword::prompt_password(prompt).context("read hidden API key")?
    } else {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        line
    };
    Ok(key.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use caudra_config::providers::SwitchStrategy;
    use caudra_providers::Billing;
    use caudra_providers::accounts::{QuotaWindow, WindowKind};
    use test_case::test_case;

    use super::*;

    const NOW_MS: u64 = 1_767_225_600_000;
    const HOUR_MS: u64 = 3_600_000;
    const SLUG: &str = "anthropic";
    const DISPLAY: &str = "Anthropic";
    const PERSONAL: &str = "personal";
    const WORK: &str = "work";
    const EMAIL: &str = "me@example.com";
    const MASKED: &str = "sk-…a1b2";
    const ENV_VAR: &str = "ANTHROPIC_API_KEY";

    fn account(name: &str, kind: AccountKind) -> AccountStatus {
        AccountStatus {
            name: name.into(),
            kind,
            billing: Billing::from_oauth(kind == AccountKind::Subscription),
            fallback: false,
            configured: false,
            identity: AccountIdentity::default(),
            masked_key: None,
            env_var: None,
            windows: Vec::new(),
            observed_at: 0,
            blocked_until: None,
            tier: AccountTier::Healthy,
            left_percent: None,
        }
    }

    fn pool(selection: AccountSelection, accounts: Vec<AccountStatus>) -> PoolStatus {
        PoolStatus {
            provider: SLUG.into(),
            selection,
            overridden: false,
            strategy: SwitchStrategy::Ordered,
            switch_below: 5,
            prefer_below: 20,
            accounts,
        }
    }

    fn pinned(name: &str) -> AccountSelection {
        AccountSelection::Pinned(name.parse().unwrap())
    }

    #[test_case(AccountSelection::Auto, 1, false => None ; "auto_alone_goes_unsaid")]
    #[test_case(AccountSelection::Auto, 2, false => Some("auto, ordered".to_owned()) ; "auto_names_the_strategy")]
    #[test_case(pinned(PERSONAL), 1, false => Some("pinned to personal".to_owned()) ; "a_pin_is_always_said")]
    #[test_case(pinned(PERSONAL), 2, true => Some("pinned to personal for this run".to_owned()) ; "an_override_says_it_is_for_this_run")]
    fn the_selection_is_said_when_it_matters(
        selection: AccountSelection,
        accounts: usize,
        overridden: bool,
    ) -> Option<String> {
        let names = [PERSONAL, WORK];
        let mut pool = pool(
            selection,
            names[..accounts]
                .iter()
                .map(|name| account(name, AccountKind::Subscription))
                .collect(),
        );
        pool.overridden = overridden;
        selection_label(&pool)
    }

    fn subscription_with_quota() -> AccountStatus {
        AccountStatus {
            identity: AccountIdentity {
                email: Some(EMAIL.into()),
                plan: Some("Max".into()),
                ..AccountIdentity::default()
            },
            windows: vec![
                QuotaWindow::new(WindowKind::FiveHour, 59.4, None),
                QuotaWindow::new(WindowKind::Weekly, 18.0, None),
            ],
            ..account(PERSONAL, AccountKind::Subscription)
        }
    }

    fn blocked_key() -> AccountStatus {
        AccountStatus {
            masked_key: Some(MASKED.into()),
            fallback: true,
            blocked_until: Some(NOW_MS + 2 * HOUR_MS + 1),
            tier: AccountTier::Spent,
            ..account(WORK, AccountKind::StoredKey)
        }
    }

    fn env_key() -> AccountStatus {
        AccountStatus {
            env_var: Some(ENV_VAR.into()),
            tier: AccountTier::Spent,
            ..account(WORK, AccountKind::EnvKey)
        }
    }

    #[test_case(subscription_with_quota(), "subscription · me@example.com · Max · 5h 41% left · weekly 82% left" ; "a_subscription_shows_who_and_its_windows")]
    #[test_case(blocked_key(), "key sk-…a1b2 · fallback · blocked for 2h01m" ; "a_blocked_key_says_for_how_long")]
    #[test_case(env_key(), "via ANTHROPIC_API_KEY · spent" ; "a_spent_env_key")]
    #[test_case(account(WORK, AccountKind::SignedOut), "signed out (run: caudra auth login anthropic --name work)" ; "a_signed_out_entry_says_how_to_sign_in")]
    fn each_account_says_where_it_stands(account: AccountStatus, expected: &str) {
        assert_eq!(account_detail(SLUG, &account, NOW_MS), expected);
    }

    #[test]
    fn a_pool_lists_its_accounts_under_the_provider_and_marks_the_pin() {
        let pool = pool(
            pinned(WORK),
            vec![
                account(PERSONAL, AccountKind::Subscription),
                account(WORK, AccountKind::SignedOut),
            ],
        );

        let rendered = render_pool(SLUG, DISPLAY, &pool, NOW_MS);

        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(STATUS_SIGNED_IN));
        assert!(lines[0].ends_with("Anthropic · pinned to work"));
        assert_eq!(lines[1], "      personal  subscription");
        assert!(lines[2].starts_with(&format!("    {PINNED_MARKER} work      signed out")));
    }

    #[test_case(0 => "0m" ; "nothing")]
    #[test_case(1 => "1m" ; "rounds_up_to_a_minute")]
    #[test_case(59 * 60_000 => "59m" ; "under_an_hour")]
    #[test_case(HOUR_MS + 5 * 60_000 => "1h05m" ; "hours_and_minutes")]
    fn waits_read_in_hours_and_minutes(millis: u64) -> String {
        format_wait(millis)
    }
}
