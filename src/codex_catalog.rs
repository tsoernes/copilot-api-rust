//! Refresh a local Codex model catalogue from this gateway's available models.
//!
//! This performs only a local `/models` GET and reads Codex's bundled model
//! metadata for compatible templates. It does not send inference requests.

use std::{
    cmp::Ordering,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const BASE_INSTRUCTIONS: &str = "You are Codex, an AI coding assistant. Help the user complete authorized tasks, use available tools as appropriate, and follow applicable instructions and permission boundaries. Do not claim actions or checks that were not performed.";
const CATALOG_PROFILE: &str = "copilot-api";
const CODEX_TIMEOUT: Duration = Duration::from_secs(60);
const CODEX_OUTPUT_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct RefreshSummary {
    pub catalog_path: PathBuf,
    pub model_ids: Vec<String>,
    pub excluded_count: usize,
    pub active: bool,
    pub config_updated: bool,
}

/// Fetch `/models` from the local gateway and refresh the Codex catalogue.
pub async fn refresh_from_endpoint(
    endpoint: &str,
    api_key: Option<String>,
    activate: bool,
) -> Result<RefreshSummary> {
    let client = crate::monitor::data::MonitorClient::new(endpoint, api_key)
        .context("creating local models client")?;
    let source = client
        .fetch_models()
        .await
        .map_err(anyhow::Error::msg)
        .context("fetching models from the local gateway")?;
    refresh_from_models(endpoint, source, activate).await
}

/// Build and atomically publish a catalogue using an already-fetched model list.
pub async fn refresh_from_models(
    endpoint: &str,
    source: Value,
    activate: bool,
) -> Result<RefreshSummary> {
    let endpoint = endpoint.to_owned();
    tokio::task::spawn_blocking(move || refresh_sync(&endpoint, source, activate))
        .await
        .context("Codex catalogue refresh task failed")?
}

fn refresh_sync(endpoint: &str, source: Value, activate: bool) -> Result<RefreshSummary> {
    let home = codex_home()?;
    let catalogs = home.join("model-catalogs");
    private_dir(&catalogs)?;
    let base = catalogs.join(CATALOG_PROFILE);
    private_dir(&base)?;
    let _lock = RefreshLock::acquire(&base)?;

    let current = base.join("current.json");
    reject_symlink_if_present(&current)?;
    let config = home.join("config.toml");
    let config_before = read_optional_string(&config)?;

    let bundled = run_codex(&["debug", "models", "--bundled"])
        .context("reading Codex's bundled model metadata")?;
    let built = build_catalog(&source, &bundled)?;
    let revision_name = format!(
        "{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock predates Unix epoch")?
            .as_nanos(),
        std::process::id()
    );
    let revision = base.join("revisions").join(&revision_name);
    private_dir(&revision)?;

    let source_path = revision.join("source-models.json");
    let bundled_path = revision.join("bundled-models.json");
    let staged = revision.join("catalog.json");
    atomic_write(&source_path, &encode(&source)?, 0o600)?;
    atomic_write(&bundled_path, &encode(&bundled)?, 0o600)?;
    atomic_write(&staged, &encode(&built.catalog)?, 0o600)?;

    let staged_setting = format!(
        "model_catalog_json={}",
        json_string(&staged.to_string_lossy())?
    );
    let validated = run_codex(&["-c", &staged_setting, "debug", "models"])
        .context("validating the staged Codex catalogue")?;
    validate_codex_catalog(&built.catalog, &validated)?;

    let catalog_path_text = current.to_string_lossy().to_string();
    let config_after = if activate {
        Some(config_with_catalog(
            config_before.as_deref().unwrap_or_default(),
            &catalog_path_text,
        )?)
    } else {
        None
    };

    if let Some(previous) = read_optional(&current)? {
        atomic_write(&revision.join("previous-catalog.json"), &previous, 0o600)?;
    }
    atomic_write(&current, &encode(&built.catalog)?, 0o600)?;

    let mut config_updated = false;
    if let Some(config_after) = config_after
        && config_before.as_deref() != Some(config_after.as_str())
    {
        if fs::symlink_metadata(&config).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!("refusing to replace a symlinked Codex config.toml");
        }
        if read_optional_string(&config)?.as_deref() != config_before.as_deref() {
            bail!(
                "Codex config changed during refresh; catalogue was written but config was not changed"
            );
        }
        if let Some(before) = &config_before {
            atomic_write(
                &revision.join("config.toml.before"),
                before.as_bytes(),
                0o600,
            )?;
        }
        let mode = existing_file_mode(&config);
        atomic_write(&config, config_after.as_bytes(), mode)?;
        config_updated = true;
    }

    let active_config = read_optional_string(&config)?;
    let active = active_config
        .as_deref()
        .and_then(|contents| setting_catalog_path(contents).ok().flatten())
        .is_some_and(|path| path == current);
    let summary = RefreshSummary {
        catalog_path: current.clone(),
        model_ids: built.model_ids.clone(),
        excluded_count: built.excluded.len(),
        active,
        config_updated,
    };
    let manifest = json!({
        "status": "complete",
        "endpoint": endpoint,
        "generated_at_unix_nanos": revision_name,
        "catalog": current,
        "source_model_count": built.source_count,
        "included_model_count": summary.model_ids.len(),
        "excluded": built.excluded,
        "model_ids": summary.model_ids,
        "active": active,
        "config_updated": config_updated,
        "inference_tested": false,
    });
    atomic_write(&revision.join("manifest.json"), &encode(&manifest)?, 0o600)?;
    atomic_write(
        &base.join("latest.json"),
        &encode(&json!({"revision": revision, "catalog": current, "status": "complete"}))?,
        0o600,
    )?;
    Ok(summary)
}

