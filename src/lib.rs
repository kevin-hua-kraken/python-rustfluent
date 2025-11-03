use chrono::NaiveDate;
use fluent::FluentArgs;
use fluent_bundle::FluentResource;
use fluent_bundle::concurrent::FluentBundle;
use miette::{LabeledSpan, miette};
use pyo3::exceptions::{PyFileNotFoundError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDate, PyDict, PyInt, PyList, PyString};
use std::collections::{HashMap, HashSet};
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

/// Represents a validation error found during compile-time checking
#[pyclass]
#[derive(Clone)]
struct ValidationError {
    #[pyo3(get)]
    error_type: String,
    #[pyo3(get)]
    message: String,
    #[pyo3(get)]
    message_id: Option<String>,
    #[pyo3(get)]
    reference: Option<String>,
}

#[pymethods]
impl ValidationError {
    fn __repr__(&self) -> String {
        format!("ValidationError(type={:?}, message={:?}, message_id={:?})",
            self.error_type, self.message, self.message_id)
    }

    fn __str__(&self) -> String {
        if let Some(ref msg_id) = self.message_id {
            format!("{} in '{}': {}", self.error_type, msg_id, self.message)
        } else {
            format!("{}: {}", self.error_type, self.message)
        }
    }
}

/// Represents a format error during message formatting
#[pyclass]
#[derive(Clone)]
struct FormatError {
    #[pyo3(get)]
    error_type: String,
    #[pyo3(get)]
    message: String,
}

#[pymethods]
impl FormatError {
    fn __repr__(&self) -> String {
        format!("FormatError(type={:?}, message={:?})", self.error_type, self.message)
    }

    fn __str__(&self) -> String {
        format!("{}: {}", self.error_type, self.message)
    }
}

impl FormatError {
    fn from_fluent_error(error: &fluent_bundle::FluentError) -> Self {
        use fluent_bundle::FluentError as BundleFluentError;
        let error_type = match error {
            BundleFluentError::Overriding { .. } => "Overriding",
            BundleFluentError::ParserError(_) => "ParserError",
            BundleFluentError::ResolverError(_) => "ResolverError",
        };
        Self {
            error_type: error_type.to_string(),
            message: error.to_string(),
        }
    }
}

create_exception!(rustfluent, ParserError, pyo3::exceptions::PyException);

/// Helper function to check all references in a resource against the bundle
fn check_references(
    resource: &FluentResource,
    bundle: &FluentBundle<FluentResource>,
) -> Vec<ValidationError> {
    use fluent_syntax::ast;
    let mut errors = Vec::new();

    for entry in resource.entries() {
        match entry {
            ast::Entry::Message(msg) => {
                let msg_id = msg.id.name.to_string();
                check_pattern_references(bundle, &msg.value, &msg_id, &mut errors);
                for attr in &msg.attributes {
                    check_pattern_references(bundle, &Some(attr.value.clone()), &msg_id, &mut errors);
                }
            }
            ast::Entry::Term(term) => {
                let term_id = format!("-{}", term.id.name);
                check_pattern_references(bundle, &Some(term.value.clone()), &term_id, &mut errors);
                for attr in &term.attributes {
                    check_pattern_references(bundle, &Some(attr.value.clone()), &term_id, &mut errors);
                }
            }
            _ => {}
        }
    }

    errors
}

fn check_pattern_references(
    bundle: &FluentBundle<FluentResource>,
    pattern: &Option<fluent_syntax::ast::Pattern<&str>>,
    current_msg_id: &str,
    errors: &mut Vec<ValidationError>,
) {
    use fluent_syntax::ast;

    if let Some(pattern) = pattern {
        for element in &pattern.elements {
            if let ast::PatternElement::Placeable { expression } = element {
                check_expression_references(bundle, expression, current_msg_id, errors);
            }
        }
    }
}

