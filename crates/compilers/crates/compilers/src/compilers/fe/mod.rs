use self::input::FeVersionedInput;
use super::{CompilationError, Compiler, CompilerOutput, CompilerSettings, Language};
use crate::{ProjectPathsConfig, SourceParser, compilers::CompilerInput};
use alloy_json_abi::JsonAbi;
use alloy_primitives::{Bytes, hex};
use foundry_compilers_artifacts::{
    Bytecode, BytecodeObject, Contract, Evm, LosslessMetadata, Severity, SourceFile,
    error::SourceLocation, output_selection::OutputSelection, sources::Source,
};
use foundry_compilers_core::error::{Result, SolcError};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

pub mod input;

/// File extensions that are recognized as Fe source files.
pub const FE_EXTENSIONS: &[&str] = &["fe"];

/// Fe language, used as [Compiler::Language] for the Fe compiler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FeLanguage;

impl serde::Serialize for FeLanguage {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("fe")
    }
}

impl<'de> serde::Deserialize<'de> for FeLanguage {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let res = String::deserialize(deserializer)?;
        if res == "fe" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!("Invalid Fe language: {res}")))
        }
    }
}

impl Language for FeLanguage {
    const FILE_EXTENSIONS: &'static [&'static str] = FE_EXTENSIONS;
}

impl fmt::Display for FeLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fe")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeOptimizationLevel {
    #[serde(rename = "0")]
    None,
    #[serde(rename = "1")]
    Fast,
    #[serde(rename = "2")]
    RuntimeGas,
    #[serde(rename = "s")]
    Size,
}

