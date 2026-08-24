//! `config` — show where things live and what is configured.

use clap::{Args, Subcommand};
use serde::Serialize;
use serde_json::Value;

use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::error::{Error, Result};
use crate::storage::sqlite::migrations;
use crate::ui;

/// Changes to the config file.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Set one setting, e.g. `contextd config set embeddings.model bge-m3`.
    Set(SetArgs),
    /// Print one setting.
    Get(GetArgs),
}

#[derive(Debug, Args)]
pub struct SetArgs {
    /// Dotted path, e.g. `vector.backend` or `context.max_context_tokens`.
    pub key: String,
    /// New value. Types follow the existing setting; an empty value clears an
    /// optional one.
    pub value: String,
}

#[derive(Debug, Args)]
pub struct GetArgs {
    /// Dotted path, e.g. `embeddings.model`.
    pub key: String,
}

/// `contextd config`
#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommand>,

    /// Print the config file path only.
    #[arg(long)]
    pub path: bool,

    /// Print the config file contents as TOML.
    #[arg(long)]
    pub toml: bool,

    /// Contact the embedding provider and vector store, and report what
    /// answers. Use after pointing ContextD at bge-m3 or Qdrant.
    #[arg(long)]
    pub check: bool,
}

/// Show or change configuration.
pub async fn run(app: &App, global: &GlobalArgs, args: &ConfigArgs) -> Result<()> {
    match &args.command {
        Some(ConfigCommand::Set(set_args)) => return set(app, global, set_args),
        Some(ConfigCommand::Get(get_args)) => return get(app, global, get_args),
        None => {}
    }

    let paths = app.paths();
    if args.path {
        println!("{}", paths.config_file().display());
        return Ok(());
    }
    if args.toml {
        let text = toml::to_string_pretty(app.config())
            .map_err(|e| crate::error::Error::Config(e.to_string()))?;
        print!("{text}");
        return Ok(());
    }
    if args.check {
        return check(app, global).await;
    }

    #[derive(Serialize)]
    struct ConfigOutput<'a> {
        root: String,
        database: String,
        config_file: String,
        schema_version: i64,
        config: &'a crate::config::Config,
    }

    let out = ConfigOutput {
        root: paths.root().display().to_string(),
        database: paths.database().display().to_string(),
        config_file: paths.config_file().display().to_string(),
        schema_version: app.store().schema_version()?,
        config: app.config(),
    };

    output::render(global, &out, || {
        let config = app.config();
        let mut text = format!("{}\n", ui::header("ContextD configuration"));
        text.push_str(&ui::kv(&[
            ("Home", out.root.clone()),
            ("Database", out.database.clone()),
            ("Config", out.config_file.clone()),
            (
                "Schema",
                format!("v{} (latest v{})", out.schema_version, migrations::target_version()),
            ),
            (
                "Embeddings",
                // Report the model the active provider actually uses: the
                // local embedder ignores `embeddings.model`, and printing a
                // model name it never sees would be misleading.
                match app.embedder() {
                    Some(provider) => format!("{} · {}", provider.id(), provider.model()),
                    None => "disabled".to_string(),
                },
            ),
            (
                "Vector store",
                if config.vector.is_external() {
                    format!(
                        "{} · {}/{}",
                        config.vector.backend, config.vector.url, config.vector.collection
                    )
                } else {
                    "sqlite (built in)".to_string()
                },
            ),
            (
                "Context budget",
                format!(
                    "{} tokens, {} memories",
                    config.context.max_context_tokens, config.context.max_memories
                ),
            ),
            ("Default agent", config.general.default_agent.clone()),
        ]));
        text.push_str(&format!(
            "\n\n{}",
            ui::hint(
                "Edit the config file to change any of this; `contextd config --check` tests it."
            )
        ));
        text
    })
}

