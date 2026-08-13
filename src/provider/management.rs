use std::{
    collections::HashSet,
    fs,
    io::{self, IsTerminal, Write},
    path::Path,
    time::Duration,
};

use serde_json::{Value, json};

use crate::config::{TtsProvider, ValidatedConfig};

use super::{
    ProviderError, ProviderInfo, SessionKind,
    client::{CatalogDescriptor, Client, ensure_provider_directories, provider_directories},
    discover_provider,
};

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum CatalogScope {
    Installed,
    Available,
    #[default]
    All,
}

impl CatalogScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Available => "available",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PrepareOptions {
    pub yes: bool,
    pub accepted_licenses: Vec<String>,
}

fn configured(
    config: &ValidatedConfig,
) -> Result<(&str, &crate::config::TtsConfig), ProviderError> {
    let tts = &config.profile().tts;
    let TtsProvider::Utterpipe(provider) = &tts.provider else {
        return Err(ProviderError::Configuration(
            "the selected profile uses system TTS; no provider operation is required".into(),
        ));
    };
    Ok((&provider.provider, tts))
}

fn start(config: &ValidatedConfig, session: SessionKind) -> Result<Client, ProviderError> {
    let (slug, tts) = configured(config)?;
    let executable = discover_provider(slug)?;
    let (data, cache) = provider_directories(slug)?;
    if session != SessionKind::Inspect {
        ensure_provider_directories(&data, &cache)?;
    }
    let provider = tts.utterpipe().expect("validated provider");
    let mut client = Client::spawn(&executable, slug, session, &provider.provider_environment)?;
    if session != SessionKind::Inspect {
        client.initialize(tts, &data, &cache)?;
    }
    Ok(client)
}

pub fn inspect_provider(config: &ValidatedConfig) -> Result<ProviderInfo, ProviderError> {
    let client = start(config, SessionKind::Inspect)?;
    let info = client.info.clone();
    client.shutdown()?;
    Ok(info)
}

pub fn validate_provider(config: &ValidatedConfig) -> Result<Value, ProviderError> {
    with_management_client(config, |client| {
        let result = client.call("provider.validate", json!({}), Duration::from_secs(30))?;
        validate_provider_result(&result)?;
        Ok(result)
    })
}

pub fn list_catalog(
    config: &ValidatedConfig,
    catalog_id: &str,
    scope: CatalogScope,
    refresh: bool,
) -> Result<Value, ProviderError> {
    with_management_client(config, |client| {
        if !client.info.capabilities.catalog {
            return Err(ProviderError::Configuration(
                "configured provider does not advertise catalogs".into(),
            ));
        }
        let descriptor = client
            .info
            .catalogs
            .iter()
            .find(|catalog| catalog.id == catalog_id)
            .cloned()
            .ok_or_else(|| {
                ProviderError::Configuration(format!(
                    "provider does not advertise catalog '{catalog_id}'"
                ))
            })?;
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let mut params = json!({
                "catalog_id":catalog_id,
                "scope":scope.as_str(),
                "refresh":refresh,
                "limit":256
            });
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let page = client.call("catalog.items", params, Duration::from_secs(60))?;
            let next = validate_catalog_page(&page, &descriptor)?;
            items.extend(
                page["items"]
                    .as_array()
                    .expect("validated catalog page")
                    .iter()
                    .cloned(),
            );
            if items.len() > 4_096 {
                return Err(ProviderError::Protocol(
                    "catalog contains more than 4,096 items".into(),
                ));
            }
            let Some(next) = next else { break };
            if !seen_cursors.insert(next.clone()) {
                return Err(ProviderError::Protocol(
                    "catalog repeated a pagination cursor".into(),
                ));
            }
            cursor = Some(next);
        }
        Ok(json!({"catalog_id":catalog_id,"items":items}))
    })
}

