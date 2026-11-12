// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

#![deny(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

// -- Error type --------------------------------------------------------

/// Errors that can occur during configuration parsing.
#[derive(Debug)]
pub enum ConfigError {
    /// File not found or unreadable.
    Io(std::io::Error),
    /// A required section is missing.
    MissingSection(String),
    /// A key in a section has an invalid value.
    InvalidValue {
        section: String,
        key: String,
        value: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "config I/O error: {err}"),
            Self::MissingSection(name) => write!(f, "missing config section [{name}]"),
            Self::InvalidValue {
                section,
                key,
                value,
            } => {
                write!(f, "invalid value in [{section}] {key} = {value}")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

// -- Transfer syntax mapping -------------------------------------------

/// Maps human-readable transfer syntax names to DICOM UIDs.
pub type TransferSyntaxMap = HashMap<&'static str, &'static str>;

/// Build the static transfer syntax name -> UID mapping.
#[must_use]
pub fn transfer_syntax_names() -> TransferSyntaxMap {
    let mut m = HashMap::new();
    m.insert("IMPLICIT_VR_LE", "1.2.840.10008.1.2");
    m.insert("EXPLICIT_VR_LE", "1.2.840.10008.1.2.1");
    m.insert("JPEG_BASELINE", "1.2.840.10008.1.2.4.50");
    m.insert("JPEG_LOSSLESS", "1.2.840.10008.1.2.4.70");
    m.insert("JPEG_LS", "1.2.840.10008.1.2.4.80");
    m.insert("JPEG2000_LOSSLESS", "1.2.840.10008.1.2.4.90");
    m.insert("RLE", "1.2.840.10008.1.2.5");
    m
}

// -- Configuration structs ---------------------------------------------

/// Anonymization profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnonymizerProfile {
    Basic,
    RetainLongitudinal,
    RetainDevice,
    RetainInstitution,
    Custom,
}

impl fmt::Display for AnonymizerProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Basic => write!(f, "1=Basic"),
            Self::RetainLongitudinal => write!(f, "2=RetainLongitudinal"),
            Self::RetainDevice => write!(f, "3=RetainDevice"),
            Self::RetainInstitution => write!(f, "4=RetainInstitution"),
            Self::Custom => write!(f, "5=Custom"),
        }
    }
}

/// Input adapter configuration.
#[derive(Debug, Clone)]
pub struct InputConfig {
    /// Input mode: "dimse" or "dicomweb".
    pub mode: String,
    /// AE title for DIMSE SCP - modalities must use this as called AE (default: CLTRANSCODE).
    pub dimse_ae_title: String,
    /// Port for DIMSE SCP listener (default: 8106).
    pub dimse_port: u16,
    /// Port for DICOMweb HTTP listener (default: 8107).
    pub dicomweb_port: u16,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            mode: "dimse".into(),
            dimse_ae_title: "CLTRANSCODE".into(),
            dimse_port: 8106,
            dicomweb_port: 8107,
        }
    }
}

/// Output adapter configuration.
#[derive(Debug, Clone)]
pub struct OutputConfig {
    /// Output mode: "dimse" or "dicomweb".
    pub mode: String,
    /// AE title of the DIMSE server we connect TO (default: CLBRIDGE).
    pub dimse_ae_title: String,
    /// AE title we identify AS when connecting (default: CLTRANSCODE).
    /// Must be unique per instance in multi-instance deployments.
    pub dimse_calling_ae: String,
    /// Host for DIMSE SCU (default: 127.0.0.1).
    pub dimse_host: String,
    /// Port for DIMSE SCU (default: 8104 = clbridge DIMSE port).
    pub dimse_port: u16,
    /// URL for DICOMweb HTTP client (default: http://127.0.0.1:8024/dicomweb).
    pub dicomweb_url: String,
    /// Authentication mode for DICOMweb (default: none).
    pub dicomweb_auth: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            mode: "dimse".into(),
            dimse_ae_title: "CLBRIDGE".into(),
            dimse_calling_ae: "CLTRANSCODE".into(),
            dimse_host: "127.0.0.1".into(),
            dimse_port: 8104,
            dicomweb_url: "http://127.0.0.1:8024/dicomweb".into(),
            dicomweb_auth: "none".into(),
        }
    }
}