/// Contact everything configured and report what answered.
///
/// A real embedding request is the only honest test of an embedding provider:
/// it exercises the endpoint, the key and the model name together, which is
/// what a typo in any of the three looks like.
async fn check(app: &App, global: &GlobalArgs) -> Result<()> {
    #[derive(Serialize)]
    struct Check {
        embeddings: ComponentCheck,
        vector_store: ComponentCheck,
    }

    #[derive(Serialize)]
    struct ComponentCheck {
        name: String,
        ok: bool,
        detail: String,
    }

    let embeddings = match app.embedder() {
        None => ComponentCheck {
            name: "none".into(),
            ok: true,
            detail: "embeddings are disabled; retrieval is keyword-only".into(),
        },
        Some(provider) => {
            let label = format!("{} \u{b7} {}", provider.id(), provider.model());
            match crate::embeddings::provider::embed_one(provider, "contextd connectivity check")
                .await
            {
                Ok(vector) if !vector.is_empty() => ComponentCheck {
                    name: label,
                    ok: true,
                    detail: format!("{} dimensions", vector.len()),
                },
                Ok(_) => ComponentCheck {
                    name: label,
                    ok: false,
                    detail: "provider returned an empty vector".into(),
                },
                Err(err) => ComponentCheck { name: label, ok: false, detail: err.to_string() },
            }
        }
    };

    let health = crate::search::IndexService::new(app).vector_health().await?;
    let vector_store = ComponentCheck {
        name: health.backend.clone(),
        ok: health.reachable,
        detail: health
            .detail
            .clone()
            .or_else(|| health.points.map(|n| format!("{n} points")))
            .unwrap_or_else(|| "built in".into()),
    };

    let all_ok = embeddings.ok && vector_store.ok;
    let result = Check { embeddings, vector_store };

    output::render(global, &result, || {
        format!(
            "{}\n{}\n\n{}",
            ui::header("Check"),
            ui::kv(&[
                (
                    "Embeddings",
                    format!(
                        "{} {} {}",
                        ui::check(result.embeddings.ok),
                        result.embeddings.name,
                        ui::dim(&result.embeddings.detail)
                    )
                ),
                (
                    "Vector store",
                    format!(
                        "{} {} {}",
                        ui::check(result.vector_store.ok),
                        result.vector_store.name,
                        ui::dim(&result.vector_store.detail)
                    )
                ),
            ]),
            if all_ok {
                ui::ok("Everything ContextD needs is reachable.")
            } else {
                ui::warn("Something is unreachable; recall falls back to keyword search.")
            }
        )
    })
}

/// Change one setting in the config file.
///
/// The value is coerced to the type the setting already has, and the whole
/// file is re-parsed and validated before it is written: a typo produces an
/// error here rather than a confusing failure on the next command.
fn set(app: &App, global: &GlobalArgs, args: &SetArgs) -> Result<()> {
    let path = app.paths().config_file();
    let config = crate::Config::load(&path)?;
    let mut document = serde_json::to_value(&config)?;

    let previous = lookup(&document, &args.key)
        .ok_or_else(|| Error::invalid("key", unknown_key_message(&document, &args.key)))?
        .clone();
    let updated = coerce(&previous, &args.value, &args.key)?;
    assign(&mut document, &args.key, updated.clone())?;

    let config: crate::Config = serde_json::from_value(document).map_err(|err| {
        Error::Config(format!("{} would make the config invalid: {err}", args.key))
    })?;
    config.validate()?;
    config.save(&path)?;

    #[derive(Serialize)]
    struct SetOutput<'a> {
        key: &'a str,
        previous: Value,
        value: Value,
    }
    let out = SetOutput { key: &args.key, previous: previous.clone(), value: updated.clone() };

    output::render(global, &out, || {
        let mut text = format!(
            "{}\n{}",
            ui::ok(&format!("{} = {}", args.key, render_value(&updated))),
            ui::dim(&format!("  was {}", render_value(&previous)))
        );
        if args.key.starts_with("embeddings.") || args.key.starts_with("vector.") {
            text.push_str(&format!(
                "\n\n{}",
                ui::hint("Check it with `contextd config --check`, then re-index with `contextd refresh --force-embeddings`.")
            ));
        }
        text
    })
}

/// Print one setting.
fn get(app: &App, global: &GlobalArgs, args: &GetArgs) -> Result<()> {
    let document = serde_json::to_value(app.config())?;
    let value = lookup(&document, &args.key)
        .ok_or_else(|| Error::invalid("key", unknown_key_message(&document, &args.key)))?;
    output::render(global, value, || render_value(value))
}

fn lookup<'a>(document: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(document, |current, segment| current.get(segment))
}

fn assign(document: &mut Value, key: &str, value: Value) -> Result<()> {
    let segments: Vec<&str> = key.split('.').collect();
    let (last, parents) = segments.split_last().expect("a key always has one segment");
    let mut current = document;
    for segment in parents {
        current = current
            .get_mut(*segment)
            .ok_or_else(|| Error::invalid("key", format!("`{key}` does not exist")))?;
    }
    match current.as_object_mut() {
        Some(object) => {
            object.insert((*last).to_string(), value);
            Ok(())
        }
        None => Err(Error::invalid("key", format!("`{key}` is not a settable field"))),
    }
}