struct BuiltCatalog {
    catalog: Value,
    model_ids: Vec<String>,
    excluded: Vec<Value>,
    source_count: usize,
}

fn build_catalog(source: &Value, bundled: &Value) -> Result<BuiltCatalog> {
    let records = source
        .get("data")
        .and_then(Value::as_array)
        .context("models endpoint did not return a data array")?;
    if source.get("has_more") == Some(&Value::Bool(true)) {
        bail!("models endpoint is paginated; refusing to publish an incomplete catalogue");
    }
    let bundled_models = bundled
        .get("models")
        .and_then(Value::as_array)
        .context("installed Codex returned an invalid bundled catalogue")?;
    let templates: std::collections::HashMap<&str, &Value> = bundled_models
        .iter()
        .filter_map(|model| Some((model.get("slug")?.as_str()?, model)))
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut included: Vec<&Value> = Vec::new();
    let mut excluded = Vec::new();
    for record in records {
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .context("models endpoint returned a model without an ID")?;
        if !seen.insert(id) {
            bail!("models endpoint returned duplicate model IDs");
        }
        let kind = record
            .pointer("/capabilities/type")
            .or_else(|| record.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let outputs = record
            .get("output_modalities")
            .or_else(|| record.pointer("/capabilities/output_modalities"))
            .and_then(Value::as_array);
        let mut reason = None;
        if matches!(kind.as_str(), "embedding" | "embeddings" | "image") {
            reason = Some("not an interactive text/coding model");
        } else if outputs.is_some_and(|modalities| {
            !modalities.is_empty()
                && !modalities
                    .iter()
                    .any(|modality| modality.as_str() == Some("text"))
        }) {
            reason = Some("model does not advertise text output");
        }
        if reason.is_none() {
            let input = record
                .get("input_modalities")
                .or_else(|| record.pointer("/capabilities/input_modalities"))
                .and_then(Value::as_array);
            let accepts_text = input.is_none_or(|modalities| {
                modalities
                    .iter()
                    .any(|modality| modality.as_str() == Some("text"))
            });
            if !accepts_text {
                reason = Some("model does not advertise text input");
            }
        }
        if let Some(reason) = reason {
            excluded.push(json!({"id": id, "reason": reason}));
        } else {
            included.push(record);
        }
    }
    if included.is_empty() {
        bail!("models endpoint returned no compatible text models; existing catalogue retained");
    }
    included.sort_by(|left, right| {
        let left_id = left["id"].as_str().unwrap_or_default();
        let right_id = right["id"].as_str().unwrap_or_default();
        compare_model_ids(left_id, right_id)
    });

    let mut model_ids = Vec::with_capacity(included.len());
    let mut models = Vec::with_capacity(included.len());
    for (priority, record) in included.into_iter().enumerate() {
        let id = record["id"].as_str().unwrap_or_default();
        let name = record.get("name").and_then(Value::as_str).unwrap_or(id);
        let display_name = record
            .get("display_name")
            .and_then(Value::as_str)
            .or_else(|| record.get("name").and_then(Value::as_str))
            .unwrap_or(id);
        let supports = record.pointer("/capabilities/supports");
        let efforts = string_array(supports.and_then(|value| value.get("reasoning_effort")))
            .context("invalid reasoning-effort metadata in model catalog")?;
        let template = templates.get(id).copied();
        let mut model = template.cloned().unwrap_or_else(|| {
            json!({
                "shell_type": "unified_exec",
                "base_instructions": BASE_INSTRUCTIONS,
                "support_verbosity": false,
                "supports_reasoning_summary_parameter": false,
                "truncation_policy": {"mode": "tokens", "limit": 10000},
                "experimental_supported_tools": [],
            })
        });
        let template_efforts = template
            .and_then(|value| value.get("supported_reasoning_levels"))
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|level| {
                        Some((
                            level.get("effort")?.as_str()?,
                            level.get("description")?.clone(),
                        ))
                    })
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .unwrap_or_default();
        let reasoning_levels: Vec<Value> = efforts
            .iter()
            .map(|effort| {
                json!({
                    "effort": effort,
                    "description": template_efforts.get(effort.as_str()).cloned().unwrap_or_else(|| json!(format!("Gateway-advertised reasoning effort: {effort}")))
                })
            })
            .collect();
        let prior_default = template
            .and_then(|value| value.get("default_reasoning_level"))
            .and_then(Value::as_str);
        let default_reasoning = if efforts
            .iter()
            .any(|effort| Some(effort.as_str()) == prior_default)
        {
            prior_default.map(|value| json!(value))
        } else if efforts.iter().any(|effort| effort == "medium") {
            Some(json!("medium"))
        } else {
            efforts.first().map(|value| json!(value))
        };
        let limits = record.pointer("/capabilities/limits");
        let context_window = positive_integer(record.get("context_window")).or_else(|| {
            positive_integer(limits.and_then(|value| value.get("max_context_window_tokens")))
        });
        let input_limit = positive_integer(record.get("input_limit"))
            .or_else(|| positive_integer(limits.and_then(|value| value.get("max_prompt_tokens"))));
        if let Some(limit) = [context_window, input_limit].into_iter().flatten().min() {
            model["context_window"] = json!(limit);
            model["max_context_window"] = json!(limit);
            model["auto_compact_token_limit"] = json!(limit.saturating_mul(9) / 10);
        }
        let input_modalities = record
            .get("input_modalities")
            .or_else(|| record.pointer("/capabilities/input_modalities"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| {
                if supports
                    .and_then(|value| value.get("vision"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    vec![json!("text"), json!("image")]
                } else {
                    vec![json!("text")]
                }
            })
            .into_iter()
            .filter(|modality| matches!(modality.as_str(), Some("text" | "image")))
            .collect::<Vec<_>>();
        model["slug"] = json!(id);
        model["display_name"] = json!(display_name);
        model["description"] = json!(format!("Local gateway: {name}"));
        model["visibility"] = json!("list");
        model["supported_in_api"] = json!(true);
        model["priority"] = json!(priority);
        model["supported_reasoning_levels"] = json!(reasoning_levels);
        model["default_reasoning_level"] = default_reasoning.unwrap_or(Value::Null);
        model["upgrade"] = Value::Null;
        model["availability_nux"] = Value::Null;
        model["input_modalities"] = json!(input_modalities);
        model_ids.push(id.to_owned());
        models.push(model);
    }
    Ok(BuiltCatalog {
        catalog: json!({"models": models}),
        model_ids,
        excluded,
        source_count: records.len(),
    })
}

fn string_array(value: Option<&Value>) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        bail!("expected a string array")
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .context("expected a string in metadata array")
        })
        .collect()
}

