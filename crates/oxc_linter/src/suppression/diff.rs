use std::path::Path;

use rustc_hash::FxHashMap;

use crate::{
    Message,
    suppression::{
        DiagnosticCounts, Filename, RuleName, RuntimeSuppressionMap, StaticSuppressionMap,
        SuppressionFile, SuppressionFileState,
    },
};

pub struct DiffManager {
    tracking_map: StaticSuppressionMap,
    runtime_map: RuntimeSuppressionMap,
    suppress_all: bool,
    file_exists: bool,
    ignore_diff: bool,
}

impl DiffManager {
    pub fn new(
        tracking_map: StaticSuppressionMap,
        file_exists: bool,
        ignore_diff: bool,
        suppress_all: bool,
    ) -> Self {
        Self {
            tracking_map,
            runtime_map: RuntimeSuppressionMap::default(),
            suppress_all,
            file_exists,
            ignore_diff,
        }
    }

    /// Process messages for a file: filter suppressed diagnostics and accumulate runtime counts.
    /// Returns the filtered messages (only new/increased violations shown to the user).
    pub fn collect_file(
        &self,
        file_path: &Path,
        cwd: &Path,
        messages: Vec<Message>,
    ) -> Vec<Message> {
        if self.ignore_diff {
            return messages;
        }

        let Ok(file_path) = file_path.strip_prefix(cwd) else {
            return messages;
        };

        let filename = Filename::new(file_path);
        let suppression_data = self.tracking_map.get(&filename);
        let suppression_file =
            SuppressionFile::new(self.file_exists, self.suppress_all, suppression_data);

        let (filtered_diagnostics, runtime_counts) =
            Self::suppress_lint_diagnostics(&suppression_file, messages);

        if let Some(counts) = runtime_counts {
            self.runtime_map.merge_file(filename, counts);
        }

        filtered_diagnostics
    }

    /// Mark that a file was seen but produced no violations (e.g. all fixed).
    /// This ensures we track it as "empty" rather than "unseen".
    pub fn collect_empty_file(&self, file_path: &Path, cwd: &Path) {
        if self.ignore_diff {
            return;
        }

        let Ok(file_path) = file_path.strip_prefix(cwd) else {
            return;
        };

        let filename = Filename::new(file_path);
        self.runtime_map.mark_seen(filename);
    }

    pub fn skip(&self) -> bool {
        self.ignore_diff
    }

    /// Return the accumulated runtime map for final diff computation.
    pub fn into_runtime_map(self) -> RuntimeSuppressionMap {
        self.runtime_map
    }

    fn suppress_lint_diagnostics(
        suppression_file_state: &SuppressionFile<'_>,
        lint_diagnostics: Vec<Message>,
    ) -> (Vec<Message>, Option<FxHashMap<RuleName, DiagnosticCounts>>) {
        let build_suppression_map = |diagnostics: &Vec<Message>| {
            let mut suppression_tracking: FxHashMap<RuleName, DiagnosticCounts> =
                FxHashMap::default();
            for message in diagnostics {
                let Ok(key) = RuleName::try_from(message) else {
                    continue;
                };

                suppression_tracking.entry(key).or_insert(DiagnosticCounts { count: 0 }).count += 1;
            }

            suppression_tracking
        };

        match suppression_file_state.suppression_state() {
            SuppressionFileState::Ignored => (lint_diagnostics, None),
            SuppressionFileState::New => {
                let runtime_suppression_tracking = build_suppression_map(&lint_diagnostics);

                (lint_diagnostics, Some(runtime_suppression_tracking))
            }
            SuppressionFileState::Exists => {
                let runtime_suppression_tracking = build_suppression_map(&lint_diagnostics);

                let Some(recorded_violations) = suppression_file_state.suppression_data() else {
                    return (lint_diagnostics, Some(runtime_suppression_tracking));
                };

                let diagnostics_filtered = lint_diagnostics
                    .into_iter()
                    .filter(|message| {
                        let Ok(key) = RuleName::try_from(message) else {
                            return true;
                        };

                        let Some(count_file) = recorded_violations.get(&key) else {
                            return true;
                        };

                        let Some(count_runtime) = runtime_suppression_tracking.get(&key) else {
                            return false;
                        };

                        count_file.count < count_runtime.count
                    })
                    .collect();

                (diagnostics_filtered, Some(runtime_suppression_tracking))
            }
        }
    }
}
