//! Unified CLI <-> agent compatibility (spec 2026-07-12).
//!
//! The single source of truth for which semantic features exist, which
//! capability string each one advertises in the `/v1/version` handshake,
//! and what happens when the other side lacks one.

use std::collections::BTreeSet;

/// What happens when a feature is missing on the other side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Error out with an update hint.
    Required,
    /// One-shot warning banner, then the caller takes its fallback path.
    // Forward contract (spec 2026-07-12): no current feature declares
    // Degradable yet; it exists so a registry line-change can adopt it.
    #[allow(dead_code)]
    Degradable,
    /// Skip quietly (spec 2026-07-10 mandates this for source-check).
    Silent,
}

/// A semantic feature of the CLI<->agent protocol. One feature may cover
/// several routes. A breaking change to a feature is a NEW variant with a
/// `name/2`-style capability string — never a mutation of the old one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    Secrets,
    Commands,
    SourceCheck,
    Stats,
    Environments,
    SecretModes,
    SecretGroups,
}

impl Feature {
    pub const ALL: &'static [Feature] = &[
        Feature::Secrets,
        Feature::Commands,
        Feature::SourceCheck,
        Feature::Stats,
        Feature::Environments,
        Feature::SecretModes,
        Feature::SecretGroups,
    ];

    /// The string this feature advertises in the `/v1/version` handshake.
    pub fn capability(self) -> &'static str {
        match self {
            Feature::Secrets => "secrets",
            Feature::Commands => "commands",
            Feature::SourceCheck => "source-check",
            Feature::Stats => "stats",
            Feature::Environments => "environments",
            Feature::SecretModes => "secret-modes",
            Feature::SecretGroups => "secret-groups",
        }
    }

    /// Human name used in user-facing messages.
    pub fn label(self) -> &'static str {
        match self {
            Feature::Secrets => "secrets",
            Feature::Commands => "container commands",
            Feature::SourceCheck => "deploy-key preflight",
            Feature::Stats => "stats",
            Feature::Environments => "environments",
            Feature::SecretModes => "secret file modes",
            Feature::SecretGroups => "secret groups",
        }
    }

    pub fn policy(self) -> Policy {
        match self {
            Feature::Secrets => Policy::Required,
            Feature::Commands => Policy::Required,
            Feature::SourceCheck => Policy::Silent,
            Feature::Stats => Policy::Required,
            Feature::Environments => Policy::Required,
            Feature::SecretModes => Policy::Required,
            Feature::SecretGroups => Policy::Required,
        }
    }

    /// First released agent version that serves this feature — powers the
    /// "requires agent >= X" hint.
    pub fn since(self) -> &'static str {
        match self {
            Feature::Secrets => "0.9.0",
            Feature::Commands => "0.9.0",
            Feature::SourceCheck => "0.18.0",
            Feature::Stats => "0.9.0",
            Feature::Environments => "0.24.0",
            Feature::SecretModes => "0.26.0",
            Feature::SecretGroups => "0.27.0",
        }
    }

    /// Everything THIS binary serves when it runs as the agent. Dropping a
    /// feature generation later means removing it from this list only.
    pub fn advertised() -> Vec<String> {
        Feature::ALL
            .iter()
            .map(|f| f.capability().to_string())
            .collect()
    }
}

/// FROZEN (spec 2026-07-12): capabilities of agents that predate the
/// `features` handshake field, keyed by the version that introduced them.
/// This table never grows — every feature shipped after the handshake is
/// advertised explicitly, so the table dies out with old agents.
const LEGACY_MATRIX: &[(&str, &str)] = &[
    ("secrets", "0.9.0"),
    ("commands", "0.9.0"),
    ("stats", "0.9.0"),
    ("source-check", "0.18.0"),
];

/// Capability set of a pre-handshake agent, inferred from its version.
/// An unparseable version yields the empty set: Required features then fail
/// with an update hint instead of guessing.
pub fn legacy_capabilities(agent_version: &str) -> BTreeSet<String> {
    LEGACY_MATRIX
        .iter()
        .filter(|(_, since)| version_at_least(agent_version, since))
        .map(|(cap, _)| (*cap).to_string())
        .collect()
}

/// Lenient x.y.z parser: tolerates a leading `v` and `-`/`+` suffixes.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn version_at_least(version: &str, floor: &str) -> bool {
    match (parse_version(version), parse_version(floor)) {
        (Some(a), Some(b)) => a >= b,
        _ => false,
    }
}

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Mutex;

use crate::proto::VersionInfo;