pub fn prepare_provider(
    config: &ValidatedConfig,
    options: &PrepareOptions,
) -> Result<Value, ProviderError> {
    with_management_client(config, |client| {
        if !client.info.capabilities.prepare {
            return Err(ProviderError::Configuration(
                "configured provider does not advertise preparation".into(),
            ));
        }
        let plan = client.call(
            "prepare.plan",
            json!({"refresh":true, "allow_network":true}),
            Duration::from_secs(120),
        )?;
        validate_plan(&plan, true)?;
        let plan_id = plan
            .get("plan_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Protocol("prepare plan omitted plan_id".into()))?
            .to_owned();
        let required: Vec<String> = plan
            .get("licenses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|license| {
                license.get("requires_acceptance").and_then(Value::as_bool) == Some(true)
            })
            .filter_map(|license| license.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        let missing: Vec<_> = required
            .iter()
            .filter(|id| !options.accepted_licenses.contains(id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(ProviderError::Configuration(format!(
                "preparation requires explicit --accept-license for: {}",
                missing.join(", ")
            )));
        }
        confirm_plan(&plan, options.yes, "preparation")?;
        let result = client.call_with_events(
            "prepare.apply",
            json!({"plan_id":plan_id, "accepted_licenses":options.accepted_licenses, "operation_id":format!("prepare-{}", std::process::id())}),
            Duration::from_secs(60 * 30),
        )?;
        validate_status_list(&result, "installed")?;
        Ok(result)
    })
}

pub fn remove_provider(
    config: &ValidatedConfig,
    artifacts: &[String],
    purge: bool,
    yes: bool,
) -> Result<Value, ProviderError> {
    if artifacts.is_empty() && !purge {
        return Err(ProviderError::Configuration(
            "removal requires at least one --artifact or --purge".into(),
        ));
    }
    if artifacts.iter().any(|artifact| {
        artifact.is_empty()
            || artifact.chars().count() > 256
            || artifact.contains(['\r', '\n', '\0'])
    }) {
        return Err(ProviderError::Configuration(
            "artifact IDs must contain 1 to 256 characters without CR, LF, or NUL".into(),
        ));
    }
    with_management_client(config, |client| {
        if !client.info.capabilities.remove {
            return Err(ProviderError::Configuration(
                "configured provider does not advertise removal".into(),
            ));
        }
        let plan = client.call(
            "remove.plan",
            json!({"artifacts": artifacts, "purge":purge}),
            Duration::from_secs(30),
        )?;
        validate_plan(&plan, false)?;
        let plan_id = plan
            .get("plan_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Protocol("remove plan omitted plan_id".into()))?
            .to_owned();
        confirm_plan(&plan, yes, "removal")?;
        let result = client.call_with_events(
            "remove.apply",
            json!({"plan_id":plan_id, "operation_id":format!("remove-{}", std::process::id())}),
            Duration::from_secs(30 * 60),
        )?;
        validate_status_list(&result, "removed")?;
        Ok(result)
    })
}

fn confirm_plan(plan: &Value, yes: bool, operation: &str) -> Result<(), ProviderError> {
    if yes {
        println!("{}", render_json(plan));
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(ProviderError::Configuration(format!(
            "{operation} requires confirmation in a non-interactive session; inspect the plan and rerun with --yes\n{}",
            render_json(plan)
        )));
    }
    println!("{}", render_json(plan));
    print!("Apply this {operation} plan? [y/N] ");
    io::stdout().flush().map_err(ProviderError::from)?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(ProviderError::from)?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        Err(ProviderError::Configuration(format!(
            "{operation} was not confirmed"
        )))
    }
}

pub fn import_asset(
    config: &ValidatedConfig,
    kind: &str,
    source: &Path,
    requested_id: &str,
    consent_confirmed: bool,
) -> Result<Value, ProviderError> {
    if !consent_confirmed {
        return Err(ProviderError::Configuration(
            "asset import requires --consent-confirmed".into(),
        ));
    }
    if !source.is_absolute() {
        return Err(ProviderError::Configuration(
            "asset import source must be an absolute path".into(),
        ));
    }
    let source = fs::canonicalize(source).map_err(|error| {
        ProviderError::Configuration(format!("asset import source could not be opened: {error}"))
    })?;
    if !fs::metadata(&source)
        .map_err(ProviderError::from)?
        .is_file()
    {
        return Err(ProviderError::Configuration(
            "asset import source is not a regular file".into(),
        ));
    }
    if requested_id.is_empty()
        || requested_id.chars().count() > 256
        || requested_id.contains(['\r', '\n', '\0'])
    {
        return Err(ProviderError::Configuration(
            "requested asset ID must contain 1 to 256 characters without CR, LF, or NUL".into(),
        ));
    }
    let source_size = fs::metadata(&source).map_err(ProviderError::from)?.len();
    let source = super::client::unicode_path(&source, "asset import source")?.to_owned();
    with_management_client(config, |client| {
        if !client.info.capabilities.asset_import {
            return Err(ProviderError::Configuration(
                "configured provider does not advertise asset import".into(),
            ));
        }
        let descriptor = client
            .info
            .import_kinds
            .iter()
            .find(|descriptor| descriptor.id == kind)
            .cloned()
            .ok_or_else(|| {
                ProviderError::Configuration(format!(
                    "provider does not advertise import kind '{kind}'"
                ))
            })?;
        if source_size > descriptor.max_source_bytes {
            return Err(ProviderError::Configuration(format!(
                "asset import source exceeds the provider's {} byte limit",
                descriptor.max_source_bytes
            )));
        }
        let result = client.call_with_events(
            "asset.import",
            json!({"kind":kind,"source_path":source, "requested_id":requested_id, "consent_confirmed":true, "operation_id":format!("asset-import-{}", std::process::id())}),
            Duration::from_secs(30 * 60),
        )?;
        if result.get("status").and_then(Value::as_str) != Some("installed")
            || result
                .get("artifact_id")
                .and_then(Value::as_str)
                .is_none_or(|id| !valid_protocol_id(id))
            || result.get("provider_options_patch").is_some_and(|patch| {
                !valid_options_patch(patch, &descriptor.patchable_provider_options)
            })
            || result.get("utterance_options_patch").is_some_and(|patch| {
                !valid_options_patch(patch, &descriptor.patchable_utterance_options)
            })
        {
            return Err(ProviderError::Protocol(
                "asset.import returned an invalid result".into(),
            ));
        }
        Ok(result)
    })
}

fn validate_provider_result(result: &Value) -> Result<(), ProviderError> {
    if !matches!(
        result.get("status").and_then(Value::as_str),
        Some("ready" | "incomplete" | "unavailable")
    ) || result
        .get("issues")
        .and_then(Value::as_array)
        .is_none_or(|issues| {
            issues.len() > 1_024 || issues.iter().any(|issue| !valid_validation_issue(issue))
        })
    {
        return Err(ProviderError::Protocol(
            "provider.validate returned an invalid result".into(),
        ));
    }
    Ok(())
}

fn validate_catalog_page(
    result: &Value,
    descriptor: &CatalogDescriptor,
) -> Result<Option<String>, ProviderError> {
    let entries = result
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Protocol("catalog page omitted items array".into()))?;
    if entries.len() > 256
        || entries
            .iter()
            .any(|entry| !valid_catalog_item(entry, descriptor))
    {
        return Err(ProviderError::Protocol(
            "catalog returned invalid generic item descriptors".into(),
        ));
    }
    match result.get("next_cursor") {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|cursor| !cursor.is_empty() && cursor.len() <= 256)
            .map(|cursor| Some(cursor.to_owned()))
            .ok_or_else(|| ProviderError::Protocol("catalog returned invalid next_cursor".into())),
    }
}

