//! The configuration variable engine (spec 2026-07-26).
//!
//! Three disjoint namespaces, told apart by syntax alone:
//!
//! - `${NAME}` — a user variable supplied via `--vars`.
//! - `${ns.field}` — a resolver input: a fact about the machine running rpi
//!   (`git.*`) or about the resolution itself (`env.*`).
//! - `RPI_*` — runtime variables. They exist only inside containers and
//!   exec'd processes, never in a TOML file, so a reference to one here is a
//!   dedicated error rather than "unknown variable".
//!
//! The syntactic split is the point: nobody who sees `${git.branch}` in a
//! TOML file will go looking for `git.branch` in `printenv`.

use std::collections::BTreeMap;

/// The namespaces `${ns.field}` accepts.
const NAMESPACES: &[&str] = &["git", "env"];

/// One `${...}` reference found in a configuration string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum VarRef {
    User(String),
    /// `Input(namespace, field)` — both already validated as lowercase.
    Input(String, String),
}

impl VarRef {
    /// The reference as it is written in a TOML file, without `${}`.
    pub fn name(&self) -> String {
        match self {
            VarRef::User(name) => name.clone(),
            VarRef::Input(ns, field) => format!("{ns}.{field}"),
        }
    }
}

/// Everything one substitution pass may resolve. `inputs` is keyed by the
/// dotted name (`"git.branch"`), so a caller can insert a lazily computed
/// value without reconstructing a `VarRef`.
#[derive(Debug, Default, Clone)]
pub struct VarSet {
    pub user: BTreeMap<String, String>,
    pub inputs: BTreeMap<String, String>,
}

impl VarSet {
    fn get(&self, r: &VarRef) -> Option<&String> {
        match r {
            VarRef::User(name) => self.user.get(name),
            VarRef::Input(..) => self.inputs.get(&r.name()),
        }
    }

    /// Sorted, comma-separated names — the "(available: ...)" error tail.
    fn available(&self) -> String {
        let mut names: Vec<&str> = self.user.keys().map(String::as_str).collect();
        names.extend(self.inputs.keys().map(String::as_str));
        names.sort_unstable();
        names.join(", ")
    }
}

fn is_user_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('A'..='Z'))
        && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
}

fn is_input_part(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

/// `--vars KEY=VALUE` pairs. Names are arbitrary within the user charset;
/// `RPI_` is refused because that namespace only exists at runtime, so a
/// user value there could never be the one a container observes.
pub fn parse_vars(pairs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut vars = BTreeMap::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            anyhow::bail!("--vars expects KEY=VALUE, got '{pair}'");
        };
        if !is_user_name(key) {
            anyhow::bail!("--vars: variable name '{key}' must match ^[A-Z][A-Z0-9_]*$");
        }
        if key.starts_with("RPI_") {
            anyhow::bail!("--vars: the RPI_ prefix is reserved for runtime variables ('{key}')");
        }
        if vars.insert(key.to_string(), value.to_string()).is_some() {
            anyhow::bail!("--vars: duplicate variable '{key}'");
        }
    }
    Ok(vars)
}

/// Classifies one reference body, or explains why it is not a valid one.
fn classify(field: &str, name: &str) -> anyhow::Result<VarRef> {
    if name.starts_with("RPI_") {
        let hint = if name == "RPI_ENV_SLUG" {
            "; did you mean ${env.slug}?"
        } else {
            ""
        };
        anyhow::bail!("{field}: RPI_* variables exist only at runtime{hint}");
    }
    if is_user_name(name) {
        return Ok(VarRef::User(name.to_string()));
    }
    if let Some((ns, rest)) = name.split_once('.') {
        if is_input_part(ns) && is_input_part(rest) {
            if !NAMESPACES.contains(&ns) {
                anyhow::bail!(
                    "{field}: unknown namespace '{ns}' (available: {})",
                    NAMESPACES.join(", ")
                );
            }
            return Ok(VarRef::Input(ns.to_string(), rest.to_string()));
        }
    }
    anyhow::bail!(
        "{field}: invalid variable name '{name}' (use ${{NAME}} for a --vars variable or ${{ns.field}} for a resolver input)"
    )
}

