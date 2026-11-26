use super::roster::{self, Entry};
use super::unix::{
    checked_target, current_selection, profile_names, prompt_name, registered,
    save_selection_identity,
};
use anyhow::{Context, Result, bail, ensure};
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;

const SESSION_FLAGS: [&str; 7] = [
    "-c",
    "--continue",
    "-r",
    "--resume",
    "--session-id",
    "--from-pr",
    "--teleport",
];

fn set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn confirm_outside_claude_code() -> Result<()> {
    if !set("CLAUDECODE") {
        return Ok(());
    }
    ensure!(
        std::io::stdin().is_terminal(),
        "CLAUDECODE is set, so a Claude Code session may still be open; quit it and run switch from a plain terminal"
    );
    eprint!(
        "CLAUDECODE is set, so a Claude Code session may still be open. Quit it before resuming. Type yes to continue: "
    );
    std::io::stderr().flush()?;
    let mut response = String::new();
    std::io::stdin().lock().take(65).read_line(&mut response)?;
    ensure!(
        response == "yes\n" || response == "yes\r\n",
        "switch cancelled"
    );
    Ok(())
}

fn next_target(entries: &[Entry], current: &str) -> Option<String> {
    let start = entries
        .iter()
        .position(|entry| entry.name == current)
        .map_or(0, |index| index + 1);
    (0..entries.len())
        .map(|offset| &entries[(start + offset) % entries.len()])
        .find(|entry| {
            entry.name != current
                && !roster::signed_out(entry)
                && !roster::same_account(entries, current, &entry.name)
        })
        .map(|entry| entry.name.clone())
}

fn names_session(arg: &OsString) -> bool {
    let bytes = arg.as_bytes();
    SESSION_FLAGS.iter().any(|flag| {
        let flag = flag.as_bytes();
        bytes == flag
            || bytes
                .strip_prefix(flag)
                .is_some_and(|rest| rest.starts_with(b"="))
    })
}

fn launch_arguments(args: Vec<OsString>) -> (Vec<OsString>, bool) {
    if args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(names_session)
    {
        return (args, false);
    }
    let mut launch = vec![OsString::from("--continue")];
    launch.extend(args);
    (launch, true)
}

fn retry_hint(target: &str, continued: bool, selection: Result<String>) -> String {
    match selection {
        Ok(current) if current == target => {
            let command = if continued {
                "duolane claude --continue"
            } else {
                "duolane claude"
            };
            format!(
                "{target} is now selected; retry with `{command}` and the same Claude arguments"
            )
        }
        Ok(_) => format!(
            "selection changed; nothing was launched; run `duolane accounts switch {target}` again with the same Claude arguments"
        ),
        Err(_) => {
            format!(
                "could not verify selection; run `duolane accounts switch {target}` again with the same Claude arguments"
            )
        }
    }
}

pub fn switch(
    name: Option<String>,
    next: bool,
    args: Vec<OsString>,
    expected: Option<String>,
) -> Result<()> {
    ensure!(
        !(name.is_some() && next),
        "give a profile name or --next, not both"
    );
    ensure!(
        !set("CLAUDE_CONFIG_DIR"),
        "CLAUDE_CONFIG_DIR is set, so duolane claude would ignore the selection"
    );
    ensure!(
        !args.iter().take_while(|arg| *arg != "--").any(|arg| {
            let bytes = arg.as_bytes();
            bytes == b"--environment" || bytes.starts_with(b"--environment=")
        }),
        "--environment is unsupported with switch"
    );
    confirm_outside_claude_code()?;
    let current = current_selection()?;
    let profiles = registered()?;
    let names = profile_names(&profiles);
    let mut entries = None;
    let target = if let Some(name) = name {
        checked_target(&name, &names)?;
        name
    } else if next {
        let roster = roster::entries(&profiles)?;
        let target =
            next_target(&roster, &current).context("no other profile with a different account")?;
        entries = Some(roster);
        target
    } else {
        let Some(name) = prompt_name(&profiles, &current)? else {
            println!("Selection unchanged.");
            return Ok(());
        };
        name
    };
    if target == current {
        bail!("{target} is already selected");
    }
    let entries = match entries {
        Some(entries) => entries,
        None => roster::entries(&profiles)?,
    };
    let summary = roster::describe(&entries, &target).0;
    save_selection_identity(&target, Some(&current), expected.as_deref())?;
    match summary {
        Some(summary) => println!("Switched to {target} ({summary})."),
        None => println!("Switched to {target}."),
    }
    std::io::stdout().flush()?;
    let (arguments, continued) = launch_arguments(args);
    crate::intercept::run_claude(arguments, Some(&target))
        .with_context(|| retry_hint(&target, continued, current_selection()))
}

#[cfg(test)]
mod tests {
    use super::{launch_arguments, retry_hint};
    use std::ffi::OsString;

    fn run(args: &[&str]) -> Vec<String> {
        launch_arguments(args.iter().map(OsString::from).collect())
            .0
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn claude_accounts_switch_conflict_retry_uses_switch() {
        let hint = retry_hint("backup", true, Ok("other".into()));
        assert!(hint.contains("selection changed"));
        assert!(hint.contains("nothing was launched"));
        assert!(hint.contains("duolane accounts switch backup"));
        assert!(hint.contains("same Claude arguments"));
        let unreadable = retry_hint("backup", true, Err(anyhow::anyhow!("unreadable")));
        assert!(unreadable.contains("duolane accounts switch backup"));
        assert!(unreadable.contains("same Claude arguments"));
        assert!(!hint.contains("is now selected"));
        assert!(!hint.contains("duolane claude"));
        let hint = retry_hint("backup", false, Ok("backup".into()));
        assert!(hint.contains("duolane claude`"));
        assert!(!hint.contains("--continue"));
    }

    #[test]
    fn claude_accounts_switch_adds_continue_only_without_session_flag() {
        assert_eq!(run(&["--model", "sol"]), ["--continue", "--model", "sol"]);
        assert_eq!(run(&["--", "-r"]), ["--continue", "--", "-r"]);
        assert_eq!(run(&["--resumed"]), ["--continue", "--resumed"]);
        for kept in [
            &["-r", "id"][..],
            &["--resume=id"],
            &["--session-id", "id"],
            &["-c"],
            &["--teleport"],
        ] {
            assert_eq!(run(kept), kept);
        }
    }
}
