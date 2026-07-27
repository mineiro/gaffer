//! gaffer — command-line control for the gaffer daemon.
//!
//! Talks D-Bus and nothing else: it never touches a light directly, so there is
//! exactly one implementation of the protocol and one owner of the state.
//! Because the daemon is D-Bus-activated, no command here needs it to be
//! running first.
//!
//! This binary is also the adapter to the shell world — the thing you bind to a
//! key in `hyprland.conf`, and the thing Waybar runs to fill a panel module.

mod proxy;
mod render;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::Write;

use futures_util::StreamExt;

use crate::proxy::{LightInfo, ManagerProxy};

#[derive(Parser, Debug)]
#[command(
    name = "gaffer",
    version,
    about = "Control Elgato Key Lights through the gaffer daemon",
    after_help = "\
VALUES
  Suffixes carry the unit, so order does not matter:
    42%      set brightness          +10%   raise brightness
    4200k    set colour temperature  -200k  cool by 200K
    on off toggle

SELECTORS
  A name substring (`left`), a hardware id, or `all`. Defaults to `all`.

EXAMPLES
  gaffer set left 42% 4200k     gaffer on 60%
  gaffer toggle                 gaffer set all -10%"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show every known light (the default).
    List {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Apply values to the lights a selector matches.
    Set {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Power on, optionally applying values in the same command.
    On {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Power off.
    Off {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Flip power. On a group, "all on" flips off and anything else flips on.
    Toggle {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Make the hardware blink so you can tell which light is which.
    Identify { selector: Option<String> },
    /// Gang lights so they move as one instrument, keeping their current
    /// brightness difference.
    Link {
        /// Snap every lamp onto the first one instead of keeping the
        /// differences. Destructive, which is why it is not the default.
        #[arg(long)]
        mirror: bool,
        #[arg(required = true, num_args = 1..)]
        selectors: Vec<String>,
    },
    /// Break the gang a light belongs to.
    Unlink { selector: Option<String> },
    /// Re-probe the network now.
    Rescan,
    /// Stream state changes until interrupted.
    Watch {
        /// Emit Waybar's `custom/` module JSON, one object per line.
        #[arg(long)]
        waybar: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let connection = proxy::connect().await?;

    match cli.command.unwrap_or(Command::List { json: false }) {
        Command::List { json } => {
            let lights = proxy::lights(&connection).await?;
            print!("{}", if json { render::json(&lights) + "\n" } else { render::table(&lights) });
        }
        Command::Set { args } => apply(&connection, &args, None).await?,
        Command::On { args } => apply(&connection, &args, Some("on")).await?,
        Command::Off { args } => apply(&connection, &args, Some("off")).await?,
        Command::Toggle { args } => apply(&connection, &args, Some("toggle")).await?,
        Command::Identify { selector } => {
            let manager = ManagerProxy::new(&connection).await?;
            let selector = selector.unwrap_or_else(|| "all".into());
            let matched = manager
                .identify(&selector)
                .await
                .context("asking the daemon to identify lights")?;
            if matched.is_empty() {
                bail!("no light matches `{selector}`");
            }
        }
        Command::Link { mirror, selectors } => {
            let manager = ManagerProxy::new(&connection).await?;
            let first = selectors.first().cloned().unwrap_or_default();
            // Several selectors gang what they collectively match, so both
            // `gaffer link left right` and `gaffer link key` read naturally.
            let members = manager
                .link(selectors)
                .await
                .map_err(user_facing)
                .context("asking the daemon to gang lights")?;
            if mirror {
                // Applied after ganging, so the gang's reference — the first
                // selector — is the lamp everything snaps onto.
                manager.set_link_mode(&first, "mirror").await.map_err(user_facing)?;
            }
            println!("ganged {} lights: {}", members.len(), members.join(", "));
        }
        Command::Unlink { selector } => {
            let manager = ManagerProxy::new(&connection).await?;
            let selector = selector.unwrap_or_else(|| "all".into());
            let affected = manager.unlink(&selector).await.map_err(user_facing)?;
            if affected.is_empty() {
                bail!("`{selector}` matched no ganged light");
            }
            println!("unganged {} lights", affected.len());
        }
        Command::Rescan => {
            let manager = ManagerProxy::new(&connection).await?;
            manager.rescan().await.context("asking the daemon to rescan")?;
        }
        Command::Watch { waybar } => watch(&connection, waybar).await?,
    }

    Ok(())
}

/// Send a command, splitting the selector from the value tokens.
async fn apply(
    connection: &zbus::Connection,
    args: &[String],
    implied_power: Option<&str>,
) -> Result<()> {
    let (selector, mut tokens) = split_selector(args);
    if let Some(power) = implied_power {
        tokens.insert(0, power.to_string());
    }
    if tokens.is_empty() {
        bail!("nothing to set — try `gaffer set {selector} 42%`");
    }

    let manager = ManagerProxy::new(connection).await?;
    let matched = manager.set_state(&selector, tokens).await.map_err(user_facing)?;

    if matched.is_empty() {
        bail!("no light matches `{selector}`");
    }
    Ok(())
}

/// Unwrap a bad-argument reply into a plain message.
///
/// A mistyped value is a user error, not a transport failure, and burying it
/// under "asking the daemon to apply the change / Caused by: org.freedesktop.
/// DBus.Error.InvalidArgs:" helps nobody at a terminal.
fn user_facing(error: zbus::Error) -> anyhow::Error {
    if let zbus::Error::MethodError(name, Some(detail), _) = &error
        && name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs"
    {
        return anyhow::anyhow!("{detail}");
    }
    anyhow::Error::new(error).context("asking the daemon to apply the change")
}

/// Decide whether the first argument is a selector or already a value.
///
/// `gaffer on 42%` and `gaffer on left 42%` should both work, so a leading
/// argument that parses as a value token is treated as one, and the selector
/// defaults to `all`.
fn split_selector(args: &[String]) -> (String, Vec<String>) {
    match args.split_first() {
        None => ("all".to_string(), Vec::new()),
        Some((first, _)) if gaffer_core::parse_token(first).is_ok() => {
            ("all".to_string(), args.to_vec())
        }
        Some((first, rest)) => (first.clone(), rest.to_vec()),
    }
}

/// Print state on every change until interrupted.
///
/// Subscribes to `PropertiesChanged` on every light plus the group object, and
/// to the object manager so lights appearing or disappearing re-arm the set.
/// On any signal the whole world is re-read rather than deltas being applied
/// incrementally: with a handful of lights the round trip is free, and a
/// re-read cannot drift out of sync with the daemon.
async fn watch(connection: &zbus::Connection, waybar: bool) -> Result<()> {
    die_with_parent();

    let render = |lights: &[LightInfo]| {
        if waybar { render::waybar(lights) } else { render::line(lights) }
    };

    let object_manager = proxy::object_manager(connection).await?;
    let mut added = object_manager.receive_interfaces_added().await?;
    let mut removed = object_manager.receive_interfaces_removed().await?;

    let mut lights = proxy::lights(connection).await?;
    let mut previous = render(&lights);
    println!("{previous}");

    let mut changes = proxy::property_changes(connection, &lights).await?;

    loop {
        let rearm = tokio::select! {
            Some(_) = changes.next() => false,
            Some(_) = added.next() => true,
            Some(_) = removed.next() => true,
            else => break,
        };

        lights = proxy::lights(connection).await?;
        if rearm {
            changes = proxy::property_changes(connection, &lights).await?;
        }

        // The daemon emits one signal per changed property; collapsing on the
        // rendered form keeps a slider drag from flooding the panel.
        let rendered = render(&lights);
        if rendered != previous {
            // Rust ignores SIGPIPE, so a closed reader surfaces as an error
            // here rather than a signal. Leave quietly: the consumer is gone,
            // which is a reason to stop, not to fail.
            if writeln!(std::io::stdout(), "{rendered}").is_err() {
                return Ok(());
            }
            previous = rendered;
        }
    }

    Ok(())
}

/// Ask the kernel to kill us when whoever started us dies.
///
/// `watch` runs forever by design, and a status bar spawns it as a child. When
/// the bar restarts, the old `watch` is reparented to init and keeps a D-Bus
/// subscription open — it never notices, because it only writes when the
/// rendering changes and a blocked process cannot discover a dead reader. One
/// leaks per bar restart.
///
/// `PR_SET_PDEATHSIG` closes that: the kernel signals us the moment the parent
/// goes. Best-effort — a failure here is not worth refusing to run over.
fn die_with_parent() {
    use rustix::process::{Signal, set_parent_process_death_signal};

    let _ = set_parent_process_death_signal(Some(Signal::TERM));

    // PDEATHSIG only fires on *future* parent death, so a process that was
    // already orphaned before it got here would still linger.
    if rustix::process::getppid().is_none() {
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_leading_name_is_taken_as_the_selector() {
        let (selector, tokens) = split_selector(&owned(&["left", "42%"]));
        assert_eq!(selector, "left");
        assert_eq!(tokens, owned(&["42%"]));
    }

    #[test]
    fn a_leading_value_implies_the_all_selector() {
        let (selector, tokens) = split_selector(&owned(&["42%", "4200k"]));
        assert_eq!(selector, "all");
        assert_eq!(tokens, owned(&["42%", "4200k"]));
    }

    #[test]
    fn a_leading_negative_value_is_not_mistaken_for_a_selector() {
        let (selector, tokens) = split_selector(&owned(&["-10%"]));
        assert_eq!(selector, "all");
        assert_eq!(tokens, owned(&["-10%"]));
    }

    #[test]
    fn no_arguments_selects_everything_with_nothing_to_apply() {
        let (selector, tokens) = split_selector(&[]);
        assert_eq!(selector, "all");
        assert!(tokens.is_empty());
    }

    #[test]
    fn a_bare_selector_yields_no_tokens() {
        let (selector, tokens) = split_selector(&owned(&["left"]));
        assert_eq!(selector, "left");
        assert!(tokens.is_empty());
    }

    #[test]
    fn power_words_are_values_not_selectors() {
        // `gaffer set on` means "turn everything on", not "select a light
        // whose name contains `on`".
        let (selector, tokens) = split_selector(&owned(&["on"]));
        assert_eq!(selector, "all");
        assert_eq!(tokens, owned(&["on"]));
    }

    #[test]
    fn a_hex_id_is_a_selector_not_a_value() {
        let (selector, tokens) = split_selector(&owned(&["00005e005301", "42%"]));
        assert_eq!(selector, "00005e005301");
        assert_eq!(tokens, owned(&["42%"]));
    }

    #[test]
    fn hyphenated_values_are_not_parsed_as_flags() {
        let cli = Cli::try_parse_from(["gaffer", "set", "left", "-10%"]).expect("should parse");
        match cli.command {
            Some(Command::Set { args }) => assert_eq!(args, owned(&["left", "-10%"])),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn a_bare_invocation_means_list() {
        let cli = Cli::try_parse_from(["gaffer"]).expect("should parse");
        assert!(cli.command.is_none());
    }
}
