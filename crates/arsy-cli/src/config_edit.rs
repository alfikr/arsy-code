//! Narrow edits to the user configuration file.
//!
//! ARSY reads configuration everywhere and writes it in exactly one place: the
//! `[provider.endpoint.*]` tables `/provider` maintains. That is why these are
//! text edits rather than a parse-and-reserialize round trip — a round trip
//! would return a file with every comment, blank line, and alignment the
//! operator wrote replaced by the serializer's own formatting.
//!
//! Each function takes the file as a string and returns a new one, so the
//! surgery is testable without touching a filesystem.

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
    fn table(&self) -> String {
        let mut table = format!(
            "[provider.endpoint.{}]\nkind = \"{}\"\nbase_url = \"{}\"\n",
            self.name, self.kind, self.base_url,
        );
        // The first model is the default; the rest are listed beside it, and
        // only when there are any, so a single-model endpoint stays as short as
        // one written by hand.
        if let Some((default, rest)) = self.models.split_first() {
            table.push_str(&format!("model = \"{default}\"\n"));
            if !rest.is_empty() {
                let listed = rest
                    .iter()
                    .map(|model| format!("\"{model}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                table.push_str(&format!("models = [{listed}]\n"));
            }
        }
        table.push_str(&format!("credential = \"{}\"\n", self.credential));
        table
    }
}

/// TOML strings here are written, not parsed, so a value that would need
/// escaping is refused up front rather than producing a file that no longer
/// loads. Every field `/provider` collects is a name, a URL, or a handle.
pub fn is_writable(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['"', '\\', '\n', '\r'])
        && value.trim() == value
        && value.is_ascii()
}

/// Add an endpoint table at the end of the file.
///
/// Appending rather than inserting keeps the diff to the lines that did not
/// exist before: nothing above the new table can move.
pub fn append_endpoint(config: &str, endpoint: &Endpoint) -> String {
    let mut out = config.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&endpoint.table());
    out
}