fn positive_integer(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|number| *number > 0)
}

fn compare_model_ids(left: &str, right: &str) -> Ordering {
    let left_gpt = gpt_version(left);
    let right_gpt = gpt_version(right);
    match (left_gpt, right_gpt) {
        (Some(left_version), Some(right_version)) => right_version
            .cmp(&left_version)
            .then_with(|| natural_cmp(right, left)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => natural_cmp(right, left),
    }
}

fn gpt_version(id: &str) -> Option<Vec<u64>> {
    let local_id = id.rsplit('/').next().unwrap_or(id);
    let lower = local_id.to_ascii_lowercase();
    let suffix = lower.strip_prefix("gpt-")?;
    let mut version = Vec::new();
    let mut number = String::new();
    for byte in suffix.bytes() {
        if byte.is_ascii_digit() {
            number.push(char::from(byte));
        } else if matches!(byte, b'.' | b'-') && !number.is_empty() {
            version.push(number.parse().ok()?);
            number.clear();
        } else {
            break;
        }
    }
    if !number.is_empty() {
        version.push(number.parse().ok()?);
    }
    (!version.is_empty()).then_some(version)
}

fn natural_cmp(left: &str, right: &str) -> Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let (mut li, mut ri) = (0, 0);
    while li < left.len() && ri < right.len() {
        if left[li].is_ascii_digit() && right[ri].is_ascii_digit() {
            let (left_end, right_end) = (digit_end(left, li), digit_end(right, ri));
            let (left_sig, right_sig) = (
                significant_start(left, li, left_end),
                significant_start(right, ri, right_end),
            );
            let left_len = left_end - left_sig;
            let right_len = right_end - right_sig;
            match left_len
                .cmp(&right_len)
                .then_with(|| left[left_sig..left_end].cmp(&right[right_sig..right_end]))
            {
                Ordering::Equal => {}
                result => return result,
            }
            li = left_end;
            ri = right_end;
        } else {
            match left[li]
                .to_ascii_lowercase()
                .cmp(&right[ri].to_ascii_lowercase())
            {
                Ordering::Equal => {}
                result => return result,
            }
            li += 1;
            ri += 1;
        }
    }
    left.len().cmp(&right.len())
}