fn check_expression_references(
    bundle: &FluentBundle<FluentResource>,
    expression: &fluent_syntax::ast::Expression<&str>,
    current_msg_id: &str,
    errors: &mut Vec<ValidationError>,
) {
    use fluent_syntax::ast;

    match expression {
        ast::Expression::Inline(inline) => {
            match inline {
                ast::InlineExpression::MessageReference { id, attribute } => {
                    if !bundle.has_message(id.name) {
                        errors.push(ValidationError {
                            error_type: "UnknownMessage".to_string(),
                            message: format!("Unknown message: {}", id.name),
                            message_id: Some(current_msg_id.to_string()),
                            reference: Some(id.name.to_string()),
                        });
                    } else if let Some(attr) = attribute {
                        if let Some(msg) = bundle.get_message(id.name) {
                            if msg.get_attribute(attr.name).is_none() {
                                errors.push(ValidationError {
                                    error_type: "UnknownAttribute".to_string(),
                                    message: format!("Unknown attribute: {}.{}", id.name, attr.name),
                                    message_id: Some(current_msg_id.to_string()),
                                    reference: Some(format!("{}.{}", id.name, attr.name)),
                                });
                            }
                        }
                    }
                }
                ast::InlineExpression::TermReference { id, attribute, .. } => {
                    let term_id = format!("-{}", id.name);
                    if !bundle.has_message(&term_id) {
                        errors.push(ValidationError {
                            error_type: "UnknownTerm".to_string(),
                            message: format!("Unknown term: -{}", id.name),
                            message_id: Some(current_msg_id.to_string()),
                            reference: Some(term_id),
                        });
                    } else if let Some(attr) = attribute {
                        if let Some(term) = bundle.get_message(&term_id) {
                            if term.get_attribute(attr.name).is_none() {
                                errors.push(ValidationError {
                                    error_type: "UnknownAttribute".to_string(),
                                    message: format!("Unknown attribute on term: -{}.{}", id.name, attr.name),
                                    message_id: Some(current_msg_id.to_string()),
                                    reference: Some(format!("-{}.{}", id.name, attr.name)),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        ast::Expression::Select { selector, variants } => {
            check_expression_references(bundle, &ast::Expression::Inline((*selector).clone()), current_msg_id, errors);
            for variant in variants {
                check_pattern_references(bundle, &Some(variant.value.clone()), current_msg_id, errors);
            }
        }
    }
}

/// Helper function to detect cycles in message references
fn detect_cycles(
    resource: &FluentResource,
) -> Vec<ValidationError> {
    use fluent_syntax::ast;
    use std::collections::{HashMap, HashSet};

    let mut errors = Vec::new();

    // Build a map of message IDs to their referenced IDs
    let mut message_refs: HashMap<String, Vec<String>> = HashMap::new();

    for entry in resource.entries() {
        match entry {
            ast::Entry::Message(msg) => {
                let msg_id = msg.id.name.to_string();
                let mut refs = Vec::new();
                collect_references(&msg.value, &mut refs);
                for attr in &msg.attributes {
                    collect_references(&Some(attr.value.clone()), &mut refs);
                }
                message_refs.insert(msg_id, refs);
            }
            ast::Entry::Term(term) => {
                let term_id = format!("-{}", term.id.name);
                let mut refs = Vec::new();
                collect_references(&Some(term.value.clone()), &mut refs);
                for attr in &term.attributes {
                    collect_references(&Some(attr.value.clone()), &mut refs);
                }
                message_refs.insert(term_id, refs);
            }
            _ => {}
        }
    }

    // Check each message for cycles using DFS
    for (msg_id, _) in &message_refs {
        let mut visited = HashSet::new();
        let mut path = Vec::new();
        if has_cycle(msg_id, &message_refs, &mut visited, &mut path) {
            errors.push(ValidationError {
                error_type: "CyclicReference".to_string(),
                message: format!("Cyclic reference detected: {}", path.join(" -> ")),
                message_id: Some(msg_id.clone()),
                reference: None,
            });
        }
    }

    errors
}

fn collect_references(
    pattern: &Option<fluent_syntax::ast::Pattern<&str>>,
    refs: &mut Vec<String>,
) {
    use fluent_syntax::ast;

    if let Some(pattern) = pattern {
        for element in &pattern.elements {
            if let ast::PatternElement::Placeable { expression } = element {
                collect_expression_references(expression, refs);
            }
        }
    }
}

fn collect_expression_references(
    expression: &fluent_syntax::ast::Expression<&str>,
    refs: &mut Vec<String>,
) {
    use fluent_syntax::ast;

    match expression {
        ast::Expression::Inline(inline) => {
            match inline {
                ast::InlineExpression::MessageReference { id, .. } => {
                    refs.push(id.name.to_string());
                }
                ast::InlineExpression::TermReference { id, .. } => {
                    refs.push(format!("-{}", id.name));
                }
                _ => {}
            }
        }
        ast::Expression::Select { selector, variants } => {
            collect_expression_references(&ast::Expression::Inline((*selector).clone()), refs);
            for variant in variants {
                collect_references(&Some(variant.value.clone()), refs);
            }
        }
    }
}

fn has_cycle(
    msg_id: &str,
    message_refs: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
    path: &mut Vec<String>,
) -> bool {
    if visited.contains(msg_id) {
        // Found a cycle - add the current message to show where cycle completes
        path.push(msg_id.to_string());
        return true;
    }

    visited.insert(msg_id.to_string());
    path.push(msg_id.to_string());

    // Check all referenced messages
    if let Some(refs) = message_refs.get(msg_id) {
        for ref_id in refs {
            if has_cycle(ref_id, message_refs, visited, path) {
                return true;
            }
        }
    }

    path.pop();
    visited.remove(msg_id);
    false
}

#[pymodule]
mod rustfluent {
    use super::*;

    #[pymodule_export]
    use super::ParserError;

    #[pymodule_export]
    use super::ParseErrorDetail;

    #[pymodule_export]
    use super::ValidationError;

    #[pymodule_export]
    use super::FormatError;

    #[pyclass]
    struct Bundle {
        bundle: FluentBundle<FluentResource>,
        compile_errors: Vec<ValidationError>,
    }

    #[pymethods]
    impl Bundle {
        #[new]
        #[pyo3(signature = (language, ftl_filenames, strict=false, validate_references=true))]
        fn new(language: &str, ftl_filenames: Vec<PathBuf>, strict: bool, validate_references: bool) -> PyResult<Self> {
            let langid: LanguageIdentifier = match language.parse() {
                Ok(langid) => langid,
                Err(_) => {
                    return Err(PyValueError::new_err(format!(
                        "Invalid language: '{language}'"
                    )));
                }
            };
            let mut bundle = FluentBundle::new_concurrent(vec![langid]);
            let mut all_errors = Vec::new();

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

                // Check for duplicates manually before adding
                // Need to detect duplicates both within this file and against existing bundle
                use fluent_syntax::ast;
                use std::collections::HashSet;
                let mut seen_in_file = HashSet::new();

                for entry in resource.entries() {
                    let (kind, id) = match entry {
                        ast::Entry::Message(msg) => ("message", msg.id.name),
                        ast::Entry::Term(term) => ("term", term.id.name),
                        _ => continue,
                    };

                    // Check if this message/term already exists in bundle or was seen in this file
                    let full_id = if kind == "term" {
                        format!("-{}", id)
                    } else {
                        id.to_string()
                    };

                    let exists_in_bundle = bundle.has_message(&full_id);
                    let exists_in_file = !seen_in_file.insert(full_id.clone());

                    if exists_in_bundle || exists_in_file {
                        let validation_err = ValidationError {
                            error_type: "DuplicateMessageId".to_string(),
                            message: format!("Duplicate {}: '{}'. Later definition will override.", kind, id),
                            message_id: Some(id.to_string()),
                            reference: None,
                        };

                        if strict {
                            // In strict mode, raise error immediately
                            return Err(PyValueError::new_err(validation_err.message.clone()));
                        }

                        all_errors.push(validation_err);
                    }
                }

                // Check references and cycles BEFORE adding if validation is enabled
                if validate_references {
                    // Check if references in this resource exist in current bundle
                    let ref_errors = check_references(&resource, &bundle);
                    if strict && !ref_errors.is_empty() {
                        return Err(PyValueError::new_err(format!(
                            "Found {} reference error(s) in {}",
                            ref_errors.len(),
                            file_path.display()
                        )));
                    }
                    all_errors.extend(ref_errors);

                    // Check for cycles within this resource
                    let cycle_errors = detect_cycles(&resource);
                    if strict && !cycle_errors.is_empty() {
                        return Err(PyValueError::new_err(format!(
                            "Found {} cyclic reference(s) in {}",
                            cycle_errors.len(),
                            file_path.display()
                        )));
                    }
                    all_errors.extend(cycle_errors);
                }

                // Add the resource (will override duplicates)
                bundle.add_resource_overriding(resource);
            }

            Ok(Self {
                bundle,
                compile_errors: all_errors,
            })
        }

        /// Get all compile-time errors found during bundle creation
        fn get_compile_errors(&self) -> Vec<ValidationError> {
            self.compile_errors.clone()
        }

        #[pyo3(signature = (identifier, variables=None, use_isolating=true, errors=None))]
        pub fn get_translation(
            &mut self,
            identifier: &str,
            variables: Option<&Bound<'_, PyDict>>,
            use_isolating: bool,
            errors: Option<&Bound<'_, PyList>>,
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
            let mut variable_errors = Vec::new();

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
                        // Collect error and fall back to displaying the variable key
                        variable_errors.push(FormatError {
                            error_type: "InvalidVariableType".to_string(),
                            message: format!(
                                "Variable '{}' has unsupported type, expected str/int/date. Using key as fallback.",
                                key
                            ),
                        });
                        let fallback_value = key.clone();
                        args.set(key, fallback_value);
                    }
                }
            }

            // Format the message and collect errors
            let mut format_errors = vec![];
            let value = self
                .bundle
                .format_pattern(pattern, Some(&args), &mut format_errors);

            // Convert and append all errors to the provided list
            if let Some(error_list) = errors {
                // Add variable type errors
                for var_err in variable_errors {
                    error_list.append(var_err).ok();
                }

                // Add format errors (cycles, unknown refs, etc.)
                for format_err in format_errors {
                    let py_error = FormatError::from_fluent_error(&format_err);
                    error_list.append(py_error).ok();
                }
            }

            Ok(value.to_string())
        }
    }
}
