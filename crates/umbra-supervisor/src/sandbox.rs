//! Runtime rendering of the single Seatbelt policy template.
//!
//! `experiments/seatbelt/umbra.sb` is the one source of policy text, embedded at
//! compile time so a runtime file read, a stale copy, or a missing experiment
//! directory cannot change enforcement. Rendering substitutes exactly one token,
//! [`RUN_ROOT_TOKEN`], with a complete quoted Seatbelt string literal naming the
//! opened run's own root binding, and refuses anything it cannot represent
//! exactly. Nothing here touches the filesystem or installs a policy: this module
//! validates *representation*, preparation validates the run binding, and the
//! platform backend owns installation.

use umbra_core::{
    BytePath, ErrorKind, Result, SandboxProfile, UmbraError, MAX_SANDBOX_PROFILE_BYTES,
    SEATBELT_PROFILE_FORMAT,
};

/// The one template source. A second template would be a second policy.
const TEMPLATE: &str = include_str!("../../../experiments/seatbelt/umbra.sb");

/// The only substitution point; it stands for a whole quoted string literal.
pub const RUN_ROOT_TOKEN: &str = "{{UMBRA_RUN_ROOT}}";

/// What the profile must permit: writes under exactly one absolute run root.
///
/// This is the runtime physical path of the opened run's `root` binding, never
/// the backing store as a whole, a sibling run, `control/`, or a base snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxSpec {
    /// Absolute physical path of this run's tracee-visible root.
    pub write_root: BytePath,
}

/// Render the embedded template for one run root.
///
/// Rejects a relative root, `/` itself, trailing separators, NUL and other
/// control bytes, and any byte sequence that is not valid UTF-8, because a
/// Seatbelt literal has no escape for bytes outside the profile's encoding and a
/// lossy conversion would silently widen or narrow the granted path. Quotes and
/// backslashes are escaped rather than rejected, so ordinary Unicode paths work.
pub fn render(spec: &SandboxSpec) -> Result<SandboxProfile> {
    let literal = quote_seatbelt_path(&spec.write_root)?;
    let occurrences = TEMPLATE.matches(RUN_ROOT_TOKEN).count();
    if occurrences != 1 {
        return Err(invalid(format!(
            "sandbox template must contain exactly one {RUN_ROOT_TOKEN}, found {occurrences}"
        )));
    }
    let source = TEMPLATE.replace(RUN_ROOT_TOKEN, &literal);
    if source.contains("{{") || source.contains("}}") {
        return Err(invalid(
            "sandbox template still contains an unresolved token after rendering",
        ));
    }
    if source.len() > MAX_SANDBOX_PROFILE_BYTES {
        return Err(invalid("rendered sandbox profile exceeds its size bound"));
    }
    SandboxProfile::new(
        SEATBELT_PROFILE_FORMAT,
        source.into_bytes(),
        spec.write_root.clone(),
    )
}

/// Produce a complete Seatbelt string literal, including its quotes.
fn quote_seatbelt_path(path: &BytePath) -> Result<String> {
    let bytes = path.as_bytes();
    if !path.is_absolute() {
        return Err(invalid_path("sandbox write root must be absolute"));
    }
    if bytes == b"/" {
        return Err(invalid_path(
            "sandbox write root must not be the filesystem root",
        ));
    }
    if bytes.len() > 1 && bytes.ends_with(b"/") {
        return Err(invalid_path(
            "sandbox write root must not have a trailing separator",
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        invalid_path("sandbox write root is not valid UTF-8 and cannot be quoted exactly")
    })?;
    if text.chars().any(|c| c.is_control()) {
        return Err(invalid_path(
            "sandbox write root contains control characters",
        ));
    }
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            literal.push('\\');
        }
        literal.push(c);
    }
    literal.push('"');
    Ok(literal)
}

fn invalid(context: impl Into<String>) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidInput, "sandbox.render", context)
}

fn invalid_path(context: &str) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidPath, "sandbox.render", context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(bytes: &[u8]) -> SandboxSpec {
        SandboxSpec {
            write_root: BytePath::new(bytes.to_vec()).unwrap(),
        }
    }

    #[test]
    fn rendering_binds_writes_to_exactly_the_supplied_run_root() {
        let profile = render(&spec(b"/mnt/store/2f/root")).unwrap();
        let source = String::from_utf8(profile.source().to_vec()).unwrap();
        assert!(source.contains(r#"(allow file-write* (subpath "/mnt/store/2f/root"))"#));
        assert!(!source.contains("{{"));
        assert_eq!(profile.format(), SEATBELT_PROFILE_FORMAT);
        assert_eq!(profile.write_root().as_bytes(), b"/mnt/store/2f/root");
    }

    #[test]
    fn no_pinned_mount_literal_survives_in_the_template() {
        assert!(!TEMPLATE.contains("/mnt/umbra-nfs"));
        assert_eq!(TEMPLATE.matches(RUN_ROOT_TOKEN).count(), 1);
    }

    #[test]
    fn the_only_persistent_write_rule_is_the_rendered_root() {
        let source =
            String::from_utf8(render(&spec(b"/runs/a/root")).unwrap().source().to_vec()).unwrap();
        let writes: Vec<_> = source
            .lines()
            .filter(|line| line.trim_start().starts_with("(allow file-write"))
            .collect();
        // One subpath rule for the run root, plus the measured /dev/null sink.
        assert_eq!(
            writes,
            vec![
                r#"(allow file-write* (subpath "/runs/a/root"))"#,
                r#"(allow file-write-data (literal "/dev/null"))"#
            ]
        );
    }

    #[test]
    fn quotes_backslashes_and_unicode_are_escaped_not_dropped() {
        let source = String::from_utf8(
            render(&spec("/runs/a b\"c\\d/ünïcode/root".as_bytes()))
                .unwrap()
                .source()
                .to_vec(),
        )
        .unwrap();
        assert!(source.contains(r#"(subpath "/runs/a b\"c\\d/ünïcode/root")"#));
    }

    #[test]
    fn unrepresentable_or_overbroad_roots_fail_before_any_launch() {
        for bytes in [
            b"relative/root".as_slice(),
            b"/",
            b"/runs/a/root/",
            b"/runs/a\x07/root",
            b"/runs/\xff/root",
        ] {
            let error = render(&spec(bytes)).unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidPath, "accepted {bytes:?}");
        }
        assert!(BytePath::new(b"/runs/a\0/root".to_vec()).is_err());
    }
}