fn digit_end(value: &[u8], mut index: usize) -> usize {
    while index < value.len() && value[index].is_ascii_digit() {
        index += 1;
    }
    index
}

fn significant_start(value: &[u8], mut start: usize, end: usize) -> usize {
    while start < end.saturating_sub(1) && value[start] == b'0' {
        start += 1;
    }
    start
}

fn codex_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CODEX_HOME").filter(|path| !path.is_empty()) {
        return Ok(path.into());
    }
    Ok(dirs::home_dir()
        .context("home directory is unavailable")?
        .join(".codex"))
}

fn run_codex(args: &[&str]) -> Result<Value> {
    let binary = codex_binary();
    let mut command = Command::new(&binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("could not launch Codex CLI at {}", binary.display()))?;
    let stdout = child.stdout.take().context("missing Codex stdout pipe")?;
    let stderr = child.stderr.take().context("missing Codex stderr pipe")?;
    let out_reader = thread::spawn(move || read_capped(stdout));
    let err_reader = thread::spawn(move || read_capped(stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(error).context("waiting for Codex CLI");
            }
        }
        if started.elapsed() >= CODEX_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_reader.join();
            let _ = err_reader.join();
            bail!("Codex CLI catalogue validation timed out");
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = out_reader
        .join()
        .map_err(|_| anyhow::anyhow!("Codex stdout reader failed"))??;
    let _stderr = err_reader
        .join()
        .map_err(|_| anyhow::anyhow!("Codex stderr reader failed"))??;
    if !status.success() {
        bail!(
            "Codex CLI catalogue validation failed (exit status {})",
            status.code().unwrap_or(-1)
        );
    }
    serde_json::from_slice(&stdout).context("Codex CLI returned invalid model JSON")
}