fn valid_validation_issue(issue: &Value) -> bool {
    issue.is_object()
        && matches!(
            issue.get("severity").and_then(Value::as_str),
            Some("info" | "warning" | "error")
        )
        && issue
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(valid_protocol_id)
        && issue
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| valid_protocol_text(message, 4_096))
        && issue
            .get("remediation")
            .and_then(Value::as_str)
            .is_some_and(|remediation| valid_protocol_text(remediation, 4_096))
}

fn valid_catalog_item(entry: &Value, descriptor: &CatalogDescriptor) -> bool {
    entry.is_object()
        && valid_catalog_identity(entry)
        && valid_catalog_status(entry.get("status"))
        && valid_languages(entry.get("languages"))
        && valid_license_descriptor(entry.get("license"))
        && entry.get("description").is_none_or(|description| {
            description
                .as_str()
                .is_some_and(|text| valid_protocol_text(text, 512))
        })
        && entry
            .get("provider_options_patch")
            .is_some_and(|patch| valid_options_patch(patch, &descriptor.patchable_provider_options))
        && entry.get("utterance_options_patch").is_some_and(|patch| {
            valid_options_patch(patch, &descriptor.patchable_utterance_options)
        })
        && entry
            .get("artifacts")
            .is_none_or(|artifacts| artifacts.as_array().is_some_and(|items| items.len() <= 256))
        && ["download_bytes", "installed_bytes"]
            .into_iter()
            .all(|field| entry.get(field).is_none_or(|size| size.as_u64().is_some()))
}

fn no_nulls(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Array(items) => items.iter().all(no_nulls),
        Value::Object(object) => object.values().all(no_nulls),
        _ => true,
    }
}

fn valid_options_patch(value: &Value, allowed: &[String]) -> bool {
    value.as_object().is_some_and(|patch| {
        patch
            .keys()
            .all(|name| allowed.iter().any(|item| item == name))
            && patch.values().all(no_nulls)
    })
}

fn valid_catalog_identity(entry: &Value) -> bool {
    entry
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty() && id.len() <= 128 && !id.contains(['\r', '\n', '\0']))
        && entry
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| valid_protocol_text(name, 80))
}

