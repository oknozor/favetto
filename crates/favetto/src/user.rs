//! `favetto user`: manage the daemon's username/password login accounts.
//!
//! Accounts live in the daemon's SQLite database (`<data_dir>/favetto.db`), so
//! they can be managed without a running daemon — the command opens and migrates
//! the database directly, mirroring `token-rotate`. Passwords are read from
//! stdin (never from argv) and stored only as argon2id hashes.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use sqlx::SqlitePool;

use crate::cli::{UserAddArgs, UserArgs, UserCommand, UserListArgs, UserRemoveArgs};
use crate::{db, paths};

/// Dispatch the `favetto user` subcommands.
pub async fn run(args: UserArgs) -> anyhow::Result<()> {
    match args.command {
        UserCommand::Add(args) => set_password(args).await,
        UserCommand::Passwd(args) => set_password(args).await,
        UserCommand::List(args) => list(args).await,
        UserCommand::Remove(args) => remove(args).await,
    }
}

/// Open (creating and migrating) the daemon database in `data_dir`.
async fn open_db(data_dir: Option<PathBuf>) -> anyhow::Result<SqlitePool> {
    let dir = paths::expand_tilde(data_dir.unwrap_or_else(crate::cli::default_data_dir));
    tokio::fs::create_dir_all(&dir).await?;
    let pool = db::open(&dir.join("favetto.db")).await?;
    db::migrate(&pool).await?;
    Ok(pool)
}

/// Create a user or reset an existing user's password.
async fn set_password(args: UserAddArgs) -> anyhow::Result<()> {
    let password = prompt_password(&format!("Password for {}: ", args.name))?;
    let hash = favetto_core::auth::hash_password(&password)?;
    let pool = open_db(args.data_dir).await?;
    db::upsert_user(&pool, &args.name, &hash).await?;
    println!("password set for user {}", args.name);
    Ok(())
}

/// List the configured usernames (never their hashes).
async fn list(args: UserListArgs) -> anyhow::Result<()> {
    let pool = open_db(args.data_dir).await?;
    let users = db::list_users(&pool).await?;
    if users.is_empty() {
        println!("no users configured; add one with `favetto user add <name>`");
    }
    for user in users {
        println!("{}", user.username);
    }
    Ok(())
}

/// Remove a user, erroring when the account does not exist.
async fn remove(args: UserRemoveArgs) -> anyhow::Result<()> {
    let pool = open_db(args.data_dir).await?;
    if db::remove_user(&pool, &args.name).await? {
        println!("removed user {}", args.name);
        Ok(())
    } else {
        anyhow::bail!("no such user: {}", args.name)
    }
}

/// Read a password from stdin, prompting on stderr so stdout stays scriptable.
fn prompt_password(prompt: &str) -> anyhow::Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        anyhow::bail!("no password provided on stdin");
    }
    let password = line.trim_end_matches(['\n', '\r']).to_string();
    if password.is_empty() {
        anyhow::bail!("password must not be empty");
    }
    Ok(password)
}
