//! CLI flag handling via flags-2-env (github.com/ORESoftware/flags-2-env).
//!
//! The contract lives in `.cli-flags.toml`: every flag maps to an environment
//! variable, and flag parsing produces an env-override map. Downstream code
//! reads configuration exclusively through [`Resolved::get`], with precedence
//! `CLI flag > process env > .cli-flags.toml default`.
//!
//! Two engines produce the override map:
//! 1. The native flags2env core (`libflags2env.dylib` / `.so`), loaded via
//!    dlopen when available — set `FLAGS2ENV_LIB` to an explicit path, or
//!    have it on the default library search path.
//! 2. A built-in pure-Rust parser of the same `.cli-flags.toml` subset
//!    (aliases, short flags, bool/string/integer types, `--flag=v`,
//!    `--flag v`, `-fV`, `--` terminator). Used when the native core is not
//!    present so the binary stays self-contained.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const EMBEDDED_CLI_FLAGS_TOML: &str = include_str!("../.cli-flags.toml");

#[derive(Debug, Deserialize, Clone)]
pub struct FlagSpec {
    pub env: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub short: Option<String>,
    #[serde(default = "default_type")]
    pub r#type: String,
    #[serde(default)]
    pub default: Option<toml::Value>,
    #[serde(default)]
    pub help: Option<String>,
}

fn default_type() -> String {
    "string".to_string()
}

#[derive(Debug, Deserialize)]
struct CliFlagsFile {
    #[serde(default)]
    flags: std::collections::BTreeMap<String, FlagSpec>,
}

pub struct FlagConfig {
    pub flags: std::collections::BTreeMap<String, FlagSpec>,
}

pub fn load_config() -> Result<FlagConfig> {
    // Prefer a project-local .cli-flags.toml (flags-2-env convention), fall
    // back to the one embedded at build time so the installed binary works
    // from any directory.
    let text = std::fs::read_to_string(".cli-flags.toml").unwrap_or_else(|_| EMBEDDED_CLI_FLAGS_TOML.to_string());
    let parsed: CliFlagsFile = toml::from_str(&text).context("invalid .cli-flags.toml")?;
    Ok(FlagConfig { flags: parsed.flags })
}

/// Parse `argv` (excluding program name and subcommand) into an env-override
/// map. Uses the native flags2env core when loadable, otherwise the built-in
/// parser. Also returns leftover positionals.
pub fn parse(config: &FlagConfig, argv: &[String]) -> Result<(HashMap<String, String>, Vec<String>)> {
    if let Some(map) = try_native(argv) {
        // Native core consumed the flags; recompute positionals with the
        // fallback tokenizer (the core reports flags only).
        let (_, positionals) = parse_fallback(config, argv)?;
        return Ok((map, positionals));
    }
    parse_fallback(config, argv)
}

// ---------------------------------------------------------------------------
// Native core (vendored thin loader, mirrors clients/rust of flags-2-env).
// ---------------------------------------------------------------------------

type ParseFn = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char;
type FreeFn = unsafe extern "C" fn(*mut c_char);

fn try_native(argv: &[String]) -> Option<HashMap<String, String>> {
    let lib_path = std::env::var("FLAGS2ENV_LIB").ok()?;
    // argv for the core must include a program-name slot.
    let mut full = vec!["dpm".to_string()];
    full.extend(argv.iter().cloned());
    let argv_json = serde_json::to_string(&full).ok()?;
    unsafe {
        let lib = libloading::Library::new(&lib_path).ok()?;
        let parse: libloading::Symbol<ParseFn> = lib.get(b"f2e_parse_json_argv_from_file").ok()?;
        let free: libloading::Symbol<FreeFn> = lib.get(b"f2e_free").ok()?;
        let config = CString::new(".cli-flags.toml").ok()?;
        let argv_c = CString::new(argv_json).ok()?;
        let result = parse(config.as_ptr(), argv_c.as_ptr());
        if result.is_null() {
            return Some(HashMap::new());
        }
        let raw = CStr::from_ptr(result).to_string_lossy().to_string();
        free(result);
        serde_json::from_str(&raw).ok()
    }
}

