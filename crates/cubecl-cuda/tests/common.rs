use std::{io::Write, num::NonZero, process::Command};

use cubecl_core::{
    prelude::{ArrayCompilationArg, TensorCompilationArg},
    Compiler, CubeDim, ExecutionMode, Kernel, KernelSettings, Runtime,
};
use cubecl_cuda::CudaRuntime;

pub fn settings() -> KernelSettings {
    KernelSettings::default().cube_dim(CubeDim::default())
}

#[allow(unused)]
pub fn tensor() -> TensorCompilationArg {
    TensorCompilationArg {
        inplace: None,
        vectorisation: NonZero::new(1),
    }
}

#[allow(unused)]
pub fn tensor_vec(vec: u8) -> TensorCompilationArg {
    TensorCompilationArg {
        inplace: None,
        vectorisation: NonZero::new(vec),
    }
}

#[allow(unused)]
pub fn array() -> ArrayCompilationArg {
    ArrayCompilationArg {
        inplace: None,
        vectorisation: NonZero::new(1),
    }
}

pub fn compile(kernel: impl Kernel) -> String {
    let kernel = <<CudaRuntime as Runtime>::Compiler as Compiler>::compile(
        kernel.define(),
        ExecutionMode::Checked,
    )
    .to_string();
    format_cpp_code(&kernel).unwrap()
}

/// Format C++ code, useful when debugging.
fn format_cpp_code(code: &str) -> Result<String, std::io::Error> {
    let mut child = Command::new("clang-format")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;

    {
        let stdin = child.stdin.as_mut().expect("Failed to open stdin");
        stdin.write_all(code.as_bytes())?;
    }

    let output = child.wait_with_output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "clang-format failed",
        ))
    }
}