impl fmt::Display for FeOptimizationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "0"),
            Self::Fast => write!(f, "1"),
            Self::RuntimeGas => write!(f, "2"),
            Self::Size => write!(f, "s"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optimize: Option<FeOptimizationLevel>,
    #[serde(default)]
    pub output_selection: OutputSelection,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FeRestrictions;

impl super::restrictions::CompilerSettingsRestrictions for FeRestrictions {
    fn merge(self, _other: Self) -> Option<Self> {
        Some(self)
    }
}

impl CompilerSettings for FeSettings {
    type Restrictions = FeRestrictions;

    fn update_output_selection(&mut self, mut f: impl FnMut(&mut OutputSelection)) {
        f(&mut self.output_selection);
    }

    fn can_use_cached(&self, other: &Self) -> bool {
        self.optimize == other.optimize
            && self.output_selection.is_subset_of(&other.output_selection)
    }

    fn satisfies_restrictions(&self, _restrictions: &Self::Restrictions) -> bool {
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeCompilationError {
    pub message: String,
}

impl fmt::Display for FeCompilationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl CompilationError for FeCompilationError {
    fn is_warning(&self) -> bool {
        false
    }

    fn is_error(&self) -> bool {
        true
    }

    fn source_location(&self) -> Option<SourceLocation> {
        None
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn error_code(&self) -> Option<u64> {
        None
    }
}

#[derive(Clone, Debug, Default)]
pub struct FeParser {
    _inner: (),
}

impl SourceParser for FeParser {
    type ParsedSource = FeParsedSource;

    fn new(_config: &ProjectPathsConfig) -> Self {
        Self { _inner: () }
    }
}

#[derive(Clone, Debug)]
pub struct FeParsedSource {
    contract_names: Vec<String>,
}

impl super::ParsedSource for FeParsedSource {
    type Language = FeLanguage;

    fn parse(content: &str, _file: &Path) -> Result<Self> {
        let contract_names = parse_contract_names(content);
        Ok(Self { contract_names })
    }

    fn version_req(&self) -> Option<&semver::VersionReq> {
        None
    }

    fn contract_names(&self) -> &[String] {
        &self.contract_names
    }

    fn language(&self) -> Self::Language {
        FeLanguage
    }

    fn resolve_imports<C>(
        &self,
        _paths: &ProjectPathsConfig<C>,
        _include_paths: &mut BTreeSet<PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
}

fn parse_contract_names(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let line = line.strip_prefix("pub ").unwrap_or(line);
            let rest = line.strip_prefix("contract ")?;
            let name = rest
                .split(|c: char| !(c == '_' || c.is_ascii_alphanumeric()))
                .next()
                .unwrap_or_default();
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect()
}

/// Fe compiler. Wrapper around the `fe` binary.
#[derive(Clone, Debug)]
pub struct Fe {
    pub path: PathBuf,
    pub version: Version,
}

impl Fe {
    /// Creates a new instance of the Fe compiler.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let version = Self::version(path.clone())?;
        Ok(Self { path, version })
    }

    /// Convenience function for compiling all Fe sources under the given path.
    pub fn compile_source(
        &self,
        path: &Path,
    ) -> Result<CompilerOutput<FeCompilationError, Contract>> {
        let input = FeVersionedInput::build(
            Source::read_all_from(path, FE_EXTENSIONS)?,
            Default::default(),
            FeLanguage,
            self.version.clone(),
        );
        self.compile(&input)
    }

    fn compile_input(
        &self,
        input: &FeVersionedInput,
    ) -> Result<CompilerOutput<FeCompilationError, Contract>> {
        let temp = TempBuildDir::new()?;
        let out_root = temp.path.join("out");
        fs::create_dir_all(&out_root).map_err(|err| SolcError::io(err, &out_root))?;

        if let Some(output) = self.compile_project_ingot(input, &out_root)? {
            return Ok(output);
        }

        match self.compile_ingot(input, &temp.path, &out_root) {
            Ok(output) => Ok(output),
            Err(_) => self.compile_standalone(input, &temp.path),
        }
    }

    fn compile_project_ingot(
        &self,
        input: &FeVersionedInput,
        out_root: &Path,
    ) -> Result<Option<CompilerOutput<FeCompilationError, Contract>>> {
        let Some(root) = input.project_root.as_deref() else {
            return Ok(None);
        };
        if !root.join("fe.toml").exists() {
            return Ok(None);
        }

        let source_map = input
            .sources()
            .map(|(source_path, _)| (source_path.to_path_buf(), source_path.to_path_buf()))
            .collect();
        self.run_build(root, out_root, &input.settings, false)?;
        read_fe_artifacts(out_root, &source_map).map(Some)
    }

    fn compile_ingot(
        &self,
        input: &FeVersionedInput,
        root: &Path,
        out_root: &Path,
    ) -> Result<CompilerOutput<FeCompilationError, Contract>> {
        let mut source_map = BTreeMap::new();
        for (source_path, source) in input.sources() {
            let ingot_path = ingot_source_path(source_path);
            let source_file = root.join(&ingot_path);
            if let Some(parent) = source_file.parent() {
                fs::create_dir_all(parent).map_err(|err| SolcError::io(err, parent))?;
            }
            fs::write(&source_file, source.content.as_bytes())
                .map_err(|err| SolcError::io(err, &source_file))?;
            source_map.insert(ingot_path, source_path.to_path_buf());
        }

        let fe_toml = root.join("fe.toml");
        fs::write(
            &fe_toml,
            r#"[ingot]
name = "foundry_fe"
version = "0.1.0"
"#,
        )
        .map_err(|err| SolcError::io(err, &fe_toml))?;

        self.run_build(root, out_root, &input.settings, false)?;
        read_fe_artifacts(out_root, &source_map)
    }

    fn compile_standalone(
        &self,
        input: &FeVersionedInput,
        temp_path: &Path,
    ) -> Result<CompilerOutput<FeCompilationError, Contract>> {
        let source_root = temp_path.join("standalone");
        let out_root = temp_path.join("out-standalone");
        fs::create_dir_all(&source_root).map_err(|err| SolcError::io(err, &source_root))?;
        fs::create_dir_all(&out_root).map_err(|err| SolcError::io(err, &out_root))?;

        let mut output = CompilerOutput::default();
        for (source_path, source) in input.sources() {
            let source_file = source_root.join(source_path);
            if let Some(parent) = source_file.parent() {
                fs::create_dir_all(parent).map_err(|err| SolcError::io(err, parent))?;
            }
            fs::write(&source_file, source.content.as_bytes())
                .map_err(|err| SolcError::io(err, &source_file))?;

            let contract_out = out_root.join(sanitize_path_for_dir(source_path));
            fs::create_dir_all(&contract_out).map_err(|err| SolcError::io(err, &contract_out))?;
            self.run_build(&source_file, &contract_out, &input.settings, true)?;
            let source_map =
                BTreeMap::from([(source_path.to_path_buf(), source_path.to_path_buf())]);
            merge_output(&mut output, read_fe_artifacts(&contract_out, &source_map)?);
        }

        Ok(output)
    }

    fn run_build(
        &self,
        target: &Path,
        out_dir: &Path,
        settings: &FeSettings,
        standalone: bool,
    ) -> Result<()> {
        let mut cmd = Command::new(&self.path);
        cmd.arg("build")
            .arg("--emit")
            .arg("bytecode,runtime-bytecode,abi,metadata")
            .arg("--out-dir")
            .arg(out_dir)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        if standalone {
            cmd.arg("--standalone");
        }
        if let Some(optimize) = settings.optimize {
            cmd.arg("--optimize").arg(optimize.to_string());
        }
        cmd.arg(target);

        debug!(?cmd, "compiling Fe source");
        let output = cmd.output().map_err(self.map_io_err())?;
        debug!(%output.status, output.stderr = ?String::from_utf8_lossy(&output.stderr), "finished");

        if output.status.success() { Ok(()) } else { Err(SolcError::solc_output(&output)) }
    }

    /// Invokes `fe --version` and parses the output as a SemVer [`Version`].
    pub fn version(fe: impl Into<PathBuf>) -> Result<Version> {
        crate::cache_version(fe.into(), &[], |fe| {
            let mut cmd = Command::new(fe);
            cmd.arg("--version").stdin(Stdio::null()).stderr(Stdio::piped()).stdout(Stdio::piped());
            debug!(?cmd, "getting Fe version");
            let output = cmd.output().map_err(|e| SolcError::io(e, fe))?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let version = stdout
                    .split_whitespace()
                    .find_map(|part| Version::from_str(part).ok())
                    .ok_or_else(|| SolcError::msg("Version not found in Fe output"))?;
                Ok(version)
            } else {
                Err(SolcError::solc_output(&output))
            }
        })
    }

    fn map_io_err(&self) -> impl FnOnce(io::Error) -> SolcError + '_ {
        move |err| SolcError::io(err, &self.path)
    }
}

impl Compiler for Fe {
    type Settings = FeSettings;
    type CompilationError = FeCompilationError;
    type Parser = FeParser;
    type Input = FeVersionedInput;
    type Language = FeLanguage;
    type CompilerContract = Contract;

    fn compile(
        &self,
        input: &Self::Input,
    ) -> Result<CompilerOutput<Self::CompilationError, Self::CompilerContract>> {
        self.compile_input(input)
    }

    fn available_versions(&self, _language: &Self::Language) -> Vec<super::CompilerVersion> {
        vec![super::CompilerVersion::Installed(Version::new(
            self.version.major,
            self.version.minor,
            self.version.patch,
        ))]
    }
}

fn read_fe_artifacts(
    out_dir: &Path,
    source_map: &BTreeMap<PathBuf, PathBuf>,
) -> Result<CompilerOutput<FeCompilationError, Contract>> {
    let mut output = CompilerOutput::default();
    for entry in fs::read_dir(out_dir).map_err(|err| SolcError::io(err, out_dir))? {
        let entry = entry.map_err(|err| SolcError::io(err, out_dir))?;
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "bin") {
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".runtime.bin"))
        {
            continue;
        }

        let contract_name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| SolcError::msg(format!("invalid Fe artifact file {}", path.display())))?
            .to_string();
        let source_path = metadata_source_path(&contract_name, out_dir, source_map)?
            .unwrap_or_else(|| fallback_source_path(&contract_name, source_map));
        let contract = read_contract(&contract_name, out_dir)?;
        output.contracts.entry(source_path.clone()).or_default().insert(contract_name, contract);
        insert_source_file(&mut output, source_path);
    }

    Ok(output)
}

