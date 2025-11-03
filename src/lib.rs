use chrono::NaiveDate;
use fluent::FluentArgs;
use fluent_bundle::FluentResource;
use fluent_bundle::concurrent::FluentBundle;
use miette::{LabeledSpan, miette};
use pyo3::exceptions::{PyFileNotFoundError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDate, PyDict, PyInt, PyString};
use std::fs;
use std::path::PathBuf;
use unic_langid::LanguageIdentifier;

use pyo3::create_exception;

/// Helper function to convert byte position to line and column numbers
fn byte_pos_to_line_col(source: &str, byte_pos: usize) -> (usize, usize) {
    let relevant = &source[..byte_pos.min(source.len())];
    let line = relevant.chars().filter(|&c| c == '\n').count() + 1;
    let col = relevant.len() - relevant.rfind('\n').map_or(0, |pos| pos + 1) + 1;
    (line, col)
}

/// Represents a single parsing error with detailed location information
#[pyclass]
#[derive(Clone)]
struct ParseErrorDetail {
    /// Human-readable error message
    #[pyo3(get)]
    message: String,

    /// Line number where the error occurred (1-indexed)
    #[pyo3(get)]
    line: usize,

    /// Column number where the error occurred (1-indexed)
    #[pyo3(get)]
    column: usize,

    /// Byte position where the error starts (0-indexed)
    #[pyo3(get)]
    byte_start: usize,

    /// Byte position where the error ends (0-indexed)
    #[pyo3(get)]
    byte_end: usize,

    /// Optional file path where the error occurred
    #[pyo3(get)]
    filename: Option<String>,
}

#[pymethods]
impl ParseErrorDetail {
    fn __repr__(&self) -> String {
        format!(
            "ParseErrorDetail(message={:?}, line={}, column={}, byte_start={}, byte_end={})",
            self.message, self.line, self.column, self.byte_start, self.byte_end
        )
    }

    fn __str__(&self) -> String {
        if let Some(ref filename) = self.filename {
            format!(
                "{}:{}:{}: {}",
                filename, self.line, self.column, self.message
            )
        } else {
            format!("{}:{}: {}", self.line, self.column, self.message)
        }
    }
}

impl ParseErrorDetail {
    fn from_parser_error(
        error: fluent_syntax::parser::ParserError,
        source: &str,
        filename: Option<String>,
    ) -> Self {
        let (line, column) = byte_pos_to_line_col(source, error.pos.start);
        Self {
            message: error.kind.to_string(),
            line,
            column,
            byte_start: error.pos.start,
            byte_end: error.pos.end,
            filename,
        }
    }
}

create_exception!(rustfluent, ParserError, pyo3::exceptions::PyException);

#[pymodule]
mod rustfluent {
    use super::*;

    #[pymodule_export]
    use super::ParserError;

    #[pymodule_export]
    use super::ParseErrorDetail;

    #[pyclass]
    struct Bundle {
        bundle: FluentBundle<FluentResource>,
    }

