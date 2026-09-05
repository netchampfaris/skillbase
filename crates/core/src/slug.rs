//! Skill-name shape: the kebab-case predicate and a slugifier.

use crate::error::MAX_NAME_LEN;

/// The name used when [`slugify`] is handed text with nothing usable in it.
pub const FALLBACK_SLUG: &str = "skill";

/// True when `s` matches `^[a-z0-9]+(-[a-z0-9]+)*$`.
///
/// That is: ASCII lowercase letters and digits, single hyphens between groups,
/// no leading, trailing or doubled hyphen, not empty.
pub fn is_kebab_case(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Starting in the "just saw a separator" state rejects a leading hyphen,
    // and ending in it rejects a trailing one.
    let mut after_separator = true;
    for c in s.chars() {
        if c == '-' {
            if after_separator {
                return false;
            }
            after_separator = true;
        } else if c.is_ascii_lowercase() || c.is_ascii_digit() {
            after_separator = false;
        } else {
            return false;
        }
    }
    !after_separator
}

/// Turns arbitrary text into a name that satisfies [`is_kebab_case`].
///
/// ASCII letters are lowercased, ASCII digits kept, and every other character —
/// punctuation, whitespace and any non-ASCII character — acts as a separator.
/// Runs of separators collapse to a single hyphen, and the result is trimmed to
/// [`MAX_NAME_LEN`] characters without leaving a trailing hyphen.
///
/// Non-ASCII text is dropped rather than transliterated, so `"Café Skill"`
/// becomes `"caf-skill"`. Input with no ASCII alphanumerics at all yields
/// [`FALLBACK_SLUG`], because an empty string is not a valid name.
///
/// ```
/// use skillbase_core::slugify;
/// assert_eq!(slugify("My Great Skill!"), "my-great-skill");
/// assert_eq!(slugify("  ___  "), "skill");
/// ```
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_separator = false;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_separator && !out.is_empty() {
                out.push('-');
            }
            pending_separator = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }

    if out.len() > MAX_NAME_LEN {
        out.truncate(MAX_NAME_LEN);
        while out.ends_with('-') {
            out.pop();
        }
    }

    if out.is_empty() {
        return FALLBACK_SLUG.to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_case_accepts_valid_names() {
        for name in ["a", "skill", "my-skill", "pdf-2-csv", "a1-b2-c3", "007"] {
            assert!(is_kebab_case(name), "{name} should be kebab-case");
        }
    }

    #[test]
    fn kebab_case_rejects_invalid_names() {
        for name in [
            "",
            "-skill",
            "skill-",
            "my--skill",
            "My-Skill",
            "my_skill",
            "my skill",
            "my.skill",
            "café",
        ] {
            assert!(!is_kebab_case(name), "{name} should not be kebab-case");
        }
    }

    #[test]
    fn slugify_normalizes_arbitrary_text() {
        assert_eq!(slugify("My Great Skill!"), "my-great-skill");
        assert_eq!(slugify("PDF -> CSV"), "pdf-csv");
        assert_eq!(slugify("  leading and trailing  "), "leading-and-trailing");
        assert_eq!(slugify("snake_case_name"), "snake-case-name");
        assert_eq!(slugify("already-kebab"), "already-kebab");
        assert_eq!(slugify("Version 2.0"), "version-2-0");
    }

    #[test]
    fn slugify_drops_non_ascii_and_falls_back_when_empty() {
        assert_eq!(slugify("Café Skill"), "caf-skill");
        assert_eq!(slugify("日本語"), FALLBACK_SLUG);
        assert_eq!(slugify(""), FALLBACK_SLUG);
        assert_eq!(slugify("  ___  "), FALLBACK_SLUG);
    }

    #[test]
    fn slugify_truncates_without_a_trailing_hyphen() {
        let long = "word ".repeat(40);
        let slug = slugify(&long);
        assert!(slug.chars().count() <= MAX_NAME_LEN);
        assert!(!slug.ends_with('-'));
        assert!(is_kebab_case(&slug));
    }

    #[test]
    fn slugify_output_is_always_kebab_case() {
        for input in [
            "A",
            "!!!x!!!",
            "1",
            "Hello, World — again",
            "----",
            "x".repeat(200).as_str(),
        ] {
            assert!(is_kebab_case(&slugify(input)), "failed for {input:?}");
        }
    }
}
