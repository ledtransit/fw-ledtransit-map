// The external tools xtask runs: cargo, espflash, probe-rs and git. All run in
// the working directory (the firmware repository) unless given another one
use std::{fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};

/// Environment variables added for a tool
pub type Env<'a> = &'a [(&'a str, String)];

pub fn cargo(args: &[&str], cwd: &Path, env: Env) -> Result<()> {
    run("cargo", args, cwd, env)
}

pub fn espflash(args: &[&str]) -> Result<()> {
    run("espflash", args, Path::new("."), &[])
}

pub fn probe_rs(args: &[&str]) -> Result<()> {
    run("probe-rs", args, Path::new("."), &[])
}

fn run(program: &str, args: &[&str], cwd: &Path, env: Env) -> Result<()> {
    if !cwd.is_dir() {
        bail!("The specified cwd {:?} is not a directory", cwd);
    }
    log::debug!(
        "Running `{} {}` in {:?} - Environment {:?}",
        program,
        args.join(" "),
        cwd,
        env
    );

    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .status()
        .with_context(|| format!("Couldn't run `{} {}`", program, args.join(" ")))?;
    if !status.success() {
        bail!(
            "Failed to execute {} subcommand `{} {}`",
            program,
            program,
            args.join(" ")
        );
    }
    Ok(())
}

/// Fails unless the repository is clean and its commit tagged with the
/// version (e.g. "1.1.1"), so a build is reproducible from the tag.
pub fn ensure_clean_tagged_commit(version: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .context("Failed to execute 'git status'")?;
    if !status.stdout.is_empty() {
        bail!("Git repository is not clean");
    }

    let tags = Command::new("git")
        .args(["tag", "--points-at", "HEAD"])
        .output()
        .context("Failed to execute 'git tag'")?;
    if !tags.status.success() {
        bail!("Failed to list the tags of the current commit");
    }
    let tags = String::from_utf8_lossy(&tags.stdout);
    let tags: Vec<&str> = tags.lines().map(str::trim).collect();
    if tags.is_empty() {
        bail!("Current commit is not tagged (expected tag {version})");
    }
    if !tags.contains(&version) {
        bail!(
            "Current commit is tagged {}, not with the package version {version}",
            tags.join(", ")
        );
    }
    Ok(())
}

pub fn copy(from: &Path, to: &Path) -> Result<()> {
    fs::copy(from, to)
        .map(|_| ())
        .with_context(|| format!("Failed to copy {:?} to {:?}", from, to))
}

pub fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("Path {:?} isn't valid UTF-8", path))
}