    #[pymethods]
    impl Bundle {
        #[new]
        #[pyo3(signature = (language, ftl_filenames, strict=false))]
        fn new(language: &str, ftl_filenames: Vec<PathBuf>, strict: bool) -> PyResult<Self> {
            let langid: LanguageIdentifier = match language.parse() {
                Ok(langid) => langid,
                Err(_) => {
                    return Err(PyValueError::new_err(format!(
                        "Invalid language: '{language}'"
                    )));
                }
            };
            let mut bundle = FluentBundle::new_concurrent(vec![langid]);

            for file_path in ftl_filenames.iter() {
                let contents = fs::read_to_string(file_path)
                    .map_err(|_| PyFileNotFoundError::new_err(file_path.clone()))?;

                let resource = match FluentResource::try_new(contents) {
                    Ok(resource) => resource,
                    Err((resource, errors)) if strict => {
                        let source = resource.source();
                        let filename_str = file_path.to_string_lossy().to_string();

                        // Create structured error details for programmatic access
                        let error_details: Vec<ParseErrorDetail> = errors
                            .iter()
                            .map(|e| {
                                ParseErrorDetail::from_parser_error(
                                    e.clone(),
                                    source,
                                    Some(filename_str.clone()),
                                )
                            })
                            .collect();

                        // Create a nice formatted error message using miette
                        let mut labels = Vec::with_capacity(errors.len());
                        for error in errors {
                            labels.push(LabeledSpan::at(error.pos, format!("{}", error.kind)))
                        }
                        let miette_error = miette!(
                            labels = labels,
                            "Error when parsing {}",
                            file_path.to_string_lossy()
                        )
                        .with_source_code(source.to_string());

                        // Create the exception with the formatted message and attach error details
                        return Err(Python::with_gil(|py| {
                            let err = ParserError::new_err(format!("{miette_error:?}"));
                            // Attach structured error details to the exception for programmatic access
                            if let Ok(exc) = err
                                .value(py)
                                .downcast::<pyo3::exceptions::PyBaseException>()
                            {
                                let _ = exc.setattr("errors", error_details);
                            }
                            err
                        }));
                    }
                    Err((resource, _errors)) => resource,
                };
                bundle.add_resource_overriding(resource);
            }

            Ok(Self { bundle })
        }

        #[pyo3(signature = (identifier, variables=None, use_isolating=true))]
        pub fn get_translation(
            &mut self,
            identifier: &str,
            variables: Option<&Bound<'_, PyDict>>,
            use_isolating: bool,
        ) -> PyResult<String> {
            self.bundle.set_use_isolating(use_isolating);

            let get_message = |id: &str| {
                self.bundle
                    .get_message(id)
                    .ok_or_else(|| PyValueError::new_err(format!("{id} not found")))
            };

            let pattern = match identifier.split_once('.') {
                Some((message_id, attribute_id)) => get_message(message_id)?
                    .get_attribute(attribute_id)
                    .ok_or_else(|| {
                        PyValueError::new_err(format!(
                            "{identifier} - Attribute '{attribute_id}' not found on message '{message_id}'."
                        ))
                    })?
                    .value(),
                    // Note: attribute.value() returns &Pattern directly (not Option)
                    // because attributes always have values, unlike messages
                None => get_message(identifier)?
                    .value()
                    .ok_or_else(|| {
                        PyValueError::new_err(format!("{identifier} - Message has no value."))
                    })?
            };

            let mut args = FluentArgs::new();

            if let Some(variables) = variables {
                for (python_key, python_value) in variables {
                    // Make sure the variable key is a Python string,
                    // raising a TypeError if not.
                    if !python_key.is_instance_of::<PyString>() {
                        return Err(PyTypeError::new_err(format!(
                            "Variable key not a str, got {python_key}."
                        )));
                    }
                    let key = python_key.to_string();
                    // Set the variable value as a string or integer,
                    // raising a TypeError if not.
                    if python_value.is_instance_of::<PyString>() {
                        args.set(key, python_value.to_string());
                    } else if python_value.is_instance_of::<PyInt>()
                        && let Ok(int_value) = python_value.extract::<i32>()
                    {
                        args.set(key, int_value);
                    } else if python_value.is_instance_of::<PyDate>()
                        && let Ok(chrono_date) = python_value.extract::<NaiveDate>()
                    {
                        args.set(key, chrono_date.format("%Y-%m-%d").to_string());
                    } else {
                        // The variable value was of an unsupported type.
                        // Fall back to displaying the variable key as its value.
                        let fallback_value = key.clone();
                        args.set(key, fallback_value);
                    }
                }
            }

            let mut errors = vec![];
            let value = self
                .bundle
                .format_pattern(pattern, Some(&args), &mut errors);
            Ok(value.to_string())
        }
    }
}