fn valid_catalog_status(status: Option<&Value>) -> bool {
    matches!(
        status.and_then(Value::as_str),
        Some("embedded" | "available" | "installed" | "remote" | "incomplete" | "incompatible")
    )
}

fn valid_languages(languages: Option<&Value>) -> bool {
    languages.is_none_or(|languages| {
        languages.as_array().is_some_and(|languages| {
            languages.len() <= 256
                && languages.iter().all(|language| {
                    language
                        .as_str()
                        .is_some_and(|language| valid_protocol_text(language, 128))
                })
        })
    })
}

fn valid_license_descriptor(license: Option<&Value>) -> bool {
    license.is_none_or(|license| {
        license.is_object()
            && license
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(valid_protocol_id)
            && license
                .get("url")
                .and_then(Value::as_str)
                .is_some_and(valid_disclosure_url)
            && license
                .get("requires_acceptance")
                .and_then(Value::as_bool)
                .is_some()
    })
}

fn validate_plan(plan: &Value, licenses_required: bool) -> Result<(), ProviderError> {
    if plan
        .get("plan_id")
        .and_then(Value::as_str)
        .is_none_or(|id| !valid_protocol_id(id))
        || plan
            .get("summary")
            .and_then(Value::as_str)
            .is_none_or(|summary| !valid_protocol_text(summary, 512))
        || plan
            .get("actions")
            .and_then(Value::as_array)
            .is_none_or(|actions| {
                actions.len() > 1_024
                    || actions.iter().any(|action| {
                        !action.is_object()
                            || action
                                .get("kind")
                                .and_then(Value::as_str)
                                .is_none_or(|kind| !valid_protocol_text(kind, 64))
                            || action
                                .get("artifact")
                                .and_then(Value::as_str)
                                .is_none_or(|artifact| !valid_protocol_id(artifact))
                            || ["download_bytes", "installed_bytes", "reclaimed_bytes"]
                                .into_iter()
                                .any(|field| {
                                    action
                                        .get(field)
                                        .is_some_and(|value| value.as_u64().is_none())
                                })
                            || action.get("sha256").is_some_and(|value| {
                                value.as_str().is_none_or(|hash| {
                                    hash.len() != 64
                                        || !hash.bytes().all(|byte| {
                                            byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                                        })
                                })
                            })
                            || action.get("source").is_some_and(|value| {
                                value
                                    .as_str()
                                    .is_none_or(|source| !valid_disclosure_url(source))
                            })
                    })
            })
    {
        return Err(ProviderError::Protocol(
            "provider returned an invalid operation plan".into(),
        ));
    }
    if licenses_required {
        let licenses = plan
            .get("licenses")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::Protocol("prepare plan omitted licenses".into()))?;
        let mut ids = HashSet::new();
        if licenses.len() > 256
            || licenses.iter().any(|license| {
                let id = license.get("id").and_then(Value::as_str);
                !license.is_object()
                    || id.is_none_or(|id| !valid_protocol_id(id) || !ids.insert(id))
                    || license
                        .get("name")
                        .and_then(Value::as_str)
                        .is_none_or(|name| !valid_protocol_text(name, 256))
                    || license
                        .get("url")
                        .and_then(Value::as_str)
                        .is_none_or(|url| !valid_disclosure_url(url))
                    || license
                        .get("requires_acceptance")
                        .and_then(Value::as_bool)
                        .is_none()
            })
        {
            return Err(ProviderError::Protocol(
                "prepare plan contains invalid license disclosures".into(),
            ));
        }
    }
    Ok(())
}

fn validate_status_list(result: &Value, member: &str) -> Result<(), ProviderError> {
    if result.get("status").and_then(Value::as_str) != Some("ready")
        || result
            .get(member)
            .and_then(Value::as_array)
            .is_none_or(|items| {
                items.len() > 4_096
                    || items
                        .iter()
                        .any(|item| item.as_str().is_none_or(|value| !valid_protocol_id(value)))
            })
    {
        return Err(ProviderError::Protocol(format!(
            "provider returned an invalid {member} result"
        )));
    }
    Ok(())
}

fn valid_protocol_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.chars().count() <= maximum && !value.chars().any(char::is_control)
}

fn valid_protocol_id(value: &str) -> bool {
    !value.is_empty() && value.chars().count() <= 256 && !value.contains(['\r', '\n', '\0'])
}

fn valid_disclosure_url(value: &str) -> bool {
    (value.starts_with("https://") || value.starts_with("http://"))
        && valid_protocol_text(value, 2_048)
        && !value.contains('@')
}