// ---------------------------------------------------------------------------
// Built-in fallback parser (same .cli-flags.toml contract).
// ---------------------------------------------------------------------------

fn parse_fallback(config: &FlagConfig, argv: &[String]) -> Result<(HashMap<String, String>, Vec<String>)> {
    // alias -> flag key; short -> flag key
    let mut by_alias: HashMap<String, &str> = HashMap::new();
    let mut by_short: HashMap<String, &str> = HashMap::new();
    for (key, spec) in &config.flags {
        by_alias.insert(key.clone(), key);
        for a in &spec.aliases {
            by_alias.insert(a.clone(), key);
        }
        if let Some(s) = &spec.short {
            by_short.insert(s.clone(), key);
        }
    }

    let mut env_map: HashMap<String, String> = HashMap::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut only_positionals = false;

    while i < argv.len() {
        let tok = &argv[i];
        i += 1;

        if only_positionals {
            positionals.push(tok.clone());
            continue;
        }
        if tok == "--" {
            only_positionals = true;
            continue;
        }

        let (key, inline_value): (&str, Option<String>) = if let Some(long) = tok.strip_prefix("--") {
            let (name, val) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let Some(key) = by_alias.get(name) else {
                bail!("unknown option --{name} (see `dpm help`)");
            };
            (key, val)
        } else if let Some(short) = tok.strip_prefix('-') {
            if short.is_empty() {
                positionals.push(tok.clone());
                continue;
            }
            let (s, val) = if short.len() > 1 {
                let (s, rest) = short.split_at(1);
                let rest = rest.strip_prefix('=').unwrap_or(rest);
                (s, Some(rest.to_string()))
            } else {
                (short, None)
            };
            let Some(key) = by_short.get(s) else {
                bail!("unknown option -{s} (see `dpm help`)");
            };
            (key, val)
        } else {
            positionals.push(tok.clone());
            continue;
        };

        let spec = &config.flags[key];
        let value = if spec.r#type == "bool" {
            match inline_value {
                Some(v) => normalize_bool(&v)?,
                None => "true".to_string(),
            }
        } else {
            match inline_value {
                Some(v) => v,
                None => {
                    if i >= argv.len() {
                        bail!("option --{key} requires a value");
                    }
                    let v = argv[i].clone();
                    i += 1;
                    v
                }
            }
        };
        if spec.r#type == "integer" {
            value
                .parse::<i64>()
                .with_context(|| format!("option --{key} expects an integer, got {value:?}"))?;
        }
        env_map.insert(spec.env.clone(), value);
    }

    Ok((env_map, positionals))
}

fn normalize_bool(v: &str) -> Result<String> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "t" | "1" | "yes" | "y" => Ok("true".into()),
        "false" | "f" | "0" | "no" | "n" => Ok("false".into()),
        other => bail!("invalid boolean value {other:?}"),
    }
}

/// Layered configuration: flag overrides > process env > declared defaults.
pub struct Resolved {
    overrides: HashMap<String, String>,
    defaults: HashMap<String, String>,
}

impl Resolved {
    pub fn new(config: &FlagConfig, overrides: HashMap<String, String>) -> Self {
        let mut defaults = HashMap::new();
        for spec in config.flags.values() {
            if let Some(d) = &spec.default {
                let v = match d {
                    toml::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                defaults.insert(spec.env.clone(), v);
            }
        }
        Self { overrides, defaults }
    }

    pub fn get(&self, env_key: &str) -> Option<String> {
        if let Some(v) = self.overrides.get(env_key) {
            return Some(v.clone());
        }
        if let Ok(v) = std::env::var(env_key) {
            if !v.is_empty() {
                return Some(v);
            }
        }
        self.defaults.get(env_key).cloned()
    }

    /// First non-empty value among several env keys (e.g. TARGET_DATABASE_URL
    /// falling back to DATABASE_URL).
    pub fn get_first(&self, env_keys: &[&str]) -> Option<String> {
        env_keys.iter().find_map(|k| self.get(k))
    }

    pub fn get_bool(&self, env_key: &str) -> bool {
        self.get(env_key)
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "t" | "1" | "yes" | "y"))
            .unwrap_or(false)
    }
}

