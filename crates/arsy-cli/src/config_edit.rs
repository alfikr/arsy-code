//! Narrow edits to the user configuration file, `arsy.json`.
//!
//! ARSY reads configuration everywhere and writes it in two places: the
//! `provider.endpoint.*` objects `/provider` maintains, and the `mcp.server.*`
//! objects `arsy mcp` maintains. The file is JSON, so an edit is a parse, a
//! change to one key, and a re-serialize — JSON carries no comments or hand
//! alignment for a round trip to destroy.
//!
//! Each function takes the file as a string and returns a new one, so the
//! surgery is testable without touching a filesystem.

use serde_json::{Map, Value};

/// A provider endpoint as `/provider` collects it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    /// Every model the endpoint offers; the first is written as its default.
    pub models: Vec<String>,
    /// The `secret://` handle the credential was stored under.
    pub credential: String,
}

impl Endpoint {
    fn object(&self) -> Value {
        let mut object = Map::new();
        object.insert("kind".to_owned(), Value::String(self.kind.clone()));
        object.insert("base_url".to_owned(), Value::String(self.base_url.clone()));
        // The first model is the default; the rest are listed beside it, and
        // only when there are any, so a single-model endpoint stays as short as
        // one written by hand.
        if let Some((default, rest)) = self.models.split_first() {
            object.insert("model".to_owned(), Value::String(default.clone()));
            if !rest.is_empty() {
                object.insert(
                    "models".to_owned(),
                    Value::Array(rest.iter().cloned().map(Value::String).collect()),
                );
            }
        }
        object.insert(
            "credential".to_owned(),
            Value::String(self.credential.clone()),
        );
        Value::Object(object)
    }
}

/// A name that may be used as an object key or a value without surprising the
/// person who later reads the file.
///
/// JSON escaping would make any of these safe to write, so this is not about
/// producing a valid file: a provider called `my "prod"` or one whose name ends
/// in a space is a name nobody can type back at the CLI.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub fn is_writable(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['"', '\\', '\n', '\r'])
        && value.trim() == value
        && value.is_ascii()
}

/// The configuration as a JSON object. An empty file is an empty object, so a
/// first edit does not need the file to exist.
fn document(config: &str) -> Result<Map<String, Value>, String> {
    if config.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(config) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err("the configuration is not a JSON object".to_owned()),
        Err(error) => Err(format!("the configuration is not valid JSON: {error}")),
    }
}

/// Two spaces and a trailing newline: what an operator's editor would have
/// written, so a later hand edit does not show up as a reformat.
fn render(document: &Map<String, Value>) -> String {
    let mut out = serde_json::to_string_pretty(document).unwrap_or_else(|_| "{}".to_owned());
    out.push('\n');
    out
}

/// The object at `path`, creating the objects along the way.
///
/// `None` when something on the path is there but is not an object: replacing
/// an operator's value with a table is not an edit, it is a deletion.
fn object_at<'a>(
    document: &'a mut Map<String, Value>,
    path: &[&str],
) -> Option<&'a mut Map<String, Value>> {
    let mut current = document;
    for key in path {
        let entry = current
            .entry((*key).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        current = entry.as_object_mut()?;
    }
    Some(current)
}

/// Set one key, creating the objects that hold it.
pub fn set(config: &str, path: &[&str], key: &str, value: Value) -> Result<String, String> {
    let mut document = document(config)?;
    let parent = object_at(&mut document, path)
        .ok_or_else(|| format!("`{}` is not an object in the configuration", path.join(".")))?;
    parent.insert(key.to_owned(), value);
    Ok(render(&document))
}

/// Set one key only where its parent object already exists.
///
/// `None` when it does not, so the caller can say "no connection named that"
/// rather than silently creating one from a single key.
pub fn set_existing(
    config: &str,
    path: &[&str],
    key: &str,
    value: Value,
) -> Result<Option<String>, String> {
    if !contains(config, path)? {
        return Ok(None);
    }
    set(config, path, key, value).map(Some)
}

/// Remove the key at the end of `path`. A path that is not there leaves the
/// file unchanged.
pub fn remove(config: &str, path: &[&str]) -> Result<String, String> {
    let Some((last, parents)) = path.split_last() else {
        return Ok(config.to_owned());
    };
    let mut document = document(config)?;
    let mut current = &mut document;
    for key in parents {
        match current.get_mut(*key).and_then(Value::as_object_mut) {
            Some(next) => current = next,
            None => return Ok(render(&document)),
        }
    }
    current.remove(*last);
    Ok(render(&document))
}

/// Whether `path` names something in the file.
pub fn contains(config: &str, path: &[&str]) -> Result<bool, String> {
    let document = document(config)?;
    let mut current = &Value::Object(document);
    for key in path {
        match current.get(*key) {
            Some(next) => current = next,
            None => return Ok(false),
        }
    }
    Ok(true)
}

/// Add `provider.endpoint.<name>`.
pub fn append_endpoint(config: &str, endpoint: &Endpoint) -> Result<String, String> {
    set(
        config,
        &["provider", "endpoint"],
        &endpoint.name,
        endpoint.object(),
    )
}

/// Remove `provider.endpoint.<name>` and everything under it.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub fn remove_endpoint(config: &str, name: &str) -> Result<String, String> {
    remove(config, &["provider", "endpoint", name])
}