fn merge_output(
    output: &mut CompilerOutput<FeCompilationError, Contract>,
    other: CompilerOutput<FeCompilationError, Contract>,
) {
    for (source_path, contracts) in other.contracts {
        output.contracts.entry(source_path.clone()).or_default().extend(contracts);
        insert_source_file(output, source_path);
    }
}

fn insert_source_file(
    output: &mut CompilerOutput<FeCompilationError, Contract>,
    source_path: PathBuf,
) {
    let next_source_id = output.sources.len() as u32;
    output.sources.entry(source_path).or_insert(SourceFile { id: next_source_id, ast: None });
}

fn metadata_source_path(
    contract_name: &str,
    out_dir: &Path,
    source_map: &BTreeMap<PathBuf, PathBuf>,
) -> Result<Option<PathBuf>> {
    let path = out_dir.join(format!("{contract_name}.metadata.json"));
    if !path.exists() {
        return Ok(None);
    }
    let metadata = read_optional_json::<serde_json::Value>(&path)?.unwrap_or_default();
    let Some(targets) = metadata.get("settings").and_then(|settings| {
        settings.get("compilationTarget").and_then(serde_json::Value::as_object)
    }) else {
        return Ok(None);
    };
    for (compiled_path, compiled_contract) in targets {
        if compiled_contract.as_str() != Some(contract_name) {
            continue;
        }
        let compiled_path = PathBuf::from(compiled_path);
        if let Some(source_path) = source_map.get(&compiled_path) {
            return Ok(Some(source_path.clone()));
        }
        return Ok(Some(unmap_ingot_source_path(&compiled_path)));
    }
    Ok(None)
}

