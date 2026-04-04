use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use oxc_diagnostics::{DiagnosticSender, DiagnosticService, OxcDiagnostic};
use rustc_hash::FxHashMap;

mod diff;
mod tracking;

pub use tracking::{
    DiagnosticCounts, Filename, RuleName, SuppressionDiff, SuppressionFile, SuppressionFileState,
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

    /// Finalize: compute diff between static suppression file and merged runtime map,
    /// then either report errors or update the suppression file.
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
        let diffs = Self::compute_diff(&static_map, &runtime_map);

        if diffs.is_empty() {
            return Ok(());
        }

        if self.is_updating_file() {
            for diff in diffs {
                match &diff {
                    // Only add new/increased entries when suppress_all is set
                    SuppressionDiff::Appeared { .. } | SuppressionDiff::Increased { .. } => {
                        if !self.suppress_all {
                            continue;
                        }
                    }
                    // Prune/decrease always applies
                    SuppressionDiff::PrunedRuled { .. } | SuppressionDiff::Decreased { .. } => {}
                }
                self.update(diff);
            }
            self.has_been_updated();
            self.write()
        } else {
            // Report diffs as diagnostics
            let errors: Vec<OxcDiagnostic> = diffs.into_iter().map(Into::into).collect();
            let diagnostics = DiagnosticService::wrap_diagnostics(cwd, Path::new(""), "", errors);
            tx_error.send(diagnostics).unwrap();
            Ok(())
        }
    }

    fn compute_diff(
        static_map: &StaticSuppressionMap,
        runtime_map: &FxHashMap<Filename, FileSuppressionsMap>,
    ) -> Vec<SuppressionDiff> {
        let mut diffs = vec![];

        // Check all files in the static map
        for (filename, static_rules) in static_map.iter() {
            if let Some(runtime_rules) = runtime_map.get(filename) {
                // File exists in both — compare rules
                for (rule, static_count) in static_rules {
                    if let Some(runtime_count) = runtime_rules.get(rule) {
                        if static_count.count > runtime_count.count {
                            diffs.push(SuppressionDiff::Decreased {
                                file: filename.clone(),
                                rule: rule.clone(),
                                from: static_count.count,
                                to: runtime_count.count,
                            });
                        } else if static_count.count < runtime_count.count {
                            diffs.push(SuppressionDiff::Increased {
                                file: filename.clone(),
                                rule: rule.clone(),
                                from: static_count.count,
                                to: runtime_count.count,
                            });
                        }
                    } else {
                        // Rule in static but not in runtime — pruned
                        diffs.push(SuppressionDiff::PrunedRuled {
                            file: filename.clone(),
                            rule: rule.clone(),
                        });
                    }
                }

                // New rules in runtime not in static
                for (rule, runtime_count) in runtime_rules {
                    if !static_rules.contains_key(rule) {
                        diffs.push(SuppressionDiff::Appeared {
                            file: filename.clone(),
                            rule: rule.clone(),
                            count: runtime_count.count,
                        });
                    }
                }
            } else if runtime_map.contains_key(filename) {
                // File seen but empty runtime — all rules pruned
                for rule in static_rules.keys() {
                    diffs.push(SuppressionDiff::PrunedRuled {
                        file: filename.clone(),
                        rule: rule.clone(),
                    });
                }
            }
            // If the file is not in runtime_map at all, it wasn't linted this run — skip
        }

        // Files in runtime but not in static — all rules are new
        for (filename, runtime_rules) in runtime_map {
            if !static_map.contains_key(filename) {
                for (rule, runtime_count) in runtime_rules {
                    diffs.push(SuppressionDiff::Appeared {
                        file: filename.clone(),
                        rule: rule.clone(),
                        count: runtime_count.count,
                    });
                }
            }
        }

        diffs
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

    fn update(&mut self, diff: SuppressionDiff) {
        let Some(file) = self.suppressions_by_file.as_mut() else {
            return;
        };

        file.update(diff);
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