fn codex_binary() -> PathBuf {
    if let Some(binary) = env::var_os("CODEX_CATALOG_BINARY").filter(|path| !path.is_empty()) {
        return PathBuf::from(binary);
    }
    if env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| directory.join("codex").is_file())
    }) {
        return PathBuf::from("codex");
    }
    dirs::home_dir()
        .map(|home| home.join(".local/bin/codex"))
        .filter(|path| path.is_file())
        .or_else(|| {
            ["/usr/local/bin/codex", "/usr/bin/codex"]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
        })
        .unwrap_or_else(|| PathBuf::from("codex"))
}

fn read_capped(mut reader: impl Read) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 16 * 1024];
    let mut exceeded = false;
    loop {
        let count = reader
            .read(&mut buffer)
            .context("reading Codex CLI output")?;
        if count == 0 {
            break;
        }
        if output.len().saturating_add(count) <= CODEX_OUTPUT_LIMIT {
            output.extend_from_slice(&buffer[..count]);
        } else {
            exceeded = true;
        }
    }
    if exceeded {
        bail!("Codex CLI output exceeded its 32 MiB limit");
    }
    Ok(output)
}

fn validate_codex_catalog(catalog: &Value, validated: &Value) -> Result<()> {
    let expected: Vec<_> = catalog["models"]
        .as_array()
        .context("generated catalogue missing models")?
        .iter()
        .filter_map(|model| model.get("slug").and_then(Value::as_str))
        .collect();
    let actual = validated["models"]
        .as_array()
        .context("Codex rejected the generated model list")?;
    let actual_ids: Vec<_> = actual
        .iter()
        .map(|model| {
            model
                .get("slug")
                .and_then(Value::as_str)
                .context("Codex returned a model without a slug")
        })
        .collect::<Result<_>>()?;
    if expected != actual_ids {
        bail!("Codex changed the model IDs or picker order during validation");
    }
    if !actual.iter().enumerate().all(|(index, model)| {
        model.get("priority").and_then(Value::as_u64) == Some(index as u64)
            && model.get("visibility").and_then(Value::as_str) == Some("list")
            && model.get("supported_in_api").and_then(Value::as_bool) == Some(true)
    }) {
        bail!("Codex model priorities or availability did not validate");
    }
    Ok(())
}

fn setting_catalog_path(config: &str) -> Result<Option<PathBuf>> {
    let boundary = config
        .lines()
        .position(|line| line.trim_start().starts_with('['))
        .unwrap_or_else(|| config.lines().count());
    let settings = config
        .lines()
        .take(boundary)
        .filter_map(|line| top_level_setting(line, "model_catalog_json"))
        .collect::<Vec<_>>();
    if settings.len() > 1 {
        bail!("multiple top-level model_catalog_json settings in Codex config");
    }
    settings
        .first()
        .map(|value| parse_toml_string(value).map(PathBuf::from))
        .transpose()
}

