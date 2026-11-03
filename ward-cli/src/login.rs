// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward login` and `ward logout`: registry credentials in Docker's
//! `config.json`, which the daemon reads when it pulls.

use std::io::Read;

use anyhow::{Result, bail};
use ward_core::pb::CheckRegistryLoginRequest;
use ward_core::registry_auth::{self, Credentials, DOCKER_HUB_SERVER};

use crate::client;

#[derive(clap::Args, Debug)]
pub struct LoginArgs {
    /// Username.
    #[arg(short, long)]
    pub username: Option<String>,
    /// Password. Visible in the process list; prefer --password-stdin.
    #[arg(short, long)]
    pub password: Option<String>,
    /// Read the password from standard input.
    #[arg(long)]
    pub password_stdin: bool,
    /// Registry host. Docker Hub when omitted.
    pub server: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct LogoutArgs {
    /// Registry host. Docker Hub when omitted.
    pub server: Option<String>,
}

fn credentials(args: &LoginArgs) -> Result<Credentials> {
    let Some(username) = args.username.clone().filter(|u| !u.is_empty()) else {
        bail!("a username is required: pass --username");
    };
    let password = match (&args.password, args.password_stdin) {
        (Some(_), true) => bail!("--password and --password-stdin are mutually exclusive"),
        (Some(password), false) => {
            eprintln!("WARNING! Using --password via the CLI is insecure. Use --password-stdin.");
            password.clone()
        }
        (None, true) => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text.trim_end_matches(['\n', '\r']).to_string()
        }
        (None, false) => bail!("a password is required: pass --password-stdin"),
    };
    if password.is_empty() {
        bail!("the password is empty");
    }
    Ok(Credentials { username, password })
}

pub async fn login(socket: &str, args: LoginArgs) -> Result<()> {
    let credentials = credentials(&args)?;
    let server = args.server.as_deref().unwrap_or(DOCKER_HUB_SERVER);
    let mut c = client::connect(socket).await?;
    c.check_registry_login(CheckRegistryLoginRequest {
        server: server.to_string(),
        username: credentials.username.clone(),
        password: credentials.password.clone(),
    })
    .await?;
    registry_auth::store(server, &credentials)?;
    println!("Login Succeeded");
    Ok(())
}

pub fn logout(args: &LogoutArgs) -> Result<()> {
    let server = args.server.as_deref().unwrap_or(DOCKER_HUB_SERVER);
    if registry_auth::erase(server)? {
        println!("Removing login credentials for {server}");
    } else {
        println!("Not logged in to {server}");
    }
    Ok(())
}
