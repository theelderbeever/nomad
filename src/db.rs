use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};

use crate::client;
use crate::config::ConnectOptions;

pub async fn create(opts: &ConnectOptions) -> Result<()> {
    let ch = client::connect_without_db(opts);
    client::execute(&ch, &format!("CREATE DATABASE IF NOT EXISTS `{}`", opts.database))
        .await
        .with_context(|| format!("failed to create database {:?}", opts.database))?;
    println!("created database {:?}", opts.database);
    Ok(())
}

pub async fn drop(opts: &ConnectOptions, yes: bool) -> Result<()> {
    if !yes && !confirm(&format!("Are you sure you want to drop the database {:?}?", opts.database))? {
        bail!("aborted");
    }
    let ch = client::connect_without_db(opts);
    client::execute(&ch, &format!("DROP DATABASE IF EXISTS `{}`", opts.database))
        .await
        .with_context(|| format!("failed to drop database {:?}", opts.database))?;
    println!("dropped database {:?}", opts.database);
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} (y/N) ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}
