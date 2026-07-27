//! Reading a [`ContextConfig`] from a configuration file.

use archivindex_wbm_json::context::ContextConfig;
use std::path::Path;

/// Errors reading a [`ContextConfig`] file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The file did not parse as TOML.
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    /// The file did not parse as JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Reads a configuration file, chosen by extension: `.json` is parsed as JSON, anything else as
/// TOML.
///
/// # Errors
///
/// Returns [`ConfigError::Io`] if the file cannot be read, or [`ConfigError::Toml`] or
/// [`ConfigError::Json`] if it does not parse.
pub fn read_context_config<P: AsRef<Path>>(path: P) -> Result<ContextConfig, ConfigError> {
    let source = std::fs::read_to_string(&path)?;

    if path.as_ref().extension().is_some_and(|ext| ext == "json") {
        Ok(serde_json::from_str(&source)?)
    } else {
        Ok(toml::from_str(&source)?)
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, read_context_config};

    /// A `.json` file is parsed as JSON and anything else as TOML, and both produce the same
    /// configuration.
    #[test]
    fn reads_json_and_toml_by_extension() {
        let dir = tempfile::tempdir().expect("tempdir");

        let json_path = dir.path().join("context.json");
        std::fs::write(
            &json_path,
            r#"{"closing_whitespace": "\n", "url_query": "content.url"}"#,
        )
        .expect("write JSON");

        let toml_path = dir.path().join("context.toml");
        std::fs::write(
            &toml_path,
            "closing_whitespace = \"\\n\"\nurl_query = \"content.url\"\n",
        )
        .expect("write TOML");

        let from_json = read_context_config(&json_path).expect("valid JSON configuration");
        let from_toml = read_context_config(&toml_path).expect("valid TOML configuration");

        assert_eq!(from_json.closing_whitespace(), "\n");
        assert_eq!(
            from_json.closing_whitespace(),
            from_toml.closing_whitespace()
        );
        assert_eq!(from_json.url_query(), Some("content.url"));
        assert_eq!(from_json.url_query(), from_toml.url_query());
    }

    /// A file that does not parse under the format its extension selects is reported as a parse
    /// error, not silently retried under the other one.
    #[test]
    fn reports_parse_errors() {
        let dir = tempfile::tempdir().expect("tempdir");

        let json_path = dir.path().join("context.json");
        std::fs::write(&json_path, "closing_whitespace = \"\\n\"\n").expect("write file");

        assert!(matches!(
            read_context_config(&json_path),
            Err(ConfigError::Json(_))
        ));

        let toml_path = dir.path().join("context.toml");
        std::fs::write(&toml_path, r#"{"closing_whitespace": "\n"}"#).expect("write file");

        assert!(matches!(
            read_context_config(&toml_path),
            Err(ConfigError::Toml(_))
        ));
    }

    /// A missing file is an I/O error.
    #[test]
    fn reports_missing_files() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert!(matches!(
            read_context_config(dir.path().join("absent.toml")),
            Err(ConfigError::Io(_))
        ));
    }
}
