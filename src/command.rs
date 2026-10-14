//! The things the phone can be asked to do, and the names they answer to.
//!
//! Every feature of the phone is reachable under two names: as the target of a
//! short code in `[numbers]` (what the rotary dial produces) and as a
//! `POST /api/action/<name>` URL (what the web page posts). Those names, the
//! list the configuration offers as suggestions, and the place that carries the
//! command out used to be four separate tables, so a feature had to be added in
//! four places and a name mistyped in one of them simply did not exist.
//!
//! One enum with a name per direction keeps them together: adding a command is
//! adding a variant, and the dial name, the web name and the dispatch arm come
//! with it.

/// What the phone can do on request.
///
/// Every variant is a command; anything else in `[numbers]` is a number to dial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Switch night mode - and with it the do-not-disturb - on or off.
    NightMode,
    /// Start or stop the sleep music.
    SleepMusic,
    /// Play the loudspeaker test tone.
    TestLoudspeaker,
    /// Play the earpiece test tone.
    TestEarpiece,
    /// Restart the machine.
    Reboot,
    /// Power the machine down.
    Shutdown,
}

impl Command {
    /// Every command, in the order the schema lists them.
    pub const ALL: [Command; 6] = [
        Command::NightMode,
        Command::SleepMusic,
        Command::TestLoudspeaker,
        Command::TestEarpiece,
        Command::Reboot,
        Command::Shutdown,
    ];

    /// The name this command has as the target of a short code in `[numbers]`.
    pub const fn dial_name(self) -> &'static str {
        match self {
            Command::NightMode => "enable-night-mode",
            Command::SleepMusic => "play-sleep-music",
            Command::TestLoudspeaker => "test-loudspeaker",
            Command::TestEarpiece => "test-earpiece",
            Command::Reboot => "reboot",
            Command::Shutdown => "shutdown",
        }
    }

    /// The name this command has in a `POST /api/action/<name>` URL.
    pub const fn web_name(self) -> &'static str {
        match self {
            Command::NightMode => "night-mode",
            Command::SleepMusic => "sleep-music",
            Command::TestLoudspeaker => "test-loudspeaker",
            Command::TestEarpiece => "test-earpiece",
            Command::Reboot => "reboot",
            Command::Shutdown => "shutdown",
        }
    }

    /// The command a short code target stands for, if it is one.
    ///
    /// `None` means the entry is a number, which is the only other thing
    /// `[numbers]` may hold.
    pub fn from_dial(name: &str) -> Option<Command> {
        Command::ALL.into_iter().find(|command| command.dial_name() == name)
    }

    /// The command an action URL stands for, if it is one.
    pub fn from_web(name: &str) -> Option<Command> {
        Command::ALL.into_iter().find(|command| command.web_name() == name)
    }
}

/// The short code targets that are commands rather than numbers.
///
/// This is what the configuration offers as a suggestion for `[numbers]` and
/// accepts as a value; it is derived from [`Command`] so it cannot drift from
/// the names the dial actually understands.
pub const DIAL_NAMES: [&str; 6] = [
    Command::NightMode.dial_name(),
    Command::SleepMusic.dial_name(),
    Command::TestLoudspeaker.dial_name(),
    Command::TestEarpiece.dial_name(),
    Command::Reboot.dial_name(),
    Command::Shutdown.dial_name(),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dial_names_round_trip() {
        for command in Command::ALL {
            assert_eq!(Command::from_dial(command.dial_name()), Some(command));
        }
    }

    #[test]
    fn the_web_names_round_trip() {
        for command in Command::ALL {
            assert_eq!(Command::from_web(command.web_name()), Some(command));
        }
    }

    #[test]
    fn a_number_is_not_a_command() {
        // The one thing that is *not* a command has to stay unrecognised: it is
        // the number the dial is about to place.
        assert_eq!(Command::from_dial("0891234"), None);
        assert_eq!(Command::from_dial(""), None);
        assert_eq!(Command::from_web("unknown"), None);
    }

    #[test]
    fn the_suggested_dial_names_are_the_commands_own_names() {
        // `[numbers]` accepts and suggests these, so they have to be exactly the
        // names `from_dial` understands - same set, same order, no more.
        let derived: Vec<&str> = Command::ALL.iter().map(|c| c.dial_name()).collect();
        assert_eq!(derived, DIAL_NAMES.to_vec());
    }

    #[test]
    fn no_name_is_used_twice() {
        // A duplicate would make one of the two commands unreachable: the first
        // match wins, and the other never fires.
        for command in Command::ALL {
            for other in Command::ALL {
                if command == other {
                    continue;
                }
                assert_ne!(
                    command.dial_name(),
                    other.dial_name(),
                    "{} und {} teilen sich einen Kurzwahlnamen",
                    command.dial_name(),
                    other.dial_name()
                );
                assert_ne!(
                    command.web_name(),
                    other.web_name(),
                    "{} und {} teilen sich einen Aktionsnamen",
                    command.web_name(),
                    other.web_name()
                );
            }
        }
    }
}