/// Point `provider.default` at `name`.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub fn set_default(config: &str, name: &str) -> Result<String, String> {
    set(
        config,
        &["provider"],
        "default",
        Value::String(name.to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint {
            name: "acme".to_owned(),
            kind: "openai".to_owned(),
            base_url: "https://acme.test/v1".to_owned(),
            models: vec!["acme-1".to_owned(), "acme-2".to_owned()],
            credential: "secret://file/acme.key".to_owned(),
        }
    }

    /// The operator's file is theirs: an edit touches the object it owns and
    /// leaves every other key exactly as it was, and removing is the inverse of
    /// adding.
    #[test]
    fn an_edit_leaves_every_other_key_alone() {
        let original = "\
{
  \"provider\": {
    \"default\": \"myai\",
    \"endpoint\": {
      \"myai\": {
        \"kind\": \"openai\",
        \"base_url\": \"https://myai.test/v1\",
        \"credential\": \"secret://os/myai\"
      }
    }
  },
  \"ui\": {
    \"color\": \"always\"
  }
}
";
        let added = append_endpoint(original, &endpoint()).unwrap();
        let loaded: Value = serde_json::from_str(&added).unwrap();
        assert_eq!(loaded["provider"]["default"], Value::String("myai".into()));
        assert_eq!(loaded["ui"]["color"], Value::String("always".into()));
        let acme = &loaded["provider"]["endpoint"]["acme"];
        assert_eq!(
            acme["credential"],
            Value::String("secret://file/acme.key".into())
        );
        // The first model is the default and the rest are listed beside it.
        assert_eq!(acme["model"], Value::String("acme-1".into()));
        assert_eq!(acme["models"], serde_json::json!(["acme-2"]));

        let removed = remove_endpoint(&added, "acme").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&removed).unwrap(),
            serde_json::from_str::<Value>(original).unwrap(),
            "removal was not the inverse of adding"
        );

        // A name that is not there changes nothing at all.
        assert_eq!(
            serde_json::from_str::<Value>(&remove_endpoint(original, "nothere").unwrap()).unwrap(),
            serde_json::from_str::<Value>(original).unwrap()
        );
    }

    #[test]
    fn the_default_is_retargeted_or_added() {
        let with_key = "{\"provider\":{\"default\":\"myai\"},\"ui\":{\"color\":\"never\"}}";
        let out = set_default(with_key, "acme").unwrap();
        let loaded: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(loaded["provider"]["default"], Value::String("acme".into()));
        assert_eq!(loaded["ui"]["color"], Value::String("never".into()));

        // An empty file gets the object the key belongs in.
        let out = set_default("", "acme").unwrap();
        let loaded: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(loaded["provider"]["default"], Value::String("acme".into()));
    }

    /// These names are typed back at the CLI, so a value nobody could type is
    /// refused before it reaches the file.
    #[test]
    fn a_value_nobody_could_type_back_is_refused() {
        for good in [
            "acme",
            "https://acme.test/v1",
            "secret://file/acme.key",
            "gpt-4o-mini",
        ] {
            assert!(is_writable(good), "{good} was refused");
        }
        for bad in [
            "",
            "has \"quotes\"",
            "back\\slash",
            "two\nlines",
            " padded ",
            "émoji-née",
        ] {
            assert!(!is_writable(bad), "{bad:?} was accepted");
        }
    }

    /// Setting a key in one object must not leak into the next one, and the
    /// file has to still load — asserted through the real loader, because "the
    /// file still means what it says" is the only property that matters.
    #[test]
    fn setting_a_key_lands_inside_its_own_object() {
        use arsy_kernel::config::{Config, Layer, CONFIG_FILE};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        let load = |body: &str| {
            std::fs::write(&path, body).unwrap();
            Config::load(&[(Layer::User, path.clone())]).expect("the file still loads")
        };

        let config = "\
{
  \"schema_version\": 1,
  \"mcp\": {
    \"server\": {
      \"first\": { \"transport\": \"stdio\", \"command\": \"one\" },
      \"second\": { \"transport\": \"stdio\", \"command\": \"two\" }
    }
  }
}
";
        let updated = set_existing(
            config,
            &["mcp", "server", "first"],
            "enabled",
            Value::Bool(false),
        )
        .unwrap()
        .unwrap();
        let loaded = load(&updated);
        assert!(!loaded.mcp_server("first").unwrap().enabled);
        assert!(
            loaded.mcp_server("second").unwrap().enabled,
            "the neighbouring object is untouched: {updated}"
        );

        // Setting it again replaces the key rather than adding a second one.
        let again = set_existing(
            &updated,
            &["mcp", "server", "first"],
            "enabled",
            Value::Bool(true),
        )
        .unwrap()
        .unwrap();
        assert_eq!(again.matches("enabled").count(), 1, "{again}");
        assert!(load(&again).mcp_server("first").unwrap().enabled);

        // An object that is not there is `None`, so the caller decides what to
        // say rather than getting a silently created connection.
        assert!(set_existing(
            config,
            &["mcp", "server", "absent"],
            "enabled",
            Value::Bool(false)
        )
        .unwrap()
        .is_none());
    }

    /// A file that is not JSON is an error, never a file quietly replaced by
    /// one holding only the edit.
    #[test]
    fn a_file_that_is_not_json_is_refused() {
        let error = set_default("schema_version = 1\n", "acme").unwrap_err();
        assert!(error.contains("not valid JSON"), "{error}");
        assert!(set_default("[1, 2]", "acme")
            .unwrap_err()
            .contains("not a JSON object"));
    }
}
