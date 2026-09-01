//! Template environment for the embedded web UI.
//!
//! Every template is compiled into the executable with `include_str!`, so the
//! UI has no runtime file dependency and a Firestone binary copied to a bare
//! host still serves it. The environment is built once and shared.

use std::sync::OnceLock;

use firestone_core::{ErrorKind, FirestoneError};
use minijinja::{Environment, Value};

/// Every template, registered under the name templates `include` each other by.
const TEMPLATES: &[(&str, &str)] = &[
    (
        "ui/_macros.html",
        include_str!("../../templates/ui/_macros.html"),
    ),
    (
        "ui/shell.html",
        include_str!("../../templates/ui/shell.html"),
    ),
    (
        "ui/_sidebar.html",
        include_str!("../../templates/ui/_sidebar.html"),
    ),
    (
        "ui/_topbar.html",
        include_str!("../../templates/ui/_topbar.html"),
    ),
    (
        "ui/_hostpill.html",
        include_str!("../../templates/ui/_hostpill.html"),
    ),
    (
        "ui/overview.html",
        include_str!("../../templates/ui/overview.html"),
    ),
    (
        "ui/_stats.html",
        include_str!("../../templates/ui/_stats.html"),
    ),
    (
        "ui/_overview_machines.html",
        include_str!("../../templates/ui/_overview_machines.html"),
    ),
    (
        "ui/machines.html",
        include_str!("../../templates/ui/machines.html"),
    ),
    (
        "ui/_machine_rows.html",
        include_str!("../../templates/ui/_machine_rows.html"),
    ),
    (
        "ui/detail.html",
        include_str!("../../templates/ui/detail.html"),
    ),
    (
        "ui/_detail_head.html",
        include_str!("../../templates/ui/_detail_head.html"),
    ),
    (
        "ui/tab_spec.html",
        include_str!("../../templates/ui/tab_spec.html"),
    ),
    (
        "ui/tab_logs.html",
        include_str!("../../templates/ui/tab_logs.html"),
    ),
    (
        "ui/tab_vmconfig.html",
        include_str!("../../templates/ui/tab_vmconfig.html"),
    ),
    (
        "ui/catalog.html",
        include_str!("../../templates/ui/catalog.html"),
    ),
    (
        "ui/_catalog_cards.html",
        include_str!("../../templates/ui/_catalog_cards.html"),
    ),
    (
        "ui/create.html",
        include_str!("../../templates/ui/create.html"),
    ),
    (
        "ui/_spec_fields.html",
        include_str!("../../templates/ui/_spec_fields.html"),
    ),
    (
        "ui/_image_picker.html",
        include_str!("../../templates/ui/_image_picker.html"),
    ),
    (
        "ui/palette.html",
        include_str!("../../templates/ui/palette.html"),
    ),
    (
        "ui/error.html",
        include_str!("../../templates/ui/error.html"),
    ),
    // Its own document rather than a body inside ui/shell.html: a terminal
    // wants the whole window, not the sidebar chrome.
    (
        "ui/terminal.html",
        include_str!("../../templates/ui/terminal.html"),
    ),
];

static ENVIRONMENT: OnceLock<Result<Environment<'static>, String>> = OnceLock::new();

fn environment() -> Result<&'static Environment<'static>, FirestoneError> {
    match ENVIRONMENT.get_or_init(build_environment) {
        Ok(environment) => Ok(environment),
        Err(message) => Err(FirestoneError::new(
            ErrorKind::Generic,
            format!("the embedded web UI templates failed to compile: {message}"),
        )
        .with_hint("this is a Firestone build defect; report it with the version string")),
    }
}

fn build_environment() -> Result<Environment<'static>, String> {
    let mut environment = Environment::new();
    // minijinja gates its own urlencode behind a feature. Registering one
    // keeps the encoding identical to the paths the handlers build, so a
    // machine name with a slash or a space links to exactly the route that
    // will match it.
    environment.add_filter("urlencode", |value: String| urlencode(&value));
    // Every template is named *.html, so minijinja's default callback selects
    // HTML auto-escaping for all of them. Machine names, image references and
    // log text are attacker-influenced in the general case; none of them is
    // ever interpolated unescaped.
    for (name, source) in TEMPLATES {
        environment
            .add_template(name, source)
            .map_err(|error| format!("{name}: {error}"))?;
    }
    Ok(environment)
}

/// Percent-encodes one URL path segment, keeping only the RFC 3986
/// unreserved set.
pub fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(*byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Renders one registered template with the supplied context.
pub fn render(name: &str, context: Value) -> Result<String, FirestoneError> {
    let environment = environment()?;
    let template = environment.get_template(name).map_err(|error| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("the embedded web UI template {name} is missing: {error}"),
        )
        .with_hint("this is a Firestone build defect; report it with the version string")
    })?;
    template.render(context).map_err(|error| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("the embedded web UI template {name} failed to render: {error}"),
        )
        .with_hint("this is a Firestone build defect; report it with the version string")
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{TEMPLATES, environment};

    #[test]
    fn every_embedded_template_compiles() {
        let environment = environment().expect("the embedded templates must compile");
        for (name, _) in TEMPLATES {
            assert!(
                environment.get_template(name).is_ok(),
                "template {name} is not registered"
            );
        }
    }

    #[test]
    fn template_names_are_unique() {
        let mut names: Vec<&str> = TEMPLATES.iter().map(|(name, _)| *name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate template name");
    }
}
