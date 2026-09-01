//! Static assets embedded in the executable.
//!
//! Firestone ships one file. The UI therefore carries its own script, style
//! and font bytes rather than reaching for a CDN, which also lets the served
//! Content-Security-Policy restrict every source to 'self': a host with no
//! outbound network still renders the UI exactly as designed.

use axum::{
    body::Body,
    http::{
        HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
    response::Response,
};

/// One embedded file: request path suffix, media type, bytes.
struct Asset {
    name: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset {
        name: "app.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../../assets/ui/app.css"),
    },
    Asset {
        name: "app.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../assets/ui/app.js"),
    },
    Asset {
        name: "theme.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../assets/ui/theme.js"),
    },
    Asset {
        name: "htmx.min.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../assets/ui/htmx.min.js"),
    },
    Asset {
        name: "idiomorph-ext.min.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../assets/ui/idiomorph-ext.min.js"),
    },
    Asset {
        name: "logo.svg",
        content_type: "image/svg+xml",
        bytes: include_bytes!("../../assets/ui/logo.svg"),
    },
    Asset {
        name: "fonts/ibm-plex-sans-latin-400.woff2",
        content_type: "font/woff2",
        bytes: include_bytes!("../../assets/ui/fonts/ibm-plex-sans-latin-400.woff2"),
    },
    Asset {
        name: "fonts/ibm-plex-sans-latin-500.woff2",
        content_type: "font/woff2",
        bytes: include_bytes!("../../assets/ui/fonts/ibm-plex-sans-latin-500.woff2"),
    },
    Asset {
        name: "fonts/ibm-plex-sans-latin-600.woff2",
        content_type: "font/woff2",
        bytes: include_bytes!("../../assets/ui/fonts/ibm-plex-sans-latin-600.woff2"),
    },
    Asset {
        name: "fonts/ibm-plex-mono-latin-400.woff2",
        content_type: "font/woff2",
        bytes: include_bytes!("../../assets/ui/fonts/ibm-plex-mono-latin-400.woff2"),
    },
    Asset {
        name: "fonts/ibm-plex-mono-latin-500.woff2",
        content_type: "font/woff2",
        bytes: include_bytes!("../../assets/ui/fonts/ibm-plex-mono-latin-500.woff2"),
    },
];

/// Every asset URL carries `?v=<build>`, so a cached copy can never outlive
/// the binary that served it and the cache entry is safely immutable.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Serves one embedded asset, or 404 for an unknown name.
///
/// The name is matched against a closed table rather than joined onto a
/// directory, so no request can traverse out of the asset set.
pub fn serve(name: &str) -> Response {
    let Some(asset) = ASSETS.iter().find(|asset| asset.name == name) else {
        return not_found();
    };

    let mut response = Response::new(Body::from(asset.bytes));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(asset.content_type));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
    response
}

fn not_found() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::ASSETS;

    #[test]
    fn every_asset_has_bytes_and_a_unique_name() {
        let mut names: Vec<&str> = ASSETS.iter().map(|asset| asset.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate asset name");

        for asset in ASSETS {
            assert!(!asset.bytes.is_empty(), "{} is empty", asset.name);
        }
    }

    #[test]
    fn vendored_fonts_are_woff2() {
        for asset in ASSETS
            .iter()
            .filter(|asset| asset.content_type == "font/woff2")
        {
            assert_eq!(
                &asset.bytes[..4],
                b"wOF2",
                "{} is not a woff2 file",
                asset.name
            );
        }
    }

    #[test]
    fn the_morph_extension_registers_itself_with_htmx() {
        let extension = ASSETS
            .iter()
            .find(|asset| asset.name == "idiomorph-ext.min.js")
            .expect("idiomorph extension asset");
        let source = String::from_utf8_lossy(extension.bytes);
        // The machines table and every live region swap with morph:innerHTML.
        // If the vendored bundle ever stops registering the extension those
        // swaps silently degrade to a full replace, destroying focus and
        // selection on every poll tick.
        assert!(
            source.contains("defineExtension"),
            "the vendored idiomorph build does not register an htmx extension"
        );
        assert!(
            source.contains("Idiomorph"),
            "the vendored idiomorph build does not bundle the morph core"
        );
    }

    #[test]
    fn the_vendored_htmx_still_fires_the_cancellable_poll_hook() {
        // The 5 s live-region polls are suppressed while a mutation stream is
        // open, so a transition the user is watching is never repainted from
        // under them. With no 'unsafe-eval' an htmx trigger filter is not
        // available, so app.js cancels htmx's own `hx:poll:trigger` instead.
        // If an htmx upgrade renames or drops that hook the gate stops working
        // silently, which is exactly the kind of regression a browser would
        // not reveal until someone watched a row flicker mid-start.
        let htmx = ASSETS
            .iter()
            .find(|asset| asset.name == "htmx.min.js")
            .expect("htmx asset");
        let source = String::from_utf8_lossy(htmx.bytes);
        assert!(
            source.contains("hx:poll:trigger"),
            "the vendored htmx no longer fires a cancellable poll hook"
        );

        let app = ASSETS
            .iter()
            .find(|asset| asset.name == "app.js")
            .expect("app.js asset");
        let app_source = String::from_utf8_lossy(app.bytes);
        assert!(
            app_source.contains("htmx:poll:trigger"),
            "app.js no longer listens for the poll hook"
        );
    }

    #[test]
    fn embedded_scripts_and_styles_avoid_constructs_the_csp_forbids() {
        // The served policy has neither 'unsafe-inline' nor 'unsafe-eval'.
        // htmx compiles trigger filters, hx-on handlers and js: values with
        // new Function(), so the UI must not use them.
        let app = ASSETS
            .iter()
            .find(|asset| asset.name == "app.js")
            .expect("app.js asset");
        let source = strip_comments(&String::from_utf8_lossy(app.bytes));
        assert!(!source.contains("eval("), "app.js must not call eval");
        assert!(
            !source.contains("new Function"),
            "app.js must not construct functions from strings"
        );
    }

    /// Removes block and line comments so prose about a forbidden construct
    /// is not mistaken for a use of it.
    fn strip_comments(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut rest = source;
        while let Some(start) = rest.find("/*") {
            out.push_str(&rest[..start]);
            match rest[start + 2..].find("*/") {
                Some(end) => rest = &rest[start + 2 + end + 2..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        out.push_str(rest);
        out.lines()
            .map(|line| match line.find("//") {
                Some(index) => &line[..index],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
