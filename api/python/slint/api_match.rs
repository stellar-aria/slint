// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use pyo3::prelude::*;
use pyo3_stub_gen::{
    derive::gen_stub_pyclass, derive::gen_stub_pyclass_enum, derive::gen_stub_pymethods,
};

#[gen_stub_pyclass]
#[pyclass(name = "GeneratedAPI", unsendable)]
pub struct PyGeneratedAPI {
    module: i_slint_compiler::generator::python::PyModule,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyGeneratedAPI {
    #[new]
    fn new(json: &str) -> PyResult<Self> {
        let module = i_slint_compiler::generator::python::PyModule::load_from_json(json)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self { module })
    }

    #[staticmethod]
    fn compare_generated_vs_actual(generated: &Self, actual: &Self) -> PyResult<()> {
        Ok(())
    }
}

impl From<i_slint_compiler::generator::python::PyModule> for PyGeneratedAPI {
    fn from(module: i_slint_compiler::generator::python::PyModule) -> Self {
        Self { module }
    }
}
