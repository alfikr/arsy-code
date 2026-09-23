//! The remembered `/model`, `/effort` and `/theme` choices: where they live
//! beside the user configuration, and how a session reads them back.

#[cfg(feature = "tui")]
use crate::*;
use arsy_kernel::provider::Effort;
use std::io::{self, Write};
use std::path::PathBuf;
/// Persist the picked model, reporting only that persistence failed — the
/// choice still applies to this session.
#[cfg(feature = "tui")]
pub(crate) fn remember_model(route: &tui::ModelRoute, emitter: &mut Emitter) {
    if let Err(error) = save_route(route) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the model choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

#[cfg(feature = "tui")]
pub(crate) fn remember_effort(effort: Option<Effort>, emitter: &mut Emitter) {
    if let Err(error) = save_effort(effort) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the effort choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

/// The remembered model lives beside the user configuration layer that
/// `arsy doctor` already reports.
#[cfg(feature = "tui")]
fn model_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("model"))
}

/// The route chosen last time, as `provider/model`.
///
/// The model is re-validated on read: a file written by an older build that
/// accepted anything must not keep selecting an unusable model on every later
/// start.
#[cfg(feature = "tui")]
pub(crate) fn saved_route() -> Option<tui::ModelRoute> {
    let raw = std::fs::read_to_string(model_store()?).ok()?;
    let raw = raw.trim();
    let route = (!raw.is_empty())
        .then(|| tui::ModelRoute::parse(raw))
        .flatten()?;
    tui::validate_slug(&route.model).ok()?;
    Some(route)
}

#[cfg(feature = "tui")]
pub(crate) fn save_route(route: &tui::ModelRoute) -> io::Result<()> {
    let path = model_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    replace_file(&path, format!("{route}\n").as_bytes())
}

/// The remembered reasoning effort, beside the remembered model.
#[cfg(feature = "tui")]
#[cfg(feature = "tui")]
fn effort_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("effort"))
}

/// The effort chosen last time, re-validated on read for the same reason the
/// model is: an unreadable file must not decide what a turn sends.
#[cfg(feature = "tui")]
pub(crate) fn saved_effort() -> Option<Effort> {
    Effort::parse(std::fs::read_to_string(effort_store()?).ok()?.trim())
}

/// `None` clears the choice, so a turn goes back to carrying no reasoning knob.
#[cfg(feature = "tui")]
pub(crate) fn save_effort(effort: Option<Effort>) -> io::Result<()> {
    let path = effort_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    match effort {
        Some(effort) => replace_file(&path, format!("{effort}\n").as_bytes()),
        None => match std::fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        },
    }
}

/// The remembered colour theme, beside the remembered effort.
#[cfg(feature = "tui")]
fn theme_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("theme"))
}

/// The theme chosen last time, kept only if it is still a built-in name: a
/// file written by a build that knew a theme this one dropped must not select
/// nothing.
#[cfg(feature = "tui")]
pub(crate) fn saved_theme() -> Option<String> {
    let raw = std::fs::read_to_string(theme_store()?).ok()?;
    let name = raw.trim().to_owned();
    tui::builtin_palette(&name).map(|_| name)
}

#[cfg(feature = "tui")]
pub(crate) fn save_theme(name: &str) -> io::Result<()> {
    let path = theme_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    replace_file(&path, format!("{name}\n").as_bytes())
}

#[cfg(feature = "tui")]
pub(crate) fn remember_theme(name: &str, emitter: &mut Emitter) {
    if let Err(error) = save_theme(name) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the theme choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

#[cfg(feature = "tui")]
pub(crate) fn apply_theme(
    answer: &str,
    current: &mut String,
    roles: &std::collections::BTreeMap<String, String>,
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> io::Result<bool> {
    let picked = match tui::resolve_theme_answer(answer, current) {
        Ok(picked) => picked,
        Err(reason) => {
            writeln!(stdout, "{}", tui::safe_text(&reason))?;
            return Ok(false);
        }
    };
    tui::set_palette(&picked, roles);
    *current = picked;
    remember_theme(current, emitter);
    writeln!(stdout, "Theme: {current}")?;
    Ok(true)
}

#[cfg(feature = "tui")]
pub(crate) fn endpoint_models(invocation: &Invocation) -> Vec<tui::ModelChoice> {
    crate::provider::configuration(invocation)
        .map(|config| {
            config
                .endpoints()
                .flat_map(|endpoint| {
                    endpoint.models.iter().map(move |slug| tui::ModelChoice {
                        provider: endpoint.id.clone(),
                        slug: slug.clone(),
                        name: format!("on {}", endpoint.id),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The palette the session paints with: a built-in base — the `[theme]` base,
/// else the remembered theme, else the default — with any `[theme]` role
/// overrides on top. Returns the base name (for the `/theme` picker) and the
/// palette, or the reason an override was rejected.
#[cfg(feature = "tui")]
pub(crate) fn resolve_palette(
    theme: &arsy_kernel::config::Theme,
) -> (String, Result<tui::Palette, String>) {
    let base = theme
        .base
        .clone()
        .or_else(saved_theme)
        .unwrap_or_else(|| tui::DEFAULT_THEME.to_owned());
    let palette = tui::builtin_palette(&base).unwrap_or_else(|| {
        tui::builtin_palette(tui::DEFAULT_THEME).expect("the default theme is built in")
    });
    let built = if theme.roles.is_empty() {
        Ok(palette)
    } else {
        palette.with_overrides(&theme.roles)
    };
    (base, built)
}
