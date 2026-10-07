use std::{collections::HashMap, fmt, path::Path};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironmentLoadError {
    Read,
    Parse,
}

impl fmt::Display for EnvironmentLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Read => "the environment file could not be read",
            Self::Parse => "the environment file is invalid",
        })
    }
}

impl std::error::Error for EnvironmentLoadError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironmentValueError {
    InvalidUnicode,
}

pub struct StartupEnvironment {
    file_values: HashMap<String, String>,
}

impl fmt::Debug for StartupEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StartupEnvironment")
            .field(
                "file_values",
                &format_args!("[{} CONFIGURED]", self.file_values.len()),
            )
            .finish()
    }
}

impl StartupEnvironment {
    pub fn load(path: Option<&Path>) -> Result<Self, EnvironmentLoadError> {
        let Some(path) = path else {
            return Ok(Self {
                file_values: HashMap::new(),
            });
        };
        let iterator = dotenvy::from_path_iter(path).map_err(|_| EnvironmentLoadError::Read)?;
        let mut file_values = HashMap::new();
        for entry in iterator {
            let (name, value) = entry.map_err(|_| EnvironmentLoadError::Parse)?;
            file_values.insert(name, value);
        }
        Ok(Self { file_values })
    }

    /// Process values deliberately override file defaults.
    pub fn read(&self, name: &str) -> Result<Option<String>, EnvironmentValueError> {
        match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(self.file_values.get(name).cloned()),
            Err(std::env::VarError::NotUnicode(_)) => Err(EnvironmentValueError::InvalidUnicode),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn loads_dotenv_values_without_disclosing_them() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("server.env");
        fs::write(
            &path,
            "WORKCELL_WEBSEARCH_BACKEND=exa\nEXA_API_KEY='secret-canary'\nWORKCELL_LOG_LEVEL=debug\n",
        )
        .expect("write dotenv fixture");

        let environment = StartupEnvironment::load(Some(&path)).expect("load dotenv fixture");
        assert_eq!(
            environment.read("WORKCELL_WEBSEARCH_BACKEND").unwrap(),
            Some("exa".to_owned())
        );
        assert_eq!(
            environment.read("WORKCELL_LOG_LEVEL").unwrap(),
            Some("debug".to_owned())
        );
        assert!(!format!("{environment:?}").contains("secret-canary"));
        assert!(!format!("{environment:?}").contains("server.env"));
    }

    #[test]
    fn process_environment_overrides_file_values() {
        let Some((name, process_value)) = std::env::vars().next() else {
            return;
        };
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("server.env");
        fs::write(&path, format!("{name}=file-canary\n")).expect("write dotenv fixture");

        let environment = StartupEnvironment::load(Some(&path)).expect("load dotenv fixture");
        assert_eq!(environment.read(&name).unwrap(), Some(process_value));
    }

    #[test]
    fn parses_strong_quoted_secret_values() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("server.env");
        fs::write(
            &path,
            "WORKCELL_MCP_HTTP_TOKEN='private-password-canary-$-\"-'\\''-\\-value'\n",
        )
        .expect("write dotenv fixture");

        let environment = StartupEnvironment::load(Some(&path)).expect("load dotenv fixture");
        assert_eq!(
            environment.read("WORKCELL_MCP_HTTP_TOKEN").unwrap(),
            Some("private-password-canary-$-\"-'-\\-value".to_owned())
        );
    }

    #[test]
    fn reports_bounded_file_errors() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing = directory.path().join("missing-secret-canary.env");
        let error = StartupEnvironment::load(Some(&missing)).unwrap_err();
        assert_eq!(error, EnvironmentLoadError::Read);
        assert!(!error.to_string().contains("canary"));

        let malformed = directory.path().join("malformed.env");
        fs::write(&malformed, "INVALID='unterminated\n").expect("write malformed fixture");
        assert_eq!(
            StartupEnvironment::load(Some(&malformed)).unwrap_err(),
            EnvironmentLoadError::Parse
        );
    }
}
