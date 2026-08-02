use serde::Serialize;
use serde_json::Value;

/// Matrix entry for build targets
#[derive(Serialize, Clone)]
pub struct MatrixEntry {
    pub os: &'static str,
    pub target: &'static str,
    pub binary_name: &'static str,
    pub binary_path: &'static str,
    pub cross: &'static str,
}

#[derive(Clone)]
pub struct ReleaseMatrix(Vec<MatrixEntry>);

impl Default for ReleaseMatrix {
    /// Returns a vector of all build matrix entries
    fn default() -> Self {
        ReleaseMatrix(vec![
            MatrixEntry {
                os: "ubuntu-latest",
                target: "x86_64-unknown-linux-musl",
                binary_name: "furcode-x86_64-unknown-linux-musl",
                binary_path: "target/x86_64-unknown-linux-musl/release/furcode",
                cross: "true",
            },
            MatrixEntry {
                os: "ubuntu-latest",
                target: "aarch64-unknown-linux-musl",
                binary_name: "furcode-aarch64-unknown-linux-musl",
                binary_path: "target/aarch64-unknown-linux-musl/release/furcode",
                cross: "true",
            },
            MatrixEntry {
                os: "ubuntu-latest",
                target: "x86_64-unknown-linux-gnu",
                binary_name: "furcode-x86_64-unknown-linux-gnu",
                binary_path: "target/x86_64-unknown-linux-gnu/release/furcode",
                cross: "false",
            },
            MatrixEntry {
                os: "ubuntu-latest",
                target: "aarch64-unknown-linux-gnu",
                binary_name: "furcode-aarch64-unknown-linux-gnu",
                binary_path: "target/aarch64-unknown-linux-gnu/release/furcode",
                cross: "true",
            },
            MatrixEntry {
                os: "macos-latest",
                target: "x86_64-apple-darwin",
                binary_name: "furcode-x86_64-apple-darwin",
                binary_path: "target/x86_64-apple-darwin/release/furcode",
                cross: "false",
            },
            MatrixEntry {
                os: "macos-latest",
                target: "aarch64-apple-darwin",
                binary_name: "furcode-aarch64-apple-darwin",
                binary_path: "target/aarch64-apple-darwin/release/furcode",
                cross: "false",
            },
            MatrixEntry {
                os: "windows-latest",
                target: "x86_64-pc-windows-msvc",
                binary_name: "furcode-x86_64-pc-windows-msvc.exe",
                binary_path: "target/x86_64-pc-windows-msvc/release/furcode.exe",
                cross: "false",
            },
            MatrixEntry {
                os: "windows-latest",
                target: "aarch64-pc-windows-msvc",
                binary_name: "furcode-aarch64-pc-windows-msvc.exe",
                binary_path: "target/aarch64-pc-windows-msvc/release/furcode.exe",
                cross: "false",
            },
            MatrixEntry {
                os: "ubuntu-latest",
                target: "aarch64-linux-android",
                binary_name: "furcode-aarch64-linux-android",
                binary_path: "target/aarch64-linux-android/release/furcode",
                cross: "true",
            },
        ])
    }
}

impl ReleaseMatrix {
    pub fn entries(&self) -> Vec<MatrixEntry> {
        self.0.clone()
    }
}

impl From<ReleaseMatrix> for Value {
    fn from(value: ReleaseMatrix) -> Self {
        serde_json::json!({
            "include": value.entries()
        })
    }
}
