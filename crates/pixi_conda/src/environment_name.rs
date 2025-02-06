use std::fmt::Display;
use std::str::FromStr;
use thiserror::Error;

const INVALID_CHARACTERS: &[char] = &['/', '\\', ':', ',', ' '];

/// A helper type that represents a valid environment name.
///
/// An environment name can be created from a string by calling
/// [`FromStr::from_str`] or [`str::parse`].
#[derive(Debug, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct EnvironmentName(String);

impl Display for EnvironmentName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for EnvironmentName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<EnvironmentName> for String {
    fn from(name: EnvironmentName) -> String {
        name.0
    }
}

#[derive(Debug, Error)]
pub enum ParseEnvironmentNameError {
    #[error("invalid character in environment name: {0}")]
    InvalidCharacter(String),
}

impl FromStr for EnvironmentName {
    type Err = ParseEnvironmentNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(invalid_char) = s
            .matches(|c| INVALID_CHARACTERS.contains(&c) || c.is_whitespace())
            .next()
        {
            return Err(ParseEnvironmentNameError::InvalidCharacter(
                invalid_char.to_owned(),
            ));
        }

        Ok(EnvironmentName(s.to_owned()))
    }
}

impl EnvironmentName {
    /// Constructs a new environment name from a string but does not check if
    /// the name is valid.
    pub fn new_unchecked(name: String) -> Self {
        EnvironmentName(name)
    }
}
