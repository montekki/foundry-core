use super::{FeLanguage, FeSettings};
use crate::compilers::CompilerInput;
use foundry_compilers_artifacts::sources::{Source, Sources};
use semver::Version;
use serde::Serialize;
use std::path::PathBuf;
use std::{borrow::Cow, path::Path};

#[derive(Clone, Debug, Serialize)]
pub struct FeVersionedInput {
    pub sources: Sources,
    pub settings: FeSettings,
    #[serde(skip)]
    pub version: Version,
    #[serde(skip)]
    pub project_root: Option<PathBuf>,
}

impl CompilerInput for FeVersionedInput {
    type Settings = FeSettings;
    type Language = FeLanguage;

    fn build(
        sources: Sources,
        settings: Self::Settings,
        _language: Self::Language,
        version: Version,
    ) -> Self {
        Self { sources, settings, version, project_root: None }
    }

    fn compiler_name(&self) -> Cow<'static, str> {
        "Fe".into()
    }

    fn strip_prefix(&mut self, base: &Path) {
        self.sources = std::mem::take(&mut self.sources)
            .into_iter()
            .map(|(path, source)| {
                let path = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
                (path, source)
            })
            .collect();
    }

    fn set_project_root(&mut self, root: &Path) {
        self.project_root = Some(root.to_path_buf());
    }

    fn language(&self) -> Self::Language {
        FeLanguage
    }

    fn version(&self) -> &Version {
        &self.version
    }

    fn sources(&self) -> impl Iterator<Item = (&Path, &Source)> {
        self.sources.iter().map(|(path, source)| (path.as_path(), source))
    }
}