/// Anonymizer configuration.
#[derive(Debug, Clone)]
pub struct AnonymizerConfig {
    /// Anonymization profile.
    pub profile: AnonymizerProfile,
    /// Salt for pseudonymization (default: changeme).
    pub salt: String,
}

impl Default for AnonymizerConfig {
    fn default() -> Self {
        Self {
            profile: AnonymizerProfile::Basic,
            salt: "changeme".into(),
        }
    }
}

/// Transcoding configuration.
#[derive(Debug, Clone)]
pub struct TranscodingConfig {
    /// Whether transcoding is enabled (default: true).
    pub enabled: bool,
    /// Target transfer syntax human name (default: JPEG_LS).
    pub target_ts: String,
    /// JPEG-LS encode profile: "t87" (default) | "legacygo".
    /// t87 = ITU-T T.87 conformant (Weasis CharLS decodes it);
    /// legacygo = frozen 1.0.0/Go bytes (old corpus; Weasis does NOT
    /// render them). The default is t87 (CharLS decodes it).
    pub jpegls_profile: String,
    /// JPEG-LS NEAR parameter 0..16 (0 = lossless 4.80, >0 =
    /// near-lossless 4.81). legacygo allows ONLY 0.
    pub jpegls_near: u8,
}

impl Default for TranscodingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            target_ts: "JPEG_LS".into(),
            jpegls_profile: "t87".into(),
            jpegls_near: 0,
        }
    }
}

/// Outbox (retry queue) configuration.
#[derive(Debug, Clone)]
pub struct OutboxConfig {
    /// Directory for outbox storage (default: ./cltranscode/outbox).
    pub dir: String,
    /// Maximum retry attempts (default: 3).
    pub max_retries: u32,
    /// Max files in pending/ before rejecting new input (default: 1000).
    pub max_pending: u32,
    /// Hours to keep files in done/ before cleanup (default: 24, 0 = keep forever).
    pub retention_hours: u32,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            dir: "./cltranscode/outbox".into(),
            max_retries: 3,
            max_pending: 1000,
            retention_hours: 24,
        }
    }
}

/// Logging configuration.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    /// Log level: ERROR, WARN, INFO, DEBUG, TRACE (default: INFO).
    pub level: String,
    /// Log directory for file output with rotation. Empty = no file logging (default).
    pub log_dir: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "INFO".into(),
            log_dir: String::new(),
        }
    }
}

/// Health + metrics endpoint configuration.
#[derive(Debug, Clone)]
pub struct HealthConfig {
    /// Port for /health and /metrics HTTP server (default: 8108).
    /// Must be different per instance in multi-instance deployments.
    pub port: u16,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self { port: 8108 }
    }
}

/// Top-level application configuration.
///
/// Parsed from an INI-style file with sections: `[input]`, `[output]`,
/// `[anonymizer]`, `[transcoding]`, `[outbox]`, `[logging]`, `[health]`.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub input: InputConfig,
    pub output: OutputConfig,
    pub anonymizer: AnonymizerConfig,
    pub transcoding: TranscodingConfig,
    pub outbox: OutboxConfig,
    pub logging: LoggingConfig,
    pub health: HealthConfig,
}