fn fallback_source_path(contract_name: &str, source_map: &BTreeMap<PathBuf, PathBuf>) -> PathBuf {
    let _ = contract_name;
    source_map.values().next().cloned().unwrap_or_else(|| PathBuf::from("src/lib.fe"))
}

fn ingot_source_path(path: &Path) -> PathBuf {
    let path = normalize_relative_path(path);
    if path.starts_with("src") { path } else { Path::new("src").join("__foundry__").join(path) }
}

fn unmap_ingot_source_path(path: &Path) -> PathBuf {
    let foundry_prefix = Path::new("src").join("__foundry__");
    path.strip_prefix(&foundry_prefix).unwrap_or(path).to_path_buf()
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.file_name().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("lib.fe"))
    } else {
        path.to_path_buf()
    }
}

fn read_contract(contract_name: &str, out_dir: &Path) -> Result<Contract> {
    let bin = read_bytecode(&out_dir.join(format!("{contract_name}.bin")))?;
    let runtime_bin =
        read_optional_bytecode(&out_dir.join(format!("{contract_name}.runtime.bin")))?;
    let abi = read_optional_json::<JsonAbi>(&out_dir.join(format!("{contract_name}.abi.json")))?;
    let metadata = read_optional_metadata(&out_dir.join(format!("{contract_name}.metadata.json")))?;

    Ok(Contract {
        abi,
        metadata,
        userdoc: Default::default(),
        devdoc: Default::default(),
        ir: None,
        storage_layout: Default::default(),
        transient_storage_layout: Default::default(),
        evm: Some(Evm {
            assembly: None,
            legacy_assembly: None,
            bytecode: Some(bin),
            deployed_bytecode: runtime_bin.map(Into::into),
            method_identifiers: Default::default(),
            gas_estimates: None,
        }),
        ewasm: None,
        ir_optimized: None,
        ir_optimized_ast: None,
    })
}

fn read_bytecode(path: &Path) -> Result<Bytecode> {
    let content = fs::read_to_string(path).map_err(|err| SolcError::io(err, path))?;
    let content = content.trim().strip_prefix("0x").unwrap_or(content.trim());
    let bytes = hex::decode(content)
        .map(Bytes::from)
        .map_err(|err| SolcError::msg(format!("failed to decode {}: {err}", path.display())))?;
    Ok(Bytecode {
        function_debug_data: Default::default(),
        object: BytecodeObject::Bytecode(bytes),
        opcodes: None,
        source_map: None,
        generated_sources: Default::default(),
        link_references: Default::default(),
    })
}

fn read_optional_bytecode(path: &Path) -> Result<Option<Bytecode>> {
    if path.exists() { read_bytecode(path).map(Some) } else { Ok(None) }
}

fn read_optional_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).map_err(|err| SolcError::io(err, path))?;
    Ok(Some(serde_json::from_str(&content)?))
}

fn read_optional_metadata(path: &Path) -> Result<Option<LosslessMetadata>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|err| SolcError::io(err, path))?;
    Ok(serde_json::from_value(serde_json::Value::String(raw)).ok())
}

fn sanitize_path_for_dir(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace(['/', '\\', ':'], "_"))
}

#[derive(Debug)]
struct TempBuildDir {
    path: PathBuf,
}

