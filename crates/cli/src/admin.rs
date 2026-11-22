//! `scanner admin`: accounts, API tokens and sessions for the web API.
//!
//! Works on the same `auth.sqlite` that `scanner serve` uses, and is safe to
//! run while the scanner is serving (SQLite handles the locking).

use anyhow::{Context, Result, bail};
use scanner_web::auth::{AuthStore, MIN_PASSWORD_LEN, now_ns};
use std::io::BufRead;
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct AdminArgs {
    /// Accounts database (serve's default is auth.sqlite in its --out).
    #[arg(long, default_value = "out/auth.sqlite", global = true)]
    db: PathBuf,
    #[command(subcommand)]
    cmd: AdminCmd,
}

#[derive(clap::Subcommand)]
enum AdminCmd {
    /// Create an admin account (prompts for the password).
    Create {
        username: String,
        /// Read the password from the first line of stdin instead of
        /// prompting (for scripts).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Set a new password for an admin; ends their browser sessions.
    Passwd {
        username: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// List admin accounts.
    List,
    /// Delete an admin account and its sessions.
    Delete { username: String },
    /// API tokens for scripts (`Authorization: Bearer scn_...`).
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// End every browser session (everyone has to log in again).
    Logout,
    /// Show recent logins and admin changes.
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

#[derive(clap::Subcommand)]
enum TokenCmd {
    /// Create a token; its secret is shown once.
    Create {
        name: String,
        /// Days until it stops working (default: never).
        #[arg(long)]
        expires_days: Option<f64>,
    },
    List,
    Revoke {
        id: i64,
    },
}

fn read_password(from_stdin: bool) -> Result<String> {
    if from_stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_string());
    }
    let pw = rpassword::prompt_password(format!("Password ({MIN_PASSWORD_LEN}+ characters): "))
        .context("reading password (use --password-stdin when there is no terminal)")?;
    let again = rpassword::prompt_password("Again: ")?;
    if pw != again {
        bail!("the passwords don't match");
    }
    Ok(pw)
}

/// `2026-11-22 20:41:02Z`.
fn time(ns: i64) -> String {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ns.max(0) as u64);
    let c = scanner_core::clock::compact_utc(t);
    format!(
        "{}-{}-{} {}:{}:{}Z",
        &c[0..4],
        &c[4..6],
        &c[6..8],
        &c[9..11],
        &c[11..13],
        &c[13..15]
    )
}

pub fn run(a: AdminArgs) -> Result<()> {
    let store = AuthStore::open(&a.db).with_context(|| format!("opening {}", a.db.display()))?;
    match a.cmd {
        AdminCmd::Create {
            username,
            password_stdin,
        } => {
            let pw = read_password(password_stdin)?;
            store.create_admin(&username, &pw, now_ns())?;
            store.audit(now_ns(), "cli", "admin_created", &username, "local");
            println!("created admin {username:?} in {}", a.db.display());
        }
        AdminCmd::Passwd {
            username,
            password_stdin,
        } => {
            let pw = read_password(password_stdin)?;
            store.set_password(&username, &pw)?;
            store.audit(now_ns(), "cli", "password_set", &username, "local");
            println!("password changed; {username}'s browser sessions have ended");
        }
        AdminCmd::List => {
            let admins = store.admins()?;
            if admins.is_empty() {
                println!("no admin accounts (create one with `scanner admin create <name>`)");
            }
            for adm in admins {
                println!("{:<20} created {}", adm.username, time(adm.created_ns));
            }
        }
        AdminCmd::Delete { username } => {
            store.delete_admin(&username)?;
            store.audit(now_ns(), "cli", "admin_deleted", &username, "local");
            println!("deleted admin {username:?}");
        }
        AdminCmd::Token { cmd } => match cmd {
            TokenCmd::Create { name, expires_days } => {
                let now = now_ns();
                let expires = match expires_days {
                    Some(d) if !(d > 0.0 && d <= 3650.0) => {
                        bail!("--expires-days must be between 0 and 3650")
                    }
                    Some(d) => Some(now + (d * 86400e9) as i64),
                    None => None,
                };
                let (id, secret) = store.create_token(&name, expires, now)?;
                store.audit(
                    now,
                    "cli",
                    "token_created",
                    &format!("{id} {name}"),
                    "local",
                );
                println!("token {id} ({name}); it won't be shown again:\n{secret}");
            }
            TokenCmd::List => {
                let tokens = store.tokens()?;
                if tokens.is_empty() {
                    println!("no API tokens");
                }
                for t in tokens {
                    let state = if t.revoked {
                        "revoked".to_string()
                    } else if t.expires_ns.is_some_and(|e| e <= now_ns()) {
                        "expired".to_string()
                    } else {
                        t.expires_ns
                            .map(|e| format!("expires {}", time(e)))
                            .unwrap_or_else(|| "no expiry".into())
                    };
                    println!(
                        "{:>4}  {:<24} created {}  last used {}  {state}",
                        t.id,
                        t.name,
                        time(t.created_ns),
                        t.last_used_ns.map(time).unwrap_or_else(|| "never".into())
                    );
                }
            }
            TokenCmd::Revoke { id } => {
                if !store.revoke_token(id)? {
                    bail!("no token {id}");
                }
                store.audit(now_ns(), "cli", "token_revoked", &id.to_string(), "local");
                println!("revoked token {id}");
            }
        },
        AdminCmd::Logout => {
            let n = store.delete_all_sessions()?;
            store.audit(now_ns(), "cli", "sessions_ended", &n.to_string(), "local");
            println!("ended {n} browser sessions");
        }
        AdminCmd::Audit { limit } => {
            for e in store.recent_audit(limit)?.into_iter().rev() {
                println!(
                    "{}  {:<16} {:<16} {:<15} {}",
                    time(e.ts_ns),
                    e.actor,
                    e.action,
                    e.client,
                    e.detail
                );
            }
        }
    }
    Ok(())
}