impl Config {
    /// Parse configuration from an INI file.
    ///
    /// # Format
    ///
    /// ```ini
    /// [input]
    /// mode = dimse
    /// dimse_port = 8106
    ///
    /// [output]
    /// mode = dimse
    /// dimse_ae_title = CLTRANSCODE
    /// ```
    ///
    /// Lines starting with `#` or `;` are treated as comments.
    /// Unknown sections and keys are silently ignored.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] if the file cannot be read.
    /// Returns [`ConfigError::MissingSection`] if a required section is absent.
    /// Returns [`ConfigError::InvalidValue`] if a numeric field cannot be parsed.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        Self::parse(reader)
    }

    /// Parse configuration from any buffered reader (useful for testing).
    fn parse(reader: impl BufRead) -> Result<Self, ConfigError> {
        let mut config = Self::default();

        // Collect raw key-value pairs grouped by section.
        let mut current_section: Option<String> = None;
        let mut sections: HashMap<String, Vec<(String, String)>> = HashMap::new();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();

            // Skip empty lines and comments.
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }

            // Section header: [section_name]
            if let Some(section_name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
            {
                current_section = Some(section_name.trim().to_lowercase());
                continue;
            }

            // Key-value pair: key = value
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim().to_lowercase();
                let value = value.trim().to_string();
                if let Some(ref sec) = current_section {
                    sections.entry(sec.clone()).or_default().push((key, value));
                }
            }
        }

        // Apply parsed sections to config.
        for (section, pairs) in &sections {
            match section.as_str() {
                "input" => config.apply_input(pairs)?,
                "output" => config.apply_output(pairs)?,
                "anonymizer" => config.apply_anonymizer(pairs)?,
                "transcoding" => config.apply_transcoding(pairs)?,
                "outbox" => config.apply_outbox(pairs)?,
                "logging" => config.apply_logging(pairs),
                "health" => config.apply_health(pairs)?,
                _ => { /* unknown section: ignore */ }
            }
        }

        Ok(config)
    }

    fn apply_input(&mut self, pairs: &[(String, String)]) -> Result<(), ConfigError> {
        for (key, value) in pairs {
            match key.as_str() {
                "mode" => self.input.mode = value.clone(),
                "dimse_ae_title" => self.input.dimse_ae_title = value.clone(),
                "dimse_port" => {
                    self.input.dimse_port = parse_u16("input", key, value)?;
                }
                "dicomweb_port" => {
                    self.input.dicomweb_port = parse_u16("input", key, value)?;
                }
                _ => { /* unknown key: ignore */ }
            }
        }
        Ok(())
    }

    fn apply_output(&mut self, pairs: &[(String, String)]) -> Result<(), ConfigError> {
        for (key, value) in pairs {
            match key.as_str() {
                "mode" => self.output.mode = value.clone(),
                "dimse_ae_title" => self.output.dimse_ae_title = value.clone(),
                "dimse_calling_ae" => self.output.dimse_calling_ae = value.clone(),
                "dimse_host" => self.output.dimse_host = value.clone(),
                "dimse_port" => {
                    self.output.dimse_port = parse_u16("output", key, value)?;
                }
                "dicomweb_url" => self.output.dicomweb_url = value.clone(),
                "dicomweb_auth" => self.output.dicomweb_auth = value.clone(),
                _ => { /* unknown key: ignore */ }
            }
        }
        Ok(())
    }

    fn apply_anonymizer(&mut self, pairs: &[(String, String)]) -> Result<(), ConfigError> {
        for (key, value) in pairs {
            match key.as_str() {
                "profile" => {
                    self.anonymizer.profile = match value.to_lowercase().as_str() {
                        "1" | "basic" => AnonymizerProfile::Basic,
                        "2" | "retainlongitudinal" => AnonymizerProfile::RetainLongitudinal,
                        "3" | "retaindevice" => AnonymizerProfile::RetainDevice,
                        "4" | "retaininstitution" => AnonymizerProfile::RetainInstitution,
                        "5" | "custom" => AnonymizerProfile::Custom,
                        other => {
                            return Err(ConfigError::InvalidValue {
                                section: "anonymizer".into(),
                                key: key.clone(),
                                value: other.into(),
                            });
                        }
                    };
                }
                "salt" => self.anonymizer.salt = value.clone(),
                _ => { /* unknown key: ignore */ }
            }
        }
        Ok(())
    }

    fn apply_transcoding(&mut self, pairs: &[(String, String)]) -> Result<(), ConfigError> {
        for (key, value) in pairs {
            match key.as_str() {
                "enabled" => {
                    self.transcoding.enabled = match value.to_lowercase().as_str() {
                        "true" | "1" | "yes" => true,
                        "false" | "0" | "no" => false,
                        other => {
                            return Err(ConfigError::InvalidValue {
                                section: "transcoding".into(),
                                key: key.clone(),
                                value: other.into(),
                            });
                        }
                    };
                }
                "target_ts" => self.transcoding.target_ts = value.clone(),
                "jpegls_profile" => {
                    self.transcoding.jpegls_profile = match value.to_lowercase().as_str() {
                        "t87" => "t87".into(),
                        "legacygo" => "legacygo".into(),
                        other => {
                            return Err(ConfigError::InvalidValue {
                                section: "transcoding".into(),
                                key: key.clone(),
                                value: other.into(),
                            });
                        }
                    };
                }
                "jpegls_near" => {
                    let near = value.parse::<u8>().map_err(|_| ConfigError::InvalidValue {
                        section: "transcoding".into(),
                        key: key.clone(),
                        value: value.clone(),
                    })?;
                    if near > 16 {
                        return Err(ConfigError::InvalidValue {
                            section: "transcoding".into(),
                            key: key.clone(),
                            value: value.clone(),
                        });
                    }
                    self.transcoding.jpegls_near = near;
                }
                _ => { /* unknown key: ignore */ }
            }
        }
        // Cross-key validation (31.08): the LegacyGo profile is frozen
        // 1.0.0 bytes and only exists for NEAR = 0.
        if self.transcoding.jpegls_profile == "legacygo" && self.transcoding.jpegls_near > 0 {
            return Err(ConfigError::InvalidValue {
                section: "transcoding".into(),
                key: "jpegls_near".into(),
                value: self.transcoding.jpegls_near.to_string(),
            });
        }
        Ok(())
    }

    fn apply_outbox(&mut self, pairs: &[(String, String)]) -> Result<(), ConfigError> {
        for (key, value) in pairs {
            match key.as_str() {
                "dir" => self.outbox.dir = value.clone(),
                "max_retries" => {
                    self.outbox.max_retries = parse_u32("outbox", key, value)?;
                }
                "max_pending" => {
                    self.outbox.max_pending = parse_u32("outbox", key, value)?;
                }
                "retention_hours" => {
                    self.outbox.retention_hours = parse_u32("outbox", key, value)?;
                }
                _ => { /* unknown key: ignore */ }
            }
        }
        Ok(())
    }

    fn apply_logging(&mut self, pairs: &[(String, String)]) {
        for (key, value) in pairs {
            match key.as_str() {
                "level" => self.logging.level = value.clone(),
                "log_dir" => self.logging.log_dir = value.clone(),
                _ => { /* unknown key: ignore */ }
            }
        }
    }

    fn apply_health(&mut self, pairs: &[(String, String)]) -> Result<(), ConfigError> {
        for (key, value) in pairs {
            match key.as_str() {
                "port" => {
                    self.health.port = parse_u16("health", key, value)?;
                }
                _ => { /* unknown key: ignore */ }
            }
        }
        Ok(())
    }
}