/// Render the `dpm help` flag table from the declared contract.
pub fn help_table(config: &FlagConfig) -> String {
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    for (key, spec) in &config.flags {
        let mut names = vec![format!("--{key}")];
        for a in &spec.aliases {
            if a != key {
                names.push(format!("--{a}"));
            }
        }
        if let Some(s) = &spec.short {
            names.insert(0, format!("-{s}"));
        }
        let default = spec
            .default
            .as_ref()
            .map(|d| match d {
                toml::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        rows.push((
            names.join(", "),
            spec.env.clone(),
            default,
            spec.help.clone().unwrap_or_default(),
        ));
    }
    let w0 = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max("options".len());
    let w1 = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max("env".len());
    let w2 = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max("default".len());
    let mut out = String::new();
    out.push_str(&format!("  {:w0$}  {:w1$}  {:w2$}  description\n", "options", "env", "default"));
    for (a, b, c, d) in rows {
        out.push_str(&format!("  {a:w0$}  {b:w1$}  {c:w2$}  {d}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> FlagConfig {
        let text = r#"
[flags.source]
env = "SOURCE_DATABASE_URL"
aliases = ["source", "from"]
short = "s"
type = "string"
help = "desired-state database"

[flags.allow-destructive]
env = "DPM_ALLOW_DESTRUCTIVE"
aliases = ["allow-destructive"]
type = "bool"
default = "false"

[flags.jobs]
env = "DPM_JOBS"
aliases = ["jobs"]
type = "integer"
"#;
        let parsed: CliFlagsFile = toml::from_str(text).unwrap();
        FlagConfig { flags: parsed.flags }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn long_short_equals_and_separate_values() {
        let cfg = config();
        for argv in [
            args(&["--source", "postgres://x"]),
            args(&["--source=postgres://x"]),
            args(&["--from", "postgres://x"]),
            args(&["-s", "postgres://x"]),
            args(&["-spostgres://x"]),
        ] {
            let (map, _) = parse_fallback(&cfg, &argv).unwrap();
            assert_eq!(map.get("SOURCE_DATABASE_URL").map(String::as_str), Some("postgres://x"), "argv: {argv:?}");
        }
    }

    #[test]
    fn bool_flags_and_integers() {
        let cfg = config();
        let (map, _) = parse_fallback(&cfg, &args(&["--allow-destructive"])).unwrap();
        assert_eq!(map.get("DPM_ALLOW_DESTRUCTIVE").map(String::as_str), Some("true"));
        let (map, _) = parse_fallback(&cfg, &args(&["--allow-destructive=0"])).unwrap();
        assert_eq!(map.get("DPM_ALLOW_DESTRUCTIVE").map(String::as_str), Some("false"));
        assert!(parse_fallback(&cfg, &args(&["--jobs", "abc"])).is_err());
    }

    #[test]
    fn unknown_flags_error_and_double_dash_stops() {
        let cfg = config();
        assert!(parse_fallback(&cfg, &args(&["--nope"])).is_err());
        let (map, pos) = parse_fallback(&cfg, &args(&["--", "--nope"])).unwrap();
        assert!(map.is_empty());
        assert_eq!(pos, vec!["--nope".to_string()]);
    }

    #[test]
    fn resolution_precedence_flag_over_env_over_default() {
        let cfg = config();
        let (map, _) = parse_fallback(&cfg, &args(&["--allow-destructive"])).unwrap();
        let resolved = Resolved::new(&cfg, map);
        assert!(resolved.get_bool("DPM_ALLOW_DESTRUCTIVE"));
        let resolved = Resolved::new(&cfg, HashMap::new());
        assert_eq!(resolved.get("DPM_ALLOW_DESTRUCTIVE").as_deref(), Some("false"));
    }
}