fn config_with_catalog(config: &str, path: &str) -> Result<String> {
    let encoded = json_string(path)?;
    let setting = format!("model_catalog_json = {encoded}");
    let mut lines: Vec<String> = config.lines().map(str::to_owned).collect();
    let boundary = lines
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    let existing: Vec<usize> = lines
        .iter()
        .take(boundary)
        .enumerate()
        .filter_map(|(index, line)| top_level_setting(line, "model_catalog_json").map(|_| index))
        .collect();
    if existing.len() > 1 {
        bail!("multiple top-level model_catalog_json settings in Codex config");
    }
    if let Some(index) = existing.first() {
        lines[*index] = setting;
    } else {
        lines.insert(0, setting);
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn top_level_setting<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let line = line.trim_start();
    if line.starts_with('#') {
        return None;
    }
    let (name, value) = line.split_once('=')?;
    (name.trim() == key).then_some(value.trim())
}

fn parse_toml_string(value: &str) -> Result<String> {
    if value.starts_with('"') {
        return serde_json::from_str(value)
            .context("invalid model_catalog_json string in Codex config");
    }
    if let Some(value) = value
        .strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
    {
        return Ok(value.to_owned());
    }
    bail!("model_catalog_json must be a quoted string")
}

fn json_string(value: &str) -> Result<String> {
    serde_json::to_string(value).context("encoding Codex config string")
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn read_optional_string(path: &Path) -> Result<Option<String>> {
    read_optional(path)?
        .map(|contents| String::from_utf8(contents).context("Codex config is not UTF-8"))
        .transpose()
}

struct RefreshLock(PathBuf);

impl RefreshLock {
    fn acquire(base: &Path) -> Result<Self> {
        let path = base.join(".refresh-lock");
        fs::create_dir(&path).with_context(|| {
            format!(
                "catalog refresh is already running or a stale lock exists at {}",
                path.display()
            )
        })?;
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        let lock = Self(path.clone());
        if let Err(error) = atomic_write(
            &path.join("owner.json"),
            &encode(
                &json!({"pid": std::process::id(), "started_unix_nanos": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()}),
            )?,
            0o600,
        ) {
            drop(lock);
            return Err(error);
        }
        Ok(lock)
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("owner.json"));
        let _ = fs::remove_dir(&self.0);
    }
}

fn private_dir(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("refusing to use a symlinked Codex catalog directory");
    }
    fs::create_dir_all(path)
        .with_context(|| format!("creating private directory {}", path.display()))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protecting private directory {}", path.display()))?;
    Ok(())
}

fn reject_symlink_if_present(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("refusing to replace a symlinked catalogue file");
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    #[cfg(not(unix))]
    let _ = mode;
    let parent = path.parent().context("output path has no parent")?;
    private_dir(parent)?;
    reject_symlink_if_present(path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("output path has an invalid filename")?;
    let temp = parent.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(mode);
    let mut file = options
        .open(&temp)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(contents)
            .context("writing private catalogue file")?;
        file.sync_all().context("syncing private catalogue file")?;
        Ok(())
    })();
    drop(file);
    let result = result.and_then(|()| {
        fs::rename(&temp, path).with_context(|| format!("publishing {}", path.display()))?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .context("syncing catalogue directory")?;
        Ok(())
    });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn existing_file_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o600)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0o600
    }
}