// -- Helpers -----------------------------------------------------------

fn parse_u16(section: &str, key: &str, value: &str) -> Result<u16, ConfigError> {
    value.parse::<u16>().map_err(|_| ConfigError::InvalidValue {
        section: section.into(),
        key: key.into(),
        value: value.into(),
    })
}

fn parse_u32(section: &str, key: &str, value: &str) -> Result<u32, ConfigError> {
    value.parse::<u32>().map_err(|_| ConfigError::InvalidValue {
        section: section.into(),
        key: key.into(),
        value: value.into(),
    })
}

// -- Tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse_str(input: &str) -> Result<Config, ConfigError> {
        Config::parse(Cursor::new(input))
    }

    #[test]
    fn parses_valid_ini() {
        let ini = r#"
[input]
mode = dimse
dimse_port = 8106
dicomweb_port = 8107

[output]
mode = dicomweb
dimse_ae_title = MYAE
dimse_host = 192.168.1.1
dimse_port = 8104
dicomweb_url = http://pacs:8042/dicomweb
dicomweb_auth = basic

[anonymizer]
profile = RetainDevice
salt = my-secret-salt

[transcoding]
enabled = false
target_ts = JPEG_BASELINE

[outbox]
dir = /var/spool/cltranscode
max_retries = 5

[logging]
level = DEBUG
format = json
log_dir = ./logs
"#;
        let cfg = parse_str(ini).unwrap();

        assert_eq!(cfg.input.mode, "dimse");
        assert_eq!(cfg.input.dimse_port, 8106);
        assert_eq!(cfg.input.dicomweb_port, 8107);

        assert_eq!(cfg.output.mode, "dicomweb");
        assert_eq!(cfg.output.dimse_ae_title, "MYAE");
        assert_eq!(cfg.output.dimse_host, "192.168.1.1");
        assert_eq!(cfg.output.dimse_port, 8104);
        assert_eq!(cfg.output.dicomweb_url, "http://pacs:8042/dicomweb");
        assert_eq!(cfg.output.dicomweb_auth, "basic");

        assert_eq!(cfg.anonymizer.profile, AnonymizerProfile::RetainDevice);
        assert_eq!(cfg.anonymizer.salt, "my-secret-salt");

        assert!(!cfg.transcoding.enabled);
        assert_eq!(cfg.transcoding.target_ts, "JPEG_BASELINE");

        assert_eq!(cfg.outbox.dir, "/var/spool/cltranscode");
        assert_eq!(cfg.outbox.max_retries, 5);

        assert_eq!(cfg.logging.level, "DEBUG");
        assert_eq!(cfg.logging.log_dir, "./logs");
    }

    #[test]
    fn empty_file_yields_defaults() {
        let cfg = parse_str("").unwrap();
        let default = Config::default();

        assert_eq!(cfg.input.mode, default.input.mode);
        assert_eq!(cfg.input.dimse_port, default.input.dimse_port);
        assert_eq!(cfg.output.dimse_port, default.output.dimse_port);
        assert_eq!(cfg.output.dicomweb_url, default.output.dicomweb_url);
        assert_eq!(cfg.anonymizer.profile, default.anonymizer.profile);
        assert_eq!(cfg.transcoding.enabled, default.transcoding.enabled);
        assert_eq!(cfg.outbox.dir, default.outbox.dir);
        assert_eq!(cfg.logging.level, default.logging.level);
    }

    #[test]
    fn comment_lines_are_ignored() {
        let ini = r#"
; this is a comment
# so is this
[input]
# mode = dicomweb   <-- commented out
mode = dimse
"#;
        let cfg = parse_str(ini).unwrap();
        assert_eq!(cfg.input.mode, "dimse");
    }

    #[test]
    fn unknown_section_is_ignored() {
        let ini = r#"
[unknown]
foo = bar
[input]
mode = dicomweb
"#;
        let cfg = parse_str(ini).unwrap();
        assert_eq!(cfg.input.mode, "dicomweb");
    }

    #[test]
    fn unknown_key_is_ignored() {
        let ini = r#"
[input]
mode = dimse
future_option = 42
"#;
        let cfg = parse_str(ini).unwrap();
        assert_eq!(cfg.input.mode, "dimse");
    }

    #[test]
    fn invalid_port_returns_error() {
        let ini = "[input]\ndimse_port = not_a_number\n";
        let err = parse_str(ini).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn invalid_profile_returns_error() {
        let ini = "[anonymizer]\nprofile = BogusProfile\n";
        let err = parse_str(ini).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn invalid_bool_returns_error() {
        let ini = "[transcoding]\nenabled = maybe\n";
        let err = parse_str(ini).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn transfer_syntax_mapping_contains_all_entries() {
        let ts = transfer_syntax_names();
        assert_eq!(ts.get("IMPLICIT_VR_LE"), Some(&"1.2.840.10008.1.2"));
        assert_eq!(ts.get("EXPLICIT_VR_LE"), Some(&"1.2.840.10008.1.2.1"));
        assert_eq!(ts.get("JPEG_BASELINE"), Some(&"1.2.840.10008.1.2.4.50"));
        assert_eq!(ts.get("JPEG_LOSSLESS"), Some(&"1.2.840.10008.1.2.4.70"));
        assert_eq!(ts.get("JPEG_LS"), Some(&"1.2.840.10008.1.2.4.80"));
        assert_eq!(ts.get("JPEG2000_LOSSLESS"), Some(&"1.2.840.10008.1.2.4.90"));
        assert_eq!(ts.get("RLE"), Some(&"1.2.840.10008.1.2.5"));
    }

    #[test]
    fn defaults_match_spec() {
        let cfg = Config::default();
        assert_eq!(cfg.input.dimse_ae_title, "CLTRANSCODE");
        assert_eq!(cfg.input.dimse_port, 8106);
        assert_eq!(cfg.input.dicomweb_port, 8107);
        assert_eq!(cfg.output.dimse_ae_title, "CLBRIDGE");
        assert_eq!(cfg.output.dimse_calling_ae, "CLTRANSCODE");
        assert_eq!(cfg.output.dimse_host, "127.0.0.1");
        assert_eq!(cfg.output.dimse_port, 8104);
        assert_eq!(cfg.output.dicomweb_url, "http://127.0.0.1:8024/dicomweb");
        assert_eq!(cfg.output.dicomweb_auth, "none");
        assert_eq!(cfg.anonymizer.profile, AnonymizerProfile::Basic);
        assert_eq!(cfg.anonymizer.salt, "changeme");
        assert!(cfg.transcoding.enabled);
        assert_eq!(cfg.transcoding.target_ts, "JPEG_LS");
        assert_eq!(cfg.transcoding.jpegls_profile, "t87"); // T87 is the default
        assert_eq!(cfg.transcoding.jpegls_near, 0);
        assert_eq!(cfg.outbox.dir, "./cltranscode/outbox");
        assert_eq!(cfg.outbox.max_retries, 3);
        assert_eq!(cfg.logging.level, "INFO");
        assert!(cfg.logging.log_dir.is_empty());
    }

    #[test]
    fn jpegls_keys_parsed() {
        let ini = "[transcoding]\njpegls_profile = t87\njpegls_near = 4\n";
        let cfg = parse_str(ini).unwrap();
        assert_eq!(cfg.transcoding.jpegls_profile, "t87");
        assert_eq!(cfg.transcoding.jpegls_near, 4);

        let ini = "[transcoding]\njpegls_profile = LegacyGo\njpegls_near = 0\n";
        let cfg = parse_str(ini).unwrap();
        assert_eq!(cfg.transcoding.jpegls_profile, "legacygo");
        assert_eq!(cfg.transcoding.jpegls_near, 0);
    }

    #[test]
    fn jpegls_profile_invalid_rejected() {
        let ini = "[transcoding]\njpegls_profile = bogus\n";
        let err = parse_str(ini).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn jpegls_near_out_of_range_rejected() {
        let ini = "[transcoding]\njpegls_near = 17\n";
        let err = parse_str(ini).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn jpegls_legacygo_near_nonzero_rejected() {
        // The LegacyGo profile is frozen 1.0.0 bytes and only exists
        // for NEAR = 0 (31.08 validation).
        let ini = "[transcoding]\njpegls_profile = legacygo\njpegls_near = 4\n";
        let err = parse_str(ini).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn boolean_variants_are_parsed() {
        for (input, expected) in [
            ("true", true),
            ("1", true),
            ("yes", true),
            ("True", true),
            ("false", false),
            ("0", false),
            ("no", false),
        ] {
            let ini = format!("[transcoding]\nenabled = {input}\n");
            let cfg = parse_str(&ini).unwrap();
            assert_eq!(
                cfg.transcoding.enabled, expected,
                "failed for input: {input}"
            );
        }
    }

    #[test]
    fn case_insensitive_sections_and_keys() {
        let ini = r#"
[INPUT]
MODE = DICOMWEB
DIMSE_PORT = 9999
[OUTPUT]
MODE = DIMSE
"#;
        let cfg = parse_str(ini).unwrap();
        assert_eq!(cfg.input.mode, "DICOMWEB");
        assert_eq!(cfg.input.dimse_port, 9999);
        assert_eq!(cfg.output.mode, "DIMSE");
    }

    #[test]
    fn whitespace_around_equals_is_handled() {
        let ini = "[input]\n  mode  =  dicomweb  \n";
        let cfg = parse_str(ini).unwrap();
        assert_eq!(cfg.input.mode, "dicomweb");
    }

    #[test]
    fn config_error_display() {
        let err = ConfigError::MissingSection("input".into());
        assert_eq!(err.to_string(), "missing config section [input]");

        let err = ConfigError::InvalidValue {
            section: "x".into(),
            key: "y".into(),
            value: "z".into(),
        };
        assert_eq!(err.to_string(), "invalid value in [x] y = z");
    }
}