/// Remove `[provider.endpoint.<name>]` and the keys under it.
///
/// The table ends where the next one begins, so everything from its header to
/// the following header goes, and one blank line left behind by the removal is
/// taken with it. A name that is not there leaves the file untouched.
pub fn remove_endpoint(config: &str, name: &str) -> String {
    let header = format!("[provider.endpoint.{name}]");
    let lines: Vec<&str> = config.lines().collect();
    let Some(start) = lines.iter().position(|line| line.trim() == header) else {
        return config.to_owned();
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map_or(lines.len(), |(index, _)| index);

    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    kept.extend_from_slice(&lines[..start]);
    kept.extend_from_slice(&lines[end..]);
    // The blank line that separated this table from the one above is now a
    // trailing blank, or a doubled one in the middle.
    while kept.len() > start && start > 0 && kept.get(start - 1).is_some_and(|line| line.is_empty())
    {
        if kept.get(start).is_none_or(|line| !line.is_empty()) {
            break;
        }
        kept.remove(start);
    }
    let mut out = kept.join("\n");
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Point `[provider] default` at `name`, adding the key or the table when the
/// file does not have them yet.
pub fn set_default(config: &str, name: &str) -> String {
    let line = format!("default = \"{name}\"");
    let mut lines: Vec<String> = config.lines().map(str::to_owned).collect();

    // The key belongs to whichever table it sits under, so `[provider]` has to
    // be found before a bare `default =` can be claimed as the right one.
    let table = lines.iter().position(|line| line.trim() == "[provider]");
    if let Some(table) = table {
        let end = lines
            .iter()
            .enumerate()
            .skip(table + 1)
            .find(|(_, line)| line.trim_start().starts_with('['))
            .map_or(lines.len(), |(index, _)| index);
        match lines[table + 1..end]
            .iter()
            .position(|existing| existing.trim_start().starts_with("default"))
        {
            Some(offset) => lines[table + 1 + offset] = line,
            None => lines.insert(table + 1, line),
        }
    } else {
        // A file with no `[provider]` table gets one before the first table, so
        // the key cannot land under someone else's header.
        let first = lines
            .iter()
            .position(|line| line.trim_start().starts_with('['))
            .unwrap_or(lines.len());
        lines.insert(first, String::new());
        lines.insert(first + 1, "[provider]".to_owned());
        lines.insert(first + 2, line);
        lines.insert(first + 3, String::new());
    }
    let mut out = lines.join("\n");
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// A file ARSY has never written needs the header every layer is rejected
/// without.
pub fn ensure_schema(config: &str) -> String {
    if config
        .lines()
        .any(|line| line.trim_start().starts_with("schema_version"))
    {
        return config.to_owned();
    }
    format!(
        "schema_version = 1\n{}{config}",
        if config.is_empty() { "" } else { "\n" }
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

    /// The operator's file is theirs: an edit touches the table it owns and
    /// leaves every comment and unrelated line exactly where it was.
    #[test]
    fn an_edit_leaves_every_other_line_alone() {
        let original = "\
schema_version = 1

[provider]
default = \"myai\"

# the endpoint I actually use
[provider.endpoint.myai]
kind       = \"openai\"   # aligned by hand
base_url   = \"https://myai.test/v1\"
credential = \"secret://os/myai\"

[ui]
color = \"always\"
";

        let added = append_endpoint(original, &endpoint());
        assert!(added.starts_with(original), "the original file moved");
        assert!(added.contains("[provider.endpoint.acme]"));
        assert!(added.contains("credential = \"secret://file/acme.key\""));
        // The first model is the default and the rest are listed beside it.
        assert!(added.contains("model = \"acme-1\""), "{added}");
        assert!(added.contains("models = [\"acme-2\"]"), "{added}");

        // Removing gives back a file that still has the comment, the hand
        // alignment, and the unrelated table.
        let removed = remove_endpoint(&added, "acme");
        assert_eq!(removed, original, "removal was not the inverse of adding");

        let removed = remove_endpoint(original, "myai");
        assert!(!removed.contains("[provider.endpoint.myai]"));
        assert!(!removed.contains("https://myai.test"));
        assert!(
            removed.contains("# the endpoint I actually use"),
            "a comment above the table is not part of it: {removed}"
        );
        assert!(removed.contains("[ui]"), "an unrelated table went with it");
        assert!(removed.contains("color = \"always\""));
        assert!(!removed.contains("\n\n\n"), "a hole was left behind");

        // A name that is not there changes nothing at all.
        assert_eq!(remove_endpoint(original, "nothere"), original);
    }

    #[test]
    fn the_default_is_retargeted_added_or_given_a_table() {
        let with_key =
            "schema_version = 1\n\n[provider]\ndefault = \"myai\"\n\n[ui]\ncolor = \"never\"\n";
        let out = set_default(with_key, "acme");
        assert!(out.contains("default = \"acme\""));
        assert!(!out.contains("\"myai\""), "the old default survived: {out}");
        assert!(out.contains("[ui]"));
        assert_eq!(out.matches("default").count(), 1);

        // A `[provider]` table without the key gets it, under that header.
        let no_key =
            "schema_version = 1\n\n[provider]\n\n[provider.endpoint.acme]\nkind = \"openai\"\n";
        let out = set_default(no_key, "acme");
        let lines: Vec<&str> = out.lines().collect();
        let table = lines.iter().position(|line| *line == "[provider]").unwrap();
        assert_eq!(lines[table + 1], "default = \"acme\"");

        // No table at all: one is created before the first table, so the key
        // cannot end up under someone else's header.
        let none = "schema_version = 1\n\n[provider.endpoint.acme]\nkind = \"openai\"\n";
        let out = set_default(none, "acme");
        let lines: Vec<&str> = out.lines().collect();
        let table = lines.iter().position(|line| *line == "[provider]").unwrap();
        let endpoint = lines
            .iter()
            .position(|line| *line == "[provider.endpoint.acme]")
            .unwrap();
        assert_eq!(lines[table + 1], "default = \"acme\"");
        assert!(
            table < endpoint,
            "the default landed under the endpoint: {out}"
        );
    }

    #[test]
    fn a_new_file_gets_the_schema_header_once() {
        assert_eq!(ensure_schema(""), "schema_version = 1\n");
        let once = ensure_schema("[provider]\n");
        assert_eq!(once, "schema_version = 1\n\n[provider]\n");
        assert_eq!(ensure_schema(&once), once, "the header was added twice");
    }

    /// These values are written into TOML rather than escaped into it, so a
    /// value that would need escaping is refused before it can produce a file
    /// that no longer loads.
    #[test]
    fn a_value_that_would_break_the_file_is_refused() {
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
}
