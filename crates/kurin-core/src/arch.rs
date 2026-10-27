//! Target CPU architecture for node artifacts.
//!
//! The single source of truth for arch names: the planner resolves one
//! `Arch` per node (discovered `uname -m` or `nodes.<n>.arch`), the
//! artifact store keys payloads by it, and the CLI accepts it in
//! `--arch`. `kurin-artifact` re-exports this type so existing imports
//! keep compiling.

use serde::{Deserialize, Serialize};

/// Target CPU architecture for node artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Arch {
    Amd64,
    Arm64,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }

    /// Parse an arch name (`amd64` / `arm64`), e.g. from a cache dir name
    /// or the `nodes.<n>.arch` config field.
    pub fn parse(name: &str) -> Option<Arch> {
        match name {
            "amd64" => Some(Arch::Amd64),
            "arm64" => Some(Arch::Arm64),
            _ => None,
        }
    }

    /// Map a kernel `uname -m` machine name to an artifact arch; this is
    /// what node discovery reports and what `std::env::consts::ARCH`
    /// yields on the control machine.
    pub fn from_uname(machine: &str) -> Option<Arch> {
        match machine {
            "x86_64" | "amd64" => Some(Arch::Amd64),
            "aarch64" | "arm64" => Some(Arch::Arm64),
            _ => None,
        }
    }

    /// The architecture of the machine kurin itself runs on — the only
    /// arch that is not discovered per node (it picks artifacts kurin
    /// executes locally, like the kubeadm used to resolve image pins).
    pub fn host() -> miette::Result<Arch> {
        Arch::from_uname(std::env::consts::ARCH).ok_or_else(|| {
            miette::miette!(
                "unsupported control-machine architecture {} (amd64/arm64 only)",
                std::env::consts::ARCH
            )
        })
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_round_trips_through_parse() {
        for arch in [Arch::Amd64, Arch::Arm64] {
            assert_eq!(Arch::parse(arch.as_str()), Some(arch));
        }
        assert_eq!(Arch::parse("riscv64"), None);
    }

    #[test]
    fn uname_names_map_to_artifact_arches() {
        assert_eq!(Arch::from_uname("x86_64"), Some(Arch::Amd64));
        assert_eq!(Arch::from_uname("aarch64"), Some(Arch::Arm64));
        // The Go-style names are accepted too.
        assert_eq!(Arch::from_uname("amd64"), Some(Arch::Amd64));
        assert_eq!(Arch::from_uname("arm64"), Some(Arch::Arm64));
        assert_eq!(Arch::from_uname("ppc64le"), None);
    }

    #[test]
    fn serde_uses_kebab_case_names() {
        assert_eq!(serde_json::to_string(&Arch::Amd64).unwrap(), "\"amd64\"");
        assert_eq!(serde_json::to_string(&Arch::Arm64).unwrap(), "\"arm64\"");
        assert_eq!(
            serde_json::from_str::<Arch>("\"arm64\"").unwrap(),
            Arch::Arm64
        );
    }
}
