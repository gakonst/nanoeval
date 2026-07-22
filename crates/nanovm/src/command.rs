use std::{collections::BTreeMap, ffi::OsString, path::Path};

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A command to execute as the initial process inside a libkrun guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestCommand {
    program: OsString,
    arguments: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    current_dir: OsString,
}

impl GuestCommand {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            environment: BTreeMap::from([(OsString::from("PATH"), OsString::from(DEFAULT_PATH))]),
            current_dir: OsString::from("/"),
        }
    }

    #[must_use]
    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    #[must_use]
    pub fn args<I, A>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(name.into(), value.into());
        self
    }

    #[must_use]
    pub fn current_dir(mut self, directory: impl Into<OsString>) -> Self {
        self.current_dir = directory.into();
        self
    }

    #[must_use]
    pub fn program(&self) -> &Path {
        Path::new(&self.program)
    }

    #[must_use]
    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    #[must_use]
    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    #[must_use]
    pub fn current_directory(&self) -> &Path {
        Path::new(&self.current_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_owns_guest_process_policy() {
        let command = GuestCommand::new("/bin/sh")
            .args(["-c", "pwd"])
            .env("TERM", "dumb")
            .current_dir("/workspace");

        assert_eq!(command.program(), Path::new("/bin/sh"));
        assert_eq!(
            command.arguments(),
            [OsString::from("-c"), OsString::from("pwd")]
        );
        assert_eq!(
            command.environment().get(&OsString::from("TERM")),
            Some(&OsString::from("dumb"))
        );
        assert_eq!(command.current_directory(), Path::new("/workspace"));
    }
}
