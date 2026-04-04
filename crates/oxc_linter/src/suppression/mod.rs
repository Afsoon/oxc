use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use oxc_diagnostics::{DiagnosticSender, DiagnosticService, OxcDiagnostic};
use rustc_hash::FxHashMap;

mod diff;
mod tracking;

pub use tracking::{
    DiagnosticCounts, Filename, RuleName, SuppressionFile, SuppressionFileState,
    SuppressionTracking,
};

pub use diff::DiffManager;

type StaticSuppressionMap = Arc<FxHashMap<Filename, FxHashMap<RuleName, DiagnosticCounts>>>;

type FileSuppressionsMap = FxHashMap<RuleName, DiagnosticCounts>;

/// Thread-safe accumulator for runtime suppression counts from both oxlint and tsgo passes.
#[derive(Debug, Default)]
pub struct RuntimeSuppressionMap {
    inner: std::sync::Mutex<FxHashMap<Filename, FileSuppressionsMap>>,
}

impl RuntimeSuppressionMap {
    /// Merge runtime counts for a file. Counts are additive across passes.
    pub fn merge_file(&self, filename: Filename, counts: FxHashMap<RuleName, DiagnosticCounts>) {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(filename).or_default();
        for (rule, diagnostic) in counts {
            entry.entry(rule).or_insert(DiagnosticCounts { count: 0 }).count += diagnostic.count;
        }
    }

    /// Mark a file as seen (even if it has no violations).
    pub fn mark_seen(&self, filename: Filename) {
        let mut map = self.inner.lock().unwrap();
        map.entry(filename).or_default();
    }

