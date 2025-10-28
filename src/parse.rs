use std::{collections::HashSet, io::BufRead, path::PathBuf};

use lazy_static::lazy_static;
use liberror::AnyError;
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{Level, instrument};
use valuable::Valuable;

lazy_static! {
    static ref RE_TARGET: Regex = Regex::new(r#"^(.+):$"#).expect("Failed to compile RE_TARGET");
    static ref RE_SOURCE: Regex = Regex::new(r#"^\t(.+)$"#).expect("Failed to compile RE_SOURCE");
}

#[derive(Debug, Clone, Valuable)]
pub struct Plan {
    pub target_path: PlanPath,
    pub sources: Vec<PlanPath>,
}

#[derive(Debug, Clone, Valuable)]
pub struct PlanPath {
    pub path: PathBuf,
    pub leaf: String,
}
impl PlanPath {
    pub fn new_relative_to(from: &str, relative_to: PathBuf) -> Self {
        let relative_path = format!(
            "{}{}{from}",
            relative_to.display(),
            std::path::MAIN_SEPARATOR
        );

        Self {
            path: PathBuf::from(relative_path),
            leaf: from.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Valuable, Error)]
pub enum ParseError {
    #[error("Failed to locate spec at \"{path}\": {inner_error}")]
    SpecNotFound { path: String, inner_error: AnyError },
    #[error("Failed to open spec at \"{path}\": {inner_error}")]
    Open { path: String, inner_error: AnyError },
    #[error("Failed to read line: {inner_error}")]
    ReadLine { inner_error: AnyError },
    #[error(
        "Somehow matched both source and target in \"{line}\": source=\"{src}\", target=\"{target}\""
    )]
    UnexpectedSourceAndTarget {
        line: String,
        src: String,
        target: String,
    },
    #[error("No sources defined for target \"{target_name}\"")]
    MissingSources { target_name: String },
    #[error("Unknown target for source file \"{source_name}\"")]
    MissingTarget { source_name: String },
    #[error("Validation failed")]
    Validation { errors: Vec<ValidationError> },
    #[error("Unable to parse line: \"{line}\"")]
    InvalidLine { line: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Valuable, Error)]
pub enum ValidationError {
    #[error("Duplicate source \"{source_name}\" for target \"{target_name}\"")]
    DuplicateSource {
        source_name: String,
        target_name: String,
    },
    #[error(
        "Failed to resolve source file \"{source_name}\" at \"{source_path}\" for target \"{target_name}\": {inner_error}"
    )]
    MissingSource {
        source_name: String,
        source_path: String,
        target_name: String,
        inner_error: AnyError,
    },
    #[error("Duplicate target \"{target_name}\"")]
    DuplicateTarget { target_name: String },
}

fn get_spec_reader(
    spec_path: PathBuf,
) -> Result<std::io::Lines<impl std::io::BufRead>, ParseError> {
    let spec_path_raw = spec_path.display().to_string();
    let spec_file = std::fs::OpenOptions::new()
        .read(true)
        .open(&spec_path)
        .map_err(|e| ParseError::Open {
            path: spec_path_raw.clone(),
            inner_error: e.into(),
        })
        .inspect(|_| tracing::trace!(path = spec_path_raw, "Sucessfully opened spec file"))
        .inspect_err(|e| tracing::error!(path = spec_path_raw, error =% e, error_context =? e,"Failed to open spec file"))?;

    let reader = std::io::BufReader::new(spec_file);
    Ok(reader.lines())
}

fn try_get_first_capture(line: &str, regex: &Regex) -> Result<Option<String>, ParseError> {
    if !regex.is_match(line) {
        return Ok(None);
    }

    let caps = regex.captures(line).expect("Should always have a capture");
    Ok(caps.get(1).map(|c| c.as_str().trim().to_string()))
}

