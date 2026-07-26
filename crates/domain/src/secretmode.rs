//! The file mode `rpi` gives to materialized secrets.
//!
//! Secret files are consumed by containers whose uid is unrelated to the
//! agent's, so the default has to be readable by others; the exact value is
//! configurable per project via `[secrets].file_mode`. The permitted set is
//! described by a rule rather than a list: the owner reads (and may write),
//! group and others may only read.

/// Mode for files listed in `[secrets].files` when no `file_mode` is set.
pub const DEFAULT_SECRET_FILE_MODE: u32 = 0o644;

/// Mode for the injected `.env` when no `file_mode` is set. Compose reads it
/// as the agent, so nothing needs it wider by default.
pub const DEFAULT_ENV_MODE: u32 = 0o600;

/// Parses `"0644"` / `"644"` into `0o644` and validates it.
pub fn parse(text: &str) -> Result<u32, String> {
    let digits = match text.len() {
        3 => text,
        4 if text.starts_with('0') => &text[1..],
        _ => {
            return Err(format!(
                "'{text}' is not a three-digit octal file mode (e.g. \"0644\")"
            ));
        }
    };
    let mut mode = 0u32;
    for c in digits.chars() {
        let digit = c.to_digit(8).ok_or_else(|| {
            format!("'{text}' is not a three-digit octal file mode (e.g. \"0644\")")
        })?;
        mode = mode * 8 + digit;
    }
    validate(mode)?;
    Ok(mode)
}

/// The bit rule, applied to an already-parsed mode: the owner must be able to
/// read and may write; group and others may only read. Execute bits are
/// refused because a secret is not a program, and write for anyone but the
/// owner because `rpi` overwrites the file on every deploy anyway.
pub fn validate(mode: u32) -> Result<(), String> {
    if mode & !0o777 != 0 {
        return Err(format!(
            "mode {mode:04o} sets setuid/setgid/sticky bits, which are not allowed for secret files"
        ));
    }
    if mode & 0o111 != 0 {
        return Err(format!(
            "mode {mode:04o} sets execute bits, which are not allowed for secret files"
        ));
    }
    if mode & 0o022 != 0 {
        return Err(format!(
            "mode {mode:04o} is writable by group or others, which is not allowed for secret files"
        ));
    }
    if mode & 0o400 == 0 {
        return Err(format!(
            "mode {mode:04o} is not readable by its owner (the agent), which cannot be right"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_documented_modes() {
        for (text, expected) in [
            ("0600", 0o600),
            ("600", 0o600),
            ("0640", 0o640),
            ("0644", 0o644),
            ("0400", 0o400),
            ("0440", 0o440),
            ("0444", 0o444),
            ("0604", 0o604),
        ] {
            assert_eq!(parse(text), Ok(expected), "{text}");
        }
    }

    #[test]
    fn rejects_execute_bits() {
        let err = parse("0755").unwrap_err();
        assert!(err.contains("execute"), "{err}");
    }

    #[test]
    fn rejects_write_for_group_or_other() {
        assert!(parse("0660").unwrap_err().contains("writable"));
        assert!(parse("0666").unwrap_err().contains("writable"));
    }

    #[test]
    fn rejects_a_mode_the_owner_cannot_read() {
        assert!(parse("0244").unwrap_err().contains("owner"));
    }

    #[test]
    fn rejects_setuid_setgid_and_sticky_by_shape() {
        for text in ["4644", "2644", "1644", "04644"] {
            assert!(parse(text).is_err(), "{text} must be rejected");
        }
    }

    #[test]
    fn rejects_malformed_text() {
        for text in ["", "0", "64", "0648", "0o644", "644 ", "abc"] {
            assert!(parse(text).is_err(), "{text} must be rejected");
        }
    }
}