/// Version-less variant of the missing-feature error, for the transport
/// safety net (agent swapped between handshake and call, spec 2026-07-12).
pub fn feature_unavailable_error(feature: Feature) -> anyhow::Error {
    anyhow::anyhow!(
        "agent does not support {} (requires agent >= {}) - update the agent on the Pi",
        feature.label(),
        feature.since()
    )
}

/// Direction-aware hint for the doctor `version match` check: point the user
/// at whichever side is behind. Only the "agent newer" case flips to the CLI;
/// agent-older and unparseable/same-parse-but-different both fall back to the
/// agent hint, matching the generic branch of `emit_version_banners`.
pub fn version_skew_hint(cli_version: &str, agent_version: &str) -> &'static str {
    match (parse_version(agent_version), parse_version(cli_version)) {
        (Some(agent), Some(cli)) if agent > cli => "update the CLI: npm i -g rpi-deploy@latest",
        _ => "update the agent binary on the Pi",
    }
}

/// One agent connection's negotiated compatibility. Built once per command
/// by `cli::connect::connect_agent`; every availability check and every
/// version/compat notice goes through here.
pub struct CompatSession {
    cli_version: String,
    agent_version: String,
    agent_api: String,
    capabilities: BTreeSet<String>,
    /// Dedup: each banner key prints at most once per command run.
    warned: Mutex<HashSet<String>>,
    sink: Box<dyn Fn(&str) + Send + Sync>,
}

impl CompatSession {
    pub fn new(cli_version: &str, info: &VersionInfo) -> CompatSession {
        Self::with_sink(cli_version, info, Box::new(|m| crate::output::warn(m)))
    }

    pub fn with_sink(
        cli_version: &str,
        info: &VersionInfo,
        sink: Box<dyn Fn(&str) + Send + Sync>,
    ) -> CompatSession {
        let capabilities = match &info.features {
            Some(list) => list.iter().cloned().collect(),
            None => legacy_capabilities(&info.version),
        };
        CompatSession {
            cli_version: cli_version.to_string(),
            agent_version: info.version.clone(),
            agent_api: info.api.clone(),
            capabilities,
            warned: Mutex::new(HashSet::new()),
            sink,
        }
    }

    pub fn agent_version(&self) -> &str {
        &self.agent_version
    }

    pub fn agent_api(&self) -> &str {
        &self.agent_api
    }

    /// Raw capability membership — no side effects, no policy.
    pub fn supports(&self, feature: Feature) -> bool {
        self.capabilities.contains(feature.capability())
    }

    /// Apply the feature's declared policy. `Ok(true)` — go ahead;
    /// `Ok(false)` — take the fallback path (Degradable already warned once,
    /// Silent stayed quiet); `Err` — Required feature missing.
    pub fn gate(&self, feature: Feature) -> anyhow::Result<bool> {
        if self.supports(feature) {
            return Ok(true);
        }
        match feature.policy() {
            Policy::Required => Err(self.missing_error(feature)),
            Policy::Degradable => {
                self.warn_once(
                    feature.capability(),
                    &format!(
                        "{} is not available on agent v{} (requires agent >= {}) - skipping",
                        feature.label(),
                        self.agent_version,
                        feature.since()
                    ),
                );
                Ok(false)
            }
            Policy::Silent => Ok(false),
        }
    }

    /// Best supported generation, in preference order (newest first).
    // Forward contract (spec 2026-07-12): used once versioned feature
    // generations exist (e.g. pick(&[SecretsV2, Secrets])); unit-tested.
    #[allow(dead_code)]
    pub fn pick(&self, preference: &[Feature]) -> Option<Feature> {
        preference.iter().copied().find(|f| self.supports(*f))
    }

    /// Version-skew banners (§9.1 successor): agent older -> update agent,
    /// agent newer -> update CLI, unparseable-but-different -> generic.
    pub fn emit_version_banners(&self) {
        if self.agent_version == self.cli_version {
            return;
        }
        let msg = match (
            parse_version(&self.agent_version),
            parse_version(&self.cli_version),
        ) {
            (Some(agent), Some(cli)) => match agent.cmp(&cli) {
                Ordering::Less => format!(
                    "CLI v{} is newer than agent v{} - update the agent on the Pi",
                    self.cli_version, self.agent_version
                ),
                Ordering::Greater => format!(
                    "agent v{} is newer than CLI v{} - update the CLI: npm i -g rpi-deploy@latest",
                    self.agent_version, self.cli_version
                ),
                Ordering::Equal => format!(
                    "CLI v{} and agent v{} differ - update the agent on the Pi",
                    self.cli_version, self.agent_version
                ),
            },
            _ => format!(
                "CLI v{} and agent v{} differ - update the agent on the Pi",
                self.cli_version, self.agent_version
            ),
        };
        self.warn_once("version-skew", &msg);
    }