#[instrument(level = Level::INFO)]
pub fn parse_spec(
    spec_path: PathBuf,
    target_dir: PathBuf,
    sources_dir: PathBuf,
) -> Result<Vec<Plan>, ParseError> {
    let spec_path_raw = spec_path.display().to_string();
    tracing::debug!(given_path = spec_path_raw, "Canonicalizing spec path");

    let spec_path = spec_path
        .canonicalize()
        .map_err(|e| ParseError::SpecNotFound {
            path: spec_path_raw,
            inner_error: e.into(),
        })?;

    tracing::debug!(
        canonicalized_path = &spec_path.display().to_string(),
        "Canonicalized spec path"
    );

    let mut plans = Vec::new();
    let mut plan: Option<Plan> = None;

    let reader = get_spec_reader(spec_path)?;

    for line in reader {
        let line = line.map_err(|e| ParseError::ReadLine {
            inner_error: e.into(),
        })?;

        let target_result = try_get_first_capture(&line, &RE_TARGET)?;
        let source_result = try_get_first_capture(&line, &RE_SOURCE)?;

        match (target_result, source_result) {
            (Some(target), None) => {
                if let Some(plan) = plan.take() {
                    if plan.sources.is_empty() {
                        tracing::warn!(
                            target = target,
                            line = line,
                            plan = plan.as_value(),
                            "Invalid spec - there are no sources defined for the currently active target"
                        );
                        return Err(ParseError::MissingSources {
                            target_name: plan.target_path.leaf.clone(),
                        });
                    }

                    tracing::debug!(
                        line = line,
                        plan = plan.as_value(),
                        push_reason = "target_no_source",
                        "Pushing completed plan"
                    );

                    plans.push(plan);
                }

                plan = Some(Plan {
                    target_path: PlanPath::new_relative_to(&target, target_dir.clone()),
                    sources: vec![],
                });
            }
            (None, Some(source)) => {
                let Some(plan) = plan.as_mut() else {
                    tracing::warn!(
                        source = source,
                        line = line,
                        plan = plan.as_value(),
                        "Invalid spec - We have encountered a source directive when not processing a target"
                    );
                    return Err(ParseError::MissingTarget {
                        source_name: source.to_string(),
                    });
                };

                let source_path = PlanPath::new_relative_to(&source, sources_dir.clone());

                tracing::debug!(
                    line = line,
                    plan = plan.as_value(),
                    source = source,
                    "Adding source"
                );

                plan.sources.push(source_path);
            }
            (Some(target), Some(source)) => {
                tracing::warn!(
                    source = source,
                    target = target,
                    line = line,
                    "Invalid spec - We have somehow matched both source and target, this is likely unreachable"
                );
                return Err(ParseError::UnexpectedSourceAndTarget {
                    line: line.to_string(),
                    src: source,
                    target,
                });
            }
            (None, None) => {
                // No match, with content
                if !line.trim().is_empty() {
                    tracing::warn!(line = line, "Invalid spec - Unrecognized line");
                    return Err(ParseError::InvalidLine {
                        line: line.to_string(),
                    });
                }

                if let Some(plan) = plan.take() {
                    tracing::debug!(
                        line = line,
                        plan = plan.as_value(),
                        push_reason = "empty_line",
                        "Pushing completed plan"
                    );
                    plans.push(plan)
                }
            }
        }
    }

    if let Some(plan) = plan.take() {
        tracing::debug!(
            plan = plan.as_value(),
            push_reason = "end_of_file",
            "Pushing completed plan"
        );
        plans.push(plan)
    }

    tracing::info!(plans = plans.as_value(), "Parsed {} targets", plans.len());

    tracing::info!(plans = plans.as_value(), "Validating targets");

    let mut validation_errors = vec![];

    let mut sources_set = HashSet::new();
    let mut targets_set = HashSet::new();
    for plan in plans.iter() {
        if targets_set.contains(&plan.target_path.leaf) {
            tracing::error!(
                target_name = plan.target_path.leaf,
                "Found duplicate target"
            );

            validation_errors.push(ValidationError::DuplicateTarget {
                target_name: plan.target_path.leaf.clone(),
            })
        } else {
            targets_set.insert(&plan.target_path.leaf);
        }

        sources_set.clear();
        sources_set.reserve(plan.sources.len());
        for source in plan.sources.iter() {
            if sources_set.contains(&source.leaf) {
                tracing::error!(
                    target_name = plan.target_path.leaf,
                    source_name = source.leaf,
                    "Found duplicate source"
                );
                validation_errors.push(ValidationError::DuplicateSource {
                    source_name: source.leaf.clone(),
                    target_name: plan.target_path.leaf.clone(),
                })
            } else {
                sources_set.insert(&source.leaf);
            }

            if let Err(e) = source.path.canonicalize() {
                tracing::error!(
                    target_name = plan.target_path.leaf,
                    source_name = source.leaf,
                    error =% e,
                    error_context =? e,
                    "Source file not found"
                );
                validation_errors.push(ValidationError::MissingSource {
                    source_name: source.leaf.clone(),
                    source_path: source.path.display().to_string(),
                    target_name: plan.target_path.leaf.clone(),
                    inner_error: e.into(),
                })
            }
        }
    }

    if !validation_errors.is_empty() {
        return Err(ParseError::Validation {
            errors: validation_errors,
        });
    }

    tracing::info!(
        plans = plans.as_value(),
        "Successfully validated {} targets",
        plans.len()
    );

    Ok(plans)
}