/// One step of the tokenizer: what sits at `rest`.
enum Token<'a> {
    /// `$${` — emit a literal `${`; `rest` continues after it.
    Escape(&'a str),
    /// A reference body plus the remainder after its closing brace.
    Reference(&'a str, &'a str),
}

/// Finds the next `$${` or `${` in `rest`. Returns the literal text before
/// it and what it was, or `None` when the remainder holds neither.
fn next_token<'a>(field: &str, rest: &'a str) -> anyhow::Result<Option<(&'a str, Token<'a>)>> {
    let mut from = 0usize;
    while let Some(offset) = rest[from..].find('$') {
        let at = from + offset;
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix("${") {
            return Ok(Some((&rest[..at], Token::Escape(tail))));
        }
        if let Some(body) = after.strip_prefix('{') {
            let Some(end) = body.find('}') else {
                anyhow::bail!("{field}: unclosed ${{...}} in '{rest}'");
            };
            return Ok(Some((
                &rest[..at],
                Token::Reference(&body[..end], &body[end + 1..]),
            )));
        }
        // A `$` that starts neither form is ordinary text; keep scanning.
        from = at + 1;
    }
    Ok(None)
}

/// Every reference in `value`, in order of appearance. Needs no values, so
/// callers use it to decide which lazy inputs to compute before substituting.
pub fn refs(field: &str, value: &str) -> anyhow::Result<Vec<VarRef>> {
    let mut found = Vec::new();
    let mut rest = value;
    while let Some((_, token)) = next_token(field, rest)? {
        rest = match token {
            Token::Escape(tail) => tail,
            Token::Reference(name, tail) => {
                found.push(classify(field, name)?);
                tail
            }
        };
    }
    Ok(found)
}