pub fn render_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "<invalid provider result>".into())
}

fn with_management_client(
    config: &ValidatedConfig,
    operation: impl FnOnce(&mut Client) -> Result<Value, ProviderError>,
) -> Result<Value, ProviderError> {
    let mut client = start(config, SessionKind::Management)?;
    let result = operation(&mut client);
    let shutdown = client.shutdown();
    match result {
        Ok(value) => shutdown.map(|_| value),
        Err(error) => {
            let _ = shutdown;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn license() -> Value {
        json!({
            "id": "MIT",
            "url": "https://example.invalid/license",
            "requires_acceptance": false
        })
    }

    fn catalog_descriptor() -> CatalogDescriptor {
        CatalogDescriptor {
            id: "voices".into(),
            name: "Voices".into(),
            description: "Available voices".into(),
            item_kind: "voice".into(),
            patchable_provider_options: vec!["model".into()],
            patchable_utterance_options: vec!["voice".into(), "tone".into()],
        }
    }

    fn catalog_item() -> Value {
        json!({
            "id": "alba",
            "name": "Alba",
            "description": "A clear voice",
            "status": "installed",
            "languages": ["en"],
            "provider_options_patch": {},
            "utterance_options_patch": {"voice": "alba"},
            "artifacts": [],
            "download_bytes": 123,
            "installed_bytes": 456,
            "license": license()
        })
    }

    #[test]
    fn provider_validation_requires_complete_issue_descriptors() {
        let valid = json!({
            "status": "incomplete",
            "issues": [{
                "severity": "warning",
                "code": "model_missing",
                "message": "the selected model is missing",
                "remediation": "install the selected model"
            }]
        });
        assert!(validate_provider_result(&valid).is_ok());

        for field in ["severity", "code", "message", "remediation"] {
            let mut invalid = valid.clone();
            invalid["issues"][0].as_object_mut().unwrap().remove(field);
            assert!(
                validate_provider_result(&invalid).is_err(),
                "accepted issue without {field}"
            );
        }

        let mut invalid = valid;
        invalid["issues"][0]["severity"] = json!("fatal");
        assert!(validate_provider_result(&invalid).is_err());
    }

    #[test]
    fn generic_catalog_requires_identity_status_and_safe_patch() {
        let descriptor = catalog_descriptor();
        let valid = json!({"items": [catalog_item()]});
        assert_eq!(validate_catalog_page(&valid, &descriptor).unwrap(), None);

        for field in [
            "id",
            "name",
            "status",
            "provider_options_patch",
            "utterance_options_patch",
        ] {
            let mut invalid = valid.clone();
            invalid["items"][0].as_object_mut().unwrap().remove(field);
            assert!(
                validate_catalog_page(&invalid, &descriptor).is_err(),
                "accepted catalog item without {field}"
            );
        }

        for field in ["id", "url", "requires_acceptance"] {
            let mut invalid = valid.clone();
            invalid["items"][0]["license"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                validate_catalog_page(&invalid, &descriptor).is_err(),
                "accepted catalog item license without {field}"
            );
        }

        let mut invalid_size = valid.clone();
        invalid_size["items"][0]["download_bytes"] = json!(-1);
        assert!(validate_catalog_page(&invalid_size, &descriptor).is_err());
        let mut invalid_status = valid.clone();
        invalid_status["items"][0]["status"] = json!("unknown");
        assert!(validate_catalog_page(&invalid_status, &descriptor).is_err());
        let mut invalid_languages = valid.clone();
        invalid_languages["items"][0]["languages"] = json!("en");
        assert!(validate_catalog_page(&invalid_languages, &descriptor).is_err());

        let mut unknown_patch = valid.clone();
        unknown_patch["items"][0]["utterance_options_patch"] = json!({"model": "x"});
        assert!(validate_catalog_page(&unknown_patch, &descriptor).is_err());
        let mut deleting_patch = valid;
        deleting_patch["items"][0]["utterance_options_patch"] = json!({"voice": null});
        assert!(validate_catalog_page(&deleting_patch, &descriptor).is_err());
    }

    #[test]
    fn generic_catalog_validates_pagination_cursor() {
        let descriptor = catalog_descriptor();
        let page = json!({"items": [], "next_cursor": "page-two"});
        assert_eq!(
            validate_catalog_page(&page, &descriptor).unwrap(),
            Some("page-two".into())
        );

        for invalid in [json!(""), json!(null), json!(7)] {
            let page = json!({"items": [], "next_cursor": invalid});
            assert!(validate_catalog_page(&page, &descriptor).is_err());
        }
    }
}