/// Coerce a command-line string to the type the setting already holds.
fn coerce(previous: &Value, raw: &str, key: &str) -> Result<Value> {
    let trimmed = raw.trim();
    match previous {
        Value::Bool(_) => match trimmed.to_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(Value::Bool(true)),
            "false" | "no" | "off" | "0" => Ok(Value::Bool(false)),
            _ => Err(Error::invalid(
                "value",
                format!("`{key}` is a true/false setting; got `{trimmed}`"),
            )),
        },
        Value::Number(number) if number.is_f64() && !number.is_u64() => trimmed
            .parse::<f64>()
            .map(|value| {
                serde_json::Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
            })
            .map_err(|_| Error::invalid("value", format!("`{key}` is a number; got `{trimmed}`"))),
        Value::Number(_) => trimmed
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| trimmed.parse::<f64>().map(Value::from))
            .map_err(|_| Error::invalid("value", format!("`{key}` is a number; got `{trimmed}`"))),
        Value::Array(_) => Ok(Value::Array(
            trimmed
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| Value::String(part.to_string()))
                .collect(),
        )),
        // Optional settings (`vector.api_key_env`, a remote's home) are cleared
        // by passing an empty value.
        Value::Null if trimmed.is_empty() => Ok(Value::Null),
        _ if trimmed.is_empty() => Ok(Value::Null),
        _ => Ok(Value::String(trimmed.to_string())),
    }
}

fn render_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "(unset)".to_string(),
        Value::Array(items) => items.iter().map(render_value).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// Suggest the settable keys under the section the user aimed at.
fn unknown_key_message(document: &Value, key: &str) -> String {
    let section = key.split('.').next().unwrap_or_default();
    match document.get(section).and_then(Value::as_object) {
        Some(fields) => format!(
            "`{key}` is not a setting; `{section}` has: {}",
            fields.keys().map(|k| format!("{section}.{k}")).collect::<Vec<_>>().join(", ")
        ),
        None => format!(
            "`{key}` is not a setting; sections are: {}",
            document
                .as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Value {
        serde_json::to_value(crate::Config::default()).unwrap()
    }

    #[test]
    fn lookup_walks_dotted_paths() {
        let document = document();
        assert_eq!(lookup(&document, "embeddings.provider").unwrap(), "local");
        assert!(lookup(&document, "embeddings.nonsense").is_none());
        assert!(lookup(&document, "nonsense").is_none());
    }

    #[test]
    fn values_are_coerced_to_the_existing_type() {
        let document = document();
        let tokens = lookup(&document, "context.max_context_tokens").unwrap();
        assert_eq!(coerce(tokens, "8000", "k").unwrap(), Value::from(8000));
        assert!(coerce(tokens, "lots", "k").is_err());

        let weight = lookup(&document, "search.semantic_weight").unwrap();
        assert_eq!(coerce(weight, "1.5", "k").unwrap().as_f64().unwrap(), 1.5);

        let flag = lookup(&document, "sync.protect_agent_files").unwrap();
        assert_eq!(coerce(flag, "off", "k").unwrap(), Value::Bool(false));
        assert!(coerce(flag, "maybe", "k").is_err());

        let text = lookup(&document, "embeddings.model").unwrap();
        assert_eq!(coerce(text, " bge-m3 ", "k").unwrap(), Value::String("bge-m3".into()));
    }

    #[test]
    fn assignment_produces_a_valid_config() {
        let mut document = document();
        assign(&mut document, "vector.backend", Value::String("qdrant".into())).unwrap();
        assign(&mut document, "embeddings.dimensions", Value::from(1024)).unwrap();
        let config: crate::Config = serde_json::from_value(document).unwrap();
        assert_eq!(config.vector.backend, "qdrant");
        assert_eq!(config.embeddings.dimensions, 1024);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn unknown_keys_suggest_neighbours() {
        let message = unknown_key_message(&document(), "embeddings.modle");
        assert!(message.contains("embeddings.model"), "{message}");
        let message = unknown_key_message(&document(), "nope.thing");
        assert!(message.contains("embeddings"), "{message}");
    }
}