impl TempBuildDir {
    fn new() -> Result<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "foundry-fe-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).map_err(|err| SolcError::io(err, &path))?;
        Ok(Self { path })
    }
}

impl Drop for TempBuildDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Fe, FeLanguage, FeParsedSource, FeVersionedInput, TempBuildDir, ingot_source_path,
        metadata_source_path, parse_contract_names, unmap_ingot_source_path,
    };
    use crate::compilers::{Compiler, CompilerContract, CompilerInput, ParsedSource};
    use foundry_compilers_artifacts::sources::{Source, Sources};
    use semver::Version;
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
    };

    #[test]
    fn parses_contract_names() {
        assert_eq!(
            parse_contract_names("pub contract Counter {\ncontract Internal {}\n"),
            vec!["Counter", "Internal"]
        );
    }

    #[test]
    fn parsed_source_has_no_version_requirement() {
        let parsed =
            FeParsedSource::parse("pub contract Counter {}", Path::new("Counter.fe")).unwrap();
        assert!(parsed.version_req().is_none());
        assert_eq!(parsed.contract_names(), ["Counter"]);
    }

    #[test]
    fn maps_non_src_sources_into_ingot_src_tree() {
        assert_eq!(ingot_source_path(Path::new("src/lib.fe")), PathBuf::from("src/lib.fe"));
        assert_eq!(
            ingot_source_path(Path::new("test/CounterTest.fe")),
            PathBuf::from("src/__foundry__/test/CounterTest.fe")
        );
        assert_eq!(
            unmap_ingot_source_path(Path::new("src/__foundry__/test/CounterTest.fe")),
            PathBuf::from("test/CounterTest.fe")
        );
    }

    #[test]
    fn maps_metadata_compilation_target_back_to_original_source() {
        let temp = TempBuildDir::new().unwrap();
        let metadata_path = temp.path.join("CounterTest.metadata.json");
        fs::write(
            &metadata_path,
            r#"{"settings":{"compilationTarget":{"src/__foundry__/test/CounterTest.fe":"CounterTest"}}}"#,
        )
        .unwrap();

        let source_map = BTreeMap::from([(
            PathBuf::from("src/__foundry__/test/CounterTest.fe"),
            PathBuf::from("test/CounterTest.fe"),
        )]);

        assert_eq!(
            metadata_source_path("CounterTest", &temp.path, &source_map).unwrap(),
            Some(PathBuf::from("test/CounterTest.fe"))
        );
    }

    #[test]
    fn fe_input_remembers_project_root_after_stripping_source_paths() {
        let mut sources = Sources::new();
        sources.insert(PathBuf::from("/tmp/project/src/lib.fe"), Source::new(""));
        let mut input =
            FeVersionedInput::build(sources, Default::default(), FeLanguage, Version::new(1, 2, 3));

        input.set_project_root(Path::new("/tmp/project"));
        input.strip_prefix(Path::new("/tmp/project"));

        assert_eq!(input.project_root.as_deref(), Some(Path::new("/tmp/project")));
        assert!(input.sources.contains_key(Path::new("src/lib.fe")));
    }

    #[test]
    #[ignore = "requires the fe binary to be installed"]
    fn compiles_sample_with_installed_fe() {
        let Ok(fe) = Fe::new("fe") else {
            return;
        };
        let mut sources = Sources::new();
        sources.insert(
            PathBuf::from("src/lib.fe"),
            Source::new(
                r#"
use std::abi::sol

msg CounterMsg {
    #[selector = sol("get()")]
    Get -> u256,
}

struct CounterStore {
    value: u256,
}

pub contract Counter {
    mut store: CounterStore

    init() uses (mut store) {
        store.value = 0
    }

    recv CounterMsg {
        Get -> u256 uses (store) {
            store.value
        }
    }
}
"#,
            ),
        );
        let input =
            FeVersionedInput::build(sources, Default::default(), FeLanguage, fe.version.clone());
        let output = Compiler::compile(&fe, &input).unwrap();
        let contracts = output.contracts.get(Path::new("src/lib.fe")).unwrap();
        let contract = contracts.get("Counter").unwrap();
        assert!(contract.abi.is_some());
        assert!(contract.bin_ref().is_some());
        assert!(contract.bin_runtime_ref().is_some());
    }
}