/// Substitutes every reference and turns `$${` into a literal `${`.
pub fn substitute(field: &str, value: &str, vars: &VarSet) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some((literal, token)) = next_token(field, rest)? {
        out.push_str(literal);
        rest = match token {
            Token::Escape(tail) => {
                out.push_str("${");
                tail
            }
            Token::Reference(name, tail) => {
                let r = classify(field, name)?;
                let Some(v) = vars.get(&r) else {
                    anyhow::bail!(
                        "{field}: unknown variable '{}' (available: {})",
                        r.name(),
                        vars.available()
                    );
                };
                out.push_str(v);
                tail
            }
        };
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> VarSet {
        let mut v = VarSet::default();
        v.user.insert("BRANCH_NAME".into(), "feature/login".into());
        v.inputs.insert("git.branch".into(), "main".into());
        v.inputs.insert("env.slug".into(), "feature-login".into());
        v
    }

    #[test]
    fn parse_vars_accepts_arbitrary_names() {
        let vars = parse_vars(&["FOO=bar".into(), "BRANCH_NAME=x/y".into()]).unwrap();
        assert_eq!(vars["FOO"], "bar");
        assert_eq!(vars["BRANCH_NAME"], "x/y");
        assert!(parse_vars(&[]).unwrap().is_empty());
    }

    #[test]
    fn parse_vars_keeps_everything_after_the_first_equals() {
        let vars = parse_vars(&["DSN=postgres://u:p@h/db?a=b".into()]).unwrap();
        assert_eq!(vars["DSN"], "postgres://u:p@h/db?a=b");
    }

    #[test]
    fn parse_vars_allows_an_empty_value() {
        let vars = parse_vars(&["EMPTY=".into()]).unwrap();
        assert_eq!(vars["EMPTY"], "");
    }

    #[test]
    fn parse_vars_rejects_bad_names_rpi_prefix_and_duplicates() {
        for (bad, needle) in [
            ("NOEQUALS", "KEY=VALUE"),
            ("lower=x", "^[A-Z][A-Z0-9_]*$"),
            ("1BAD=x", "^[A-Z][A-Z0-9_]*$"),
            ("RPI_X=1", "reserved"),
        ] {
            let err = parse_vars(&[bad.to_string()]).unwrap_err().to_string();
            assert!(err.contains(needle), "{bad}: {err}");
        }
        let err = parse_vars(&["A=1".into(), "A=2".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn refs_lists_both_namespaces_in_order() {
        let found = refs("ingress.hostname", "${env.slug}.${BRANCH_NAME}.example.com").unwrap();
        assert_eq!(
            found,
            vec![
                VarRef::Input("env".into(), "slug".into()),
                VarRef::User("BRANCH_NAME".into()),
            ]
        );
        assert!(refs("source.repo", "git@github.com:a/b.git")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn substitutes_both_namespaces_and_repeated_references() {
        let out = substitute(
            "ingress.hostname",
            "${env.slug}.${env.slug}.${git.branch}.example.com",
            &set(),
        )
        .unwrap();
        assert_eq!(out, "feature-login.feature-login.main.example.com");
    }

    #[test]
    fn dollar_dollar_brace_escapes_to_a_literal_reference() {
        let out = substitute("commands.backup", "sh -c 'tar -C $${HOME} .'", &set()).unwrap();
        assert_eq!(out, "sh -c 'tar -C ${HOME} .'");
        // The escape is inert as a reference: refs() must not report HOME.
        assert!(refs("commands.backup", "sh -c 'tar -C $${HOME} .'")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn lone_dollars_are_literal() {
        for text in ["echo $$", "cost is $5", "$", "a$b"] {
            assert_eq!(substitute("commands.x", text, &set()).unwrap(), text);
            assert!(refs("commands.x", text).unwrap().is_empty());
        }
    }

    #[test]
    fn rpi_reference_names_the_runtime_rule() {
        let err = refs("ingress.hostname", "${RPI_ENV_SLUG}.example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("runtime"), "got: {err}");
        assert!(err.contains("${env.slug}"), "must suggest the fix: {err}");

        let err = refs("source.branch", "${RPI_BRANCH_NAME}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("runtime"), "got: {err}");
        assert!(
            !err.contains("${env.slug}"),
            "the slug hint is only for RPI_ENV_SLUG: {err}"
        );
    }

    #[test]
    fn unknown_namespace_lists_the_available_ones() {
        let err = refs("ingress.hostname", "${foo.bar}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown namespace 'foo'"), "got: {err}");
        assert!(err.contains("git, env"), "got: {err}");
    }

    #[test]
    fn malformed_names_and_unclosed_braces_are_errors() {
        for bad in ["${}", "${a.b.c}", "${Mixed.case}", "${git.}", "${.branch}"] {
            let err = refs("ingress.hostname", bad).unwrap_err().to_string();
            assert!(err.contains("ingress.hostname"), "{bad}: {err}");
        }
        let err = refs("source.branch", "${BRANCH_NAME")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unclosed"), "got: {err}");
    }

    #[test]
    fn substituting_an_absent_variable_lists_what_is_available() {
        let err = substitute("source.branch", "${NOPE}", &set())
            .unwrap_err()
            .to_string();
        assert!(err.contains("NOPE"), "got: {err}");
        assert!(err.contains("BRANCH_NAME"), "got: {err}");
        assert!(err.contains("git.branch"), "got: {err}");

        let err = substitute("ingress.hostname", "${env.name}", &set())
            .unwrap_err()
            .to_string();
        assert!(err.contains("env.name"), "got: {err}");
    }

    #[test]
    fn var_ref_name_round_trips_both_forms() {
        assert_eq!(VarRef::User("FOO".into()).name(), "FOO");
        assert_eq!(
            VarRef::Input("git".into(), "branch".into()).name(),
            "git.branch"
        );
    }
}