    fn missing_error(&self, feature: Feature) -> anyhow::Error {
        let agent_newer = matches!(
            (
                parse_version(&self.agent_version),
                parse_version(&self.cli_version),
            ),
            (Some(a), Some(c)) if a > c
        );
        if agent_newer {
            anyhow::anyhow!(
                "{} is not supported by agent v{}; CLI v{} is too old - update the CLI: npm i -g rpi-deploy@latest",
                feature.label(),
                self.agent_version,
                self.cli_version
            )
        } else {
            anyhow::anyhow!(
                "{} is not available on agent v{} (requires agent >= {}) - update the agent on the Pi",
                feature.label(),
                self.agent_version,
                feature.since()
            )
        }
    }

    fn warn_once(&self, key: &str, msg: &str) {
        if self.warned.lock().unwrap().insert(key.to_string()) {
            (self.sink)(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_strings_are_stable() {
        assert_eq!(Feature::Secrets.capability(), "secrets");
        assert_eq!(Feature::Commands.capability(), "commands");
        assert_eq!(Feature::SourceCheck.capability(), "source-check");
        assert_eq!(Feature::Stats.capability(), "stats");
        assert_eq!(Feature::Environments.capability(), "environments");
    }

    #[test]
    fn secret_modes_is_registered_and_required() {
        assert!(Feature::ALL.contains(&Feature::SecretModes));
        assert_eq!(Feature::SecretModes.capability(), "secret-modes");
        assert_eq!(Feature::SecretModes.since(), "0.26.0");
        assert!(matches!(Feature::SecretModes.policy(), Policy::Required));
    }

    #[test]
    fn secret_groups_feature_is_gated_from_0_27_0() {
        assert_eq!(Feature::SecretGroups.since(), "0.27.0");
        assert_eq!(Feature::SecretGroups.capability(), "secret-groups");
        assert!(Feature::ALL.contains(&Feature::SecretGroups));
    }

    #[test]
    fn advertised_covers_every_feature() {
        let adv = Feature::advertised();
        for f in Feature::ALL {
            assert!(
                adv.contains(&f.capability().to_string()),
                "{} missing",
                f.capability()
            );
        }
    }

    #[test]
    fn version_parsing_tolerates_prefix_and_suffix() {
        assert_eq!(parse_version("0.19.1"), Some((0, 19, 1)));
        assert_eq!(parse_version("v0.19.1"), Some((0, 19, 1)));
        assert_eq!(parse_version("1.2.3-dev+abc"), Some((1, 2, 3)));
        assert_eq!(parse_version("not-a-version"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn version_at_least_compares_numerically() {
        assert!(version_at_least("0.18.0", "0.18.0"));
        assert!(version_at_least("0.19.1", "0.18.0"));
        assert!(!version_at_least("0.17.1", "0.18.0"));
        // 0.10 > 0.9 numerically (string compare would get this wrong)
        assert!(version_at_least("0.10.0", "0.9.0"));
        // unparseable versions never satisfy a floor
        assert!(!version_at_least("garbage", "0.1.0"));
    }

    #[test]
    fn legacy_matrix_reconstructs_capabilities_by_version() {
        let caps = legacy_capabilities("0.17.1");
        assert!(caps.contains("secrets"));
        assert!(caps.contains("commands"));
        assert!(caps.contains("stats"));
        assert!(
            !caps.contains("source-check"),
            "source-check ships in 0.18.0"
        );

        let caps = legacy_capabilities("0.18.0");
        assert!(caps.contains("source-check"));

        assert!(legacy_capabilities("unknown").is_empty());
    }

    use crate::proto::VersionInfo;
    use std::sync::{Arc, Mutex};

    fn info(version: &str, features: Option<&[&str]>) -> VersionInfo {
        VersionInfo {
            version: version.to_string(),
            api: "v1".to_string(),
            features: features.map(|f| f.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn session(cli: &str, info: &VersionInfo) -> (CompatSession, Arc<Mutex<Vec<String>>>) {
        let warnings: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = warnings.clone();
        let s = CompatSession::with_sink(
            cli,
            info,
            Box::new(move |m| sink.lock().unwrap().push(m.to_string())),
        );
        (s, warnings)
    }

    #[test]
    fn handshake_features_win_over_legacy_matrix() {
        // An agent that explicitly advertises only `stats` supports exactly that,
        // whatever its version says.
        let (s, _) = session("0.20.0", &info("0.20.0", Some(&["stats"])));
        assert!(s.supports(Feature::Stats));
        assert!(!s.supports(Feature::Secrets));
    }

    #[test]
    fn legacy_agent_falls_back_to_version_matrix() {
        let (s, _) = session("0.20.0", &info("0.17.1", None));
        assert!(s.supports(Feature::Secrets));
        assert!(!s.supports(Feature::SourceCheck));
    }

    #[test]
    fn gate_required_missing_is_error_with_update_agent_hint() {
        let (s, warnings) = session("0.20.0", &info("0.17.1", Some(&[])));
        let err = s.gate(Feature::Secrets).unwrap_err().to_string();
        assert!(err.contains("secrets"), "{err}");
        assert!(err.contains(">= 0.9.0"), "{err}");
        assert!(err.contains("update the agent on the Pi"), "{err}");
        assert!(warnings.lock().unwrap().is_empty());
    }

    #[test]
    fn gate_required_missing_on_newer_agent_says_update_cli() {
        let (s, _) = session("0.20.0", &info("0.30.0", Some(&[])));
        let err = s.gate(Feature::Secrets).unwrap_err().to_string();
        assert!(err.contains("update the CLI"), "{err}");
        assert!(err.contains("npm i -g rpi-deploy@latest"), "{err}");
    }

    #[test]
    fn gate_silent_missing_is_quiet_false() {
        let (s, warnings) = session("0.20.0", &info("0.17.1", None));
        assert!(!s.gate(Feature::SourceCheck).unwrap());
        assert!(warnings.lock().unwrap().is_empty());
    }

    #[test]
    fn gate_available_is_true() {
        let (s, _) = session("0.20.0", &info("0.20.0", Some(&["secrets"])));
        assert!(s.gate(Feature::Secrets).unwrap());
    }

    #[test]
    fn pick_returns_first_supported_preference() {
        let (s, _) = session("0.20.0", &info("0.20.0", Some(&["secrets"])));
        assert_eq!(
            s.pick(&[Feature::Stats, Feature::Secrets]),
            Some(Feature::Secrets)
        );
        assert_eq!(s.pick(&[Feature::Stats]), None);
    }

    #[test]
    fn version_banner_older_agent_and_dedup() {
        let (s, warnings) = session("0.20.0", &info("0.17.1", None));
        s.emit_version_banners();
        s.emit_version_banners();
        let w = warnings.lock().unwrap();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("update the agent on the Pi"), "{}", w[0]);
    }

    #[test]
    fn version_banner_newer_agent_says_update_cli() {
        let (s, warnings) = session("0.20.0", &info("0.30.0", Some(&[])));
        s.emit_version_banners();
        let w = warnings.lock().unwrap();
        assert!(
            w[0].contains("update the CLI: npm i -g rpi-deploy@latest"),
            "{}",
            w[0]
        );
    }

    #[test]
    fn version_banner_equal_versions_is_silent() {
        let (s, warnings) = session("0.20.0", &info("0.20.0", Some(&[])));
        s.emit_version_banners();
        assert!(warnings.lock().unwrap().is_empty());
    }

    #[test]
    fn version_banner_same_parse_but_different_strings_warns_generically() {
        // "0.20.0-dirty" parses to the same (0,20,0) triple, but the strings
        // differ — warn generically rather than staying silent.
        let (s, warnings) = session("0.20.0", &info("0.20.0-dirty", Some(&[])));
        s.emit_version_banners();
        let w = warnings.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("differ"), "{}", w[0]);
    }

    #[test]
    fn version_banner_unparseable_agent_version_warns_generically() {
        let (s, warnings) = session("0.20.0", &info("garbage", Some(&[])));
        s.emit_version_banners();
        let w = warnings.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("differ"), "{}", w[0]);
    }

    #[test]
    fn version_skew_hint_points_at_the_stale_side() {
        // agent newer than CLI -> the CLI is stale
        assert_eq!(
            version_skew_hint("0.20.0", "0.20.1"),
            "update the CLI: npm i -g rpi-deploy@latest"
        );
        // agent older than CLI -> the agent is stale
        assert_eq!(
            version_skew_hint("0.20.1", "0.20.0"),
            "update the agent binary on the Pi"
        );
        // same parse but different strings -> generic (update the agent)
        assert_eq!(
            version_skew_hint("0.20.0", "0.20.0-dirty"),
            "update the agent binary on the Pi"
        );
        // unparseable agent version -> generic (update the agent)
        assert_eq!(
            version_skew_hint("0.20.0", "garbage"),
            "update the agent binary on the Pi"
        );
    }

    #[test]
    fn safety_net_error_matches_gate_wording() {
        let err = feature_unavailable_error(Feature::Commands).to_string();
        assert!(err.contains("container commands"), "{err}");
        assert!(err.contains(">= 0.9.0"), "{err}");
        assert!(err.contains("update the agent on the Pi"), "{err}");
    }
}