    /// Consume into the inner map.
    pub fn into_inner(self) -> FxHashMap<Filename, FileSuppressionsMap> {
        self.inner.into_inner().unwrap()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OxlintSuppressionFileAction {
    None,
    Updated,
    Exists,
    Created,
    Malformed(OxcDiagnostic),
    UnableToPerformFsOperation(OxcDiagnostic),
}

impl OxlintSuppressionFileAction {
    fn ignore(&self) -> bool {
        *self != OxlintSuppressionFileAction::Created
            && *self != OxlintSuppressionFileAction::Updated
            && *self != OxlintSuppressionFileAction::Exists
    }
}

#[derive(Debug)]
pub struct SuppressionManager {
    pub suppressions_by_file: Option<SuppressionTracking>,
    pub file_action: OxlintSuppressionFileAction,
    suppression_file_path: PathBuf,
    suppress_all: bool,
    prune_suppression: bool,
    //If the source of truth exists
    file_exists: bool,
}

impl SuppressionManager {
    pub fn load(cwd: &Path, file_path: &str, suppress_all: bool, prune_suppression: bool) -> Self {
        let suppression_file_path = cwd.join(file_path);
        let file_exists = suppression_file_path.exists();

        if !file_exists {
            let file_action = if suppress_all || prune_suppression {
                OxlintSuppressionFileAction::Created
            } else {
                OxlintSuppressionFileAction::None
            };

            let suppressions_by_file = if suppress_all || prune_suppression {
                Some(SuppressionTracking::default())
            } else {
                None
            };

            return Self {
                suppressions_by_file,
                file_action,
                suppression_file_path,
                suppress_all,
                prune_suppression,
                file_exists,
            };
        }

        match SuppressionTracking::from_file(&suppression_file_path, cwd) {
            Ok(suppression_file) => Self {
                suppressions_by_file: Some(suppression_file),
                file_action: OxlintSuppressionFileAction::Exists,
                suppression_file_path,
                suppress_all,
                prune_suppression,
                file_exists,
            },
            Err(err) => Self {
                suppressions_by_file: None,
                file_action: OxlintSuppressionFileAction::Malformed(err),
                suppression_file_path,
                suppress_all,
                prune_suppression,
                file_exists,
            },
        }
    }

    /// Build a shared `DiffManager` that both oxlint and tsgo passes can write into.
    pub fn build_diff(&self) -> Arc<DiffManager> {
        let diff_manager = DiffManager::new(
            self.concurrent_map(),
            self.file_exists,
            self.file_action.ignore(),
            self.suppress_all,
        );

        Arc::new(diff_manager)
    }

    /// Finalize: compute new suppression state from static file + merged runtime map,
    /// then either update the suppression file or report diagnostics.
    ///
    /// # Panics
    /// Panics if `DiffManager` has any outstanding references to it still.
    pub fn finalize(
        &mut self,
        diff_manager: Arc<DiffManager>,
        tx_error: &DiagnosticSender,
        cwd: &Path,
    ) -> Result<(), OxcDiagnostic> {
        let diff_manager = Arc::into_inner(diff_manager)
            .expect("DiffManager still has outstanding Arc references");
        let runtime_map = diff_manager.into_runtime_map().into_inner();

        let static_map = self.concurrent_map();

        if self.is_updating_file() {
            let new_map = if self.suppress_all {
                Self::compute_suppress(&static_map, &runtime_map)
            } else {
                Self::compute_prune(&static_map, &runtime_map, cwd)
            };
            self.suppressions_by_file = Some(SuppressionTracking::from_map(new_map));
            self.has_been_updated();
            self.write()
        } else {
            // Read-only mode: report diagnostics for any differences
            let errors = Self::compute_diagnostics(&static_map, &runtime_map);
            if !errors.is_empty() {
                let diagnostics =
                    DiagnosticService::wrap_diagnostics(cwd, Path::new(""), "", errors);
                tx_error.send(diagnostics).unwrap();
            }
            Ok(())
        }
    }

    /// Suppress mode: overwrite counts with runtime values for seen files.
    /// Unseen files keep their static values.
    fn compute_suppress(
        static_map: &StaticSuppressionMap,
        runtime_map: &FxHashMap<Filename, FileSuppressionsMap>,
    ) -> FxHashMap<Filename, FileSuppressionsMap> {
        let mut result: FxHashMap<Filename, FileSuppressionsMap> = static_map.as_ref().clone();

        for (filename, runtime_rules) in runtime_map {
            // For seen files, merge: keep static rules not in runtime, overwrite with runtime counts
            let entry = result.entry(filename.clone()).or_default();
            // Overwrite all runtime rule counts
            for (rule, runtime_count) in runtime_rules {
                entry.insert(rule.clone(), runtime_count.clone());
            }
        }

        result
    }

    /// Prune mode: for seen files, remove rules absent in runtime and decrease counts.
    /// Also remove entries for files that no longer exist on disk.
    fn compute_prune(
        static_map: &StaticSuppressionMap,
        runtime_map: &FxHashMap<Filename, FileSuppressionsMap>,
        cwd: &Path,
    ) -> FxHashMap<Filename, FileSuppressionsMap> {
        let mut result: FxHashMap<Filename, FileSuppressionsMap> = FxHashMap::default();

        for (filename, static_rules) in static_map.iter() {
            if let Some(runtime_rules) = runtime_map.get(filename) {
                // File was linted — only keep rules that still fire, with min count
                let mut file_rules = FileSuppressionsMap::default();
                for (rule, static_count) in static_rules {
                    if let Some(runtime_count) = runtime_rules.get(rule) {
                        let count = static_count.count.min(runtime_count.count);
                        if count > 0 {
                            file_rules.insert(rule.clone(), DiagnosticCounts { count });
                        }
                    }
                    // Rule not in runtime = pruned, don't include
                }
                if !file_rules.is_empty() {
                    result.insert(filename.clone(), file_rules);
                }
            } else {
                // File not linted this run — check if it still exists on disk
                let file_path = cwd.join(filename.to_string());
                if file_path.exists() {
                    result.insert(filename.clone(), static_rules.clone());
                }
                // File doesn't exist on disk — drop it (deleted file cleanup)
            }
        }

        result
    }

    /// Read-only mode: generate diagnostic messages for differences.
    fn compute_diagnostics(
        static_map: &StaticSuppressionMap,
        runtime_map: &FxHashMap<Filename, FileSuppressionsMap>,
    ) -> Vec<OxcDiagnostic> {
        let mut errors = vec![];

        for (filename, static_rules) in static_map.iter() {
            if let Some(runtime_rules) = runtime_map.get(filename) {
                for (rule, static_count) in static_rules {
                    if let Some(runtime_count) = runtime_rules.get(rule) {
                        if static_count.count > runtime_count.count {
                            errors.push(
                                OxcDiagnostic::error(format!(
                                    "The number of '{rule}' errors in {filename} decreased from {} to {}.",
                                    static_count.count, runtime_count.count
                                ))
                                .with_help("Update `oxlint-suppressions.json` file running `oxlint --prune-suppressions`"),
                            );
                        } else if static_count.count < runtime_count.count {
                            errors.push(
                                OxcDiagnostic::error(format!(
                                    "The number of '{rule}' errors in {filename} increased from {} to {}.",
                                    static_count.count, runtime_count.count
                                ))
                                .with_help("Update `oxlint-suppressions.json` file running `oxlint --suppress-all`"),
                            );
                        }
                    } else {
                        errors.push(
                            OxcDiagnostic::error(format!(
                                "The '{rule}' rule has been pruned from {filename}."
                            ))
                            .with_help("Update `oxlint-suppressions.json` file running `oxlint --prune-suppressions`"),
                        );
                    }
                }

                for (rule, runtime_count) in runtime_rules {
                    if !static_rules.contains_key(rule) {
                        let s = if runtime_count.count == 1 { "" } else { "s" };
                        errors.push(
                            OxcDiagnostic::error(format!(
                                "{} new '{rule}' error{s} appeared in {filename}.",
                                runtime_count.count
                            ))
                            .with_help("Update `oxlint-suppressions.json` file running `oxlint --suppress-all`"),
                        );
                    }
                }
            } else if runtime_map.contains_key(filename) {
                // File seen but empty runtime — all rules pruned
                for rule in static_rules.keys() {
                    errors.push(
                        OxcDiagnostic::error(format!(
                            "The '{rule}' rule has been pruned from {filename}."
                        ))
                        .with_help("Update `oxlint-suppressions.json` file running `oxlint --prune-suppressions`"),
                    );
                }
            }
        }

        // New files in runtime not in static
        for (filename, runtime_rules) in runtime_map {
            if !static_map.contains_key(filename) {
                for (rule, runtime_count) in runtime_rules {
                    let s = if runtime_count.count == 1 { "" } else { "s" };
                    errors.push(
                        OxcDiagnostic::error(format!(
                            "{} new '{rule}' error{s} appeared in {filename}.",
                            runtime_count.count
                        ))
                        .with_help("Update `oxlint-suppressions.json` file running `oxlint --suppress-all`"),
                    );
                }
            }
        }

        errors
    }

    fn has_been_updated(&mut self) {
        if self.file_action == OxlintSuppressionFileAction::Exists {
            self.file_action = OxlintSuppressionFileAction::Updated;
        }
    }

    fn concurrent_map(&self) -> StaticSuppressionMap {
        self.suppressions_by_file.as_ref().map(|f| Arc::clone(f.suppressions())).unwrap_or_default()
    }

    fn is_updating_file(&self) -> bool {
        self.suppress_all || self.prune_suppression
    }

    fn write(&self) -> Result<(), OxcDiagnostic> {
        let Some(file) = self.suppressions_by_file.as_ref() else {
            return Err(OxcDiagnostic::error(
                "You can't prune error messages if a bulk suppression file is malformed.",
            ));
        };

        file.save(&self.suppression_file_path)
    }
}