fn encode(value: &Value) -> Result<Vec<u8>> {
    let mut encoded = serde_json::to_vec_pretty(value).context("encoding model catalogue JSON")?;
    encoded.push(b'\n');
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(models: Vec<Value>) -> Value {
        json!({"data": models})
    }

    fn model(id: &str, kind: &str) -> Value {
        json!({"id": id, "name": id, "capabilities": {"type": kind}})
    }

    #[test]
    fn includes_text_models_without_responses_advertisement_and_sorts_gpt_first() {
        let source = source(vec![
            json!({"id":"claude-haiku-5.5","name":"Haiku","supported_endpoints":["/chat/completions"]}),
            model("gpt-5.3-codex", "chat"),
            model("gpt-6-luna", "chat"),
            model("gpt-6.1-terra", "chat"),
        ]);
        let bundled = json!({"models": []});
        let built = build_catalog(&source, &bundled).unwrap();
        assert_eq!(
            built.model_ids,
            [
                "gpt-6.1-terra",
                "gpt-6-luna",
                "gpt-5.3-codex",
                "claude-haiku-5.5"
            ]
        );
        assert_eq!(built.catalog["models"][0]["priority"], 0);
        assert_eq!(
            built.catalog["models"][0]["base_instructions"],
            BASE_INSTRUCTIONS
        );
    }

    #[test]
    fn excludes_embeddings_images_and_non_text_outputs_but_preserves_exact_ids() {
        let source = source(vec![
            model("embed-x", "embedding"),
            model("image-x", "image"),
            json!({"id":"audio-x","name":"Audio","output_modalities":["audio"]}),
            json!({"id":"MAI-Code-2-Flash","name":"MAI Code 2 Flash","output_modalities":["text"]}),
        ]);
        let built = build_catalog(&source, &json!({"models": []})).unwrap();
        assert_eq!(built.model_ids, ["MAI-Code-2-Flash"]);
        assert_eq!(built.excluded.len(), 3);
    }

    #[test]
    fn retains_installed_codex_template_and_uses_gateway_reasoning_and_limits() {
        let source = source(vec![json!({
            "id":"gpt-6-luna",
            "name":"GPT 6 Luna",
            "context_window":200000,
            "input_limit":128000,
            "capabilities":{"type":"chat","limits":{"max_prompt_tokens":100000},"supports":{"reasoning_effort":["low","high"]}}
        })]);
        let bundled = json!({"models":[{
            "slug":"gpt-6-luna",
            "base_instructions":"template instructions",
            "default_reasoning_level":"high",
            "supported_reasoning_levels":[{"effort":"low","description":"Low"},{"effort":"high","description":"High"}],
            "experimental_supported_tools":["local_shell"]
        }]});
        let built = build_catalog(&source, &bundled).unwrap();
        let entry = &built.catalog["models"][0];
        assert_eq!(entry["base_instructions"], "template instructions");
        assert_eq!(entry["experimental_supported_tools"][0], "local_shell");
        assert_eq!(entry["default_reasoning_level"], "high");
        assert_eq!(entry["context_window"], 128000);
        assert_eq!(entry["auto_compact_token_limit"], 115200);
    }

    #[test]
    fn refuses_empty_duplicate_or_paginated_catalogues() {
        assert!(build_catalog(&json!({"data": []}), &json!({"models": []})).is_err());
        assert!(
            build_catalog(
                &json!({"data": [model("x","chat"),model("x","chat")]}),
                &json!({"models": []})
            )
            .is_err()
        );
        assert!(
            build_catalog(
                &json!({"data": [model("x","chat")],"has_more":true}),
                &json!({"models": []})
            )
            .is_err()
        );
    }

    #[test]
    fn config_update_changes_only_the_top_level_catalog_entry() {
        let input = "model_catalog_json = \"/old.json\"\nmodel = \"gpt-6-luna\"\n[model_providers.github]\nname = \"github\"\n";
        let output = config_with_catalog(input, "/new.json").unwrap();
        assert!(output.starts_with("model_catalog_json = \"/new.json\"\nmodel = \"gpt-6-luna\""));
        assert!(output.contains("[model_providers.github]\nname = \"github\""));
        assert_eq!(
            setting_catalog_path(&output).unwrap(),
            Some(PathBuf::from("/new.json"))
        );
    }

    #[test]
    fn config_update_adds_only_when_requested_and_rejects_duplicate_top_level() {
        let input = "model = \"gpt-6-luna\"\n[model_providers.github]\nmodel_catalog_json = \"not-top-level\"\n";
        let output = config_with_catalog(input, "/codex/current.json").unwrap();
        assert!(output.starts_with("model_catalog_json = \"/codex/current.json\"\n"));
        assert!(
            output.contains("[model_providers.github]\nmodel_catalog_json = \"not-top-level\"")
        );
        assert!(
            config_with_catalog(
                "model_catalog_json=\"a\"\nmodel_catalog_json=\"b\"\n",
                "/new"
            )
            .is_err()
        );
    }
}
