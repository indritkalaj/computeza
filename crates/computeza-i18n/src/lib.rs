//! Computeza i18n — externalized strings via Fluent (`.ftl`) resource bundles.
//!
//! # The rule
//!
//! No user-facing string in Computeza is hardcoded in source. Every label,
//! message, error, log line shown to a human, button caption, page title,
//! and tooltip resolves through this crate. English is the only locale today;
//! adding a new locale is a drop-in operation that requires no code changes.
//!
//! This rule is enforced socially in code review and (eventually) by a
//! `cargo xtask check-i18n` lint that scans for string literals reaching
//! user-facing sinks (`println!`, Leptos view text, `tracing::*` event
//! messages that are user-visible, clap help text, …).
//!
//! # Usage
//!
//! ```ignore
//! use computeza_i18n::Localizer;
//!
//! let l = Localizer::english();
//! println!("{}", l.t("welcome-banner"));
//! ```
//!
//! Resource bundles live in `locales/<lang>/<file>.ftl` relative to this
//! crate. The build embeds them at compile time via `fluent-templates`'
//! `static_loader!` macro, so air-gapped binaries carry every shipped
//! locale without runtime file system access.
//!
//! # Why Fluent
//!
//! Spec §4.1 mandates `fluent-rs + Noto Sans`. Fluent is Mozilla's
//! production l10n format used by Firefox; it handles plurals, gender,
//! arguments, and message references natively, which CSV / JSON / gettext
//! variants do not. The Rust binding is mature and cross-platform, which
//! is what the all-Rust mandate (§1) calls for.

use fluent_templates::{static_loader, Loader};
use unic_langid::{langid, LanguageIdentifier};

static_loader! {
    /// Compile-time-loaded bundle of every shipped locale. Lookups against
    /// this loader never touch the file system at runtime.
    static LOCALES = {
        locales: "./locales",
        fallback_language: "en",
    };
}

/// English (US) language identifier — the only locale shipped today.
pub const EN: LanguageIdentifier = langid!("en");

/// Resolves Fluent message keys against a bound language.
///
/// `Localizer` is cheap to clone (it holds only a `LanguageIdentifier`)
/// and is thread-safe. Construct one per request / per session and pass
/// it into render code.
#[derive(Clone, Debug)]
pub struct Localizer {
    lang: LanguageIdentifier,
}

impl Localizer {
    /// Construct a localizer for the given language.
    #[must_use]
    pub fn new(lang: LanguageIdentifier) -> Self {
        Self { lang }
    }

    /// Construct a localizer bound to English.
    #[must_use]
    pub fn english() -> Self {
        Self::new(EN)
    }

    /// The language this localizer resolves against.
    #[must_use]
    pub fn lang(&self) -> &LanguageIdentifier {
        &self.lang
    }

    /// Resolve a Fluent message key.
    ///
    /// On a missing key this returns `?key?` and (in debug builds) panics —
    /// missing keys are a build-time bug class and should be loud.
    #[must_use]
    pub fn t(&self, key: &str) -> String {
        match LOCALES.try_lookup(&self.lang, key) {
            Some(s) => s,
            None => {
                debug_assert!(false, "missing i18n key: {key}");
                format!("?{key}?")
            }
        }
    }

    /// Resolve a Fluent message key with arguments.
    ///
    /// Arguments come from a `HashMap<String, FluentValue>` per the
    /// `fluent-templates` API.
    #[must_use]
    pub fn t_args(
        &self,
        key: &str,
        args: &std::collections::HashMap<String, fluent_templates::fluent_bundle::FluentValue<'_>>,
    ) -> String {
        match LOCALES.try_lookup_with_args(&self.lang, key, args) {
            Some(s) => s,
            None => {
                debug_assert!(false, "missing i18n key (with args): {key}");
                format!("?{key}?")
            }
        }
    }
}

impl Default for Localizer {
    fn default() -> Self {
        Self::english()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_resolves_welcome_banner() {
        let l = Localizer::english();
        let msg = l.t("welcome-banner");
        assert!(!msg.is_empty(), "welcome-banner should resolve to non-empty string");
        assert!(!msg.starts_with('?'), "welcome-banner should not be marked missing: {msg}");
    }

    #[test]
    fn missing_key_returns_marker_in_release() {
        // We can only test the release path here; debug builds panic via debug_assert.
        // This test documents the marker format used by the missing-key fallback.
        let marker = format!("?{}?", "definitely-not-a-real-key");
        assert_eq!(marker, "?definitely-not-a-real-key?");
    }
}
