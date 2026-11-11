//! `fastbrowse self-update`: name how this copy was installed and the command or link that upgrades it.
//!
//! It never replaces a running binary and makes no network call: each install channel has its own upgrade
//! command, and the channel is read from what the installer recorded rather than guessed from a path.

use std::path::{Path, PathBuf};

use crate::cli::Build;

const USAGE: &str = "usage: fastbrowse self-update\n\nPrint how this fastbrowse was installed and how to upgrade it.\n";
const RELEASES: &str = "https://github.com/agent-labs-dev/fastbrowse/releases/latest";
const PYPI: &str = "https://pypi.org/project/fastbrowse/";

/// The platform name the release archives and npm packages use for this build, as `binaries.yml` lists them.
const TARGET: Option<&str> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("darwin-arm64")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("darwin-x64")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("linux-x64")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("linux-arm64")
} else if cfg!(all(windows, target_arch = "x86_64")) {
    Some("win32-x64")
} else {
    None
};

/// What the Python package knows about the environment it was installed into, passed in by its `fastbrowse` command.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wheel {
    /// `sys.prefix`: the environment the package is installed in.
    pub prefix: PathBuf,
    /// The distribution's `INSTALLER` record, such as `uv` or `pip`, when the installer wrote one.
    pub installer: Option<String>,
    /// Whether the package is this repository's own source tree, as `fastbrowse.build` finds it.
    pub checkout: bool,
}

/// How this copy was installed, which decides the command that upgrades it.
#[derive(Debug, Clone, PartialEq)]
pub enum Install {
    Checkout,
    UvTool,
    Uvx,
    Uv(PathBuf),
    Pip(PathBuf),
    Wheel { installer: Option<String>, prefix: PathBuf },
    Npm,
    Release(PathBuf),
}

/// The channel the recorded facts name. `exe` is this executable and `uv_cache` the directory `uv cache dir` printed,
/// when uv started this process.
pub fn channel(wheel: Option<&Wheel>, exe: &Path, uv_cache: Option<&Path>) -> Install {
    let Some(wheel) = wheel else {
        // The npm platform package, named for this target, keeps its `package.json` at its root and the binary in
        // `fastbrowse/` under it.
        let manifest = exe.parent().and_then(Path::parent).map(|root| root.join("package.json"));
        let name = manifest
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|json| json.get("name").and_then(|name| name.as_str()).map(str::to_owned));
        return match (name, TARGET) {
            (Some(name), Some(target)) if name == format!("@fastbrowse/{target}") => Install::Npm,
            _ => Install::Release(exe.to_path_buf()),
        };
    };
    if wheel.checkout {
        return Install::Checkout;
    }
    // uv writes this receipt into every environment `uv tool install` makes.
    if wheel.prefix.join("uv-receipt.toml").is_file() {
        return Install::UvTool;
    }
    // uvx runs a tool from an environment it keeps in uv's cache, with no receipt.
    if let Some(cache) = uv_cache
        && canonical(&wheel.prefix).starts_with(canonical(cache))
    {
        return Install::Uvx;
    }
    match wheel.installer.as_deref() {
        Some("uv") => Install::Uv(wheel.prefix.clone()),
        Some("pip") => Install::Pip(wheel.prefix.clone()),
        installer => Install::Wheel { installer: installer.map(str::to_owned), prefix: wheel.prefix.clone() },
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Two lines: what this copy is, and the command or link that upgrades it.
pub fn advice(install: &Install) -> String {
    let version = format!("fastbrowse {}", env!("CARGO_PKG_VERSION"));
    let (what, how) = match install {
        Install::Checkout => {
            ("runs from a fastbrowse checkout".to_owned(), "git pull && uv sync --all-extras".to_owned())
        }
        Install::UvTool => ("was installed with uv tool install".to_owned(), "uv tool upgrade fastbrowse".to_owned()),
        Install::Uvx => ("was run by uvx from uv's cache".to_owned(), "uvx fastbrowse@latest".to_owned()),
        Install::Uv(prefix) => (
            format!("was installed by uv into {}", prefix.display()),
            "uv lock --upgrade-package fastbrowse && uv sync in its project, or uv pip install --upgrade fastbrowse \
             in an environment outside a project"
                .to_owned(),
        ),
        Install::Pip(prefix) => (
            format!("was installed by pip into {}", prefix.display()),
            "python -m pip install --upgrade fastbrowse, with that environment's python".to_owned(),
        ),
        Install::Wheel { installer, prefix } => (
            match installer {
                Some(installer) => format!("was installed by {installer} into {}", prefix.display()),
                None => format!("was installed into {} by an installer that left no record", prefix.display()),
            },
            format!("upgrade the fastbrowse package the way it was installed; the latest release is at {PYPI}"),
        ),
        Install::Npm => (
            "is the binary of an npm package".to_owned(),
            "npm install fastbrowse@latest in the project that depends on it".to_owned(),
        ),
        Install::Release(exe) => (
            format!("is a release binary at {}", exe.display()),
            match TARGET {
                Some(target) => format!(
                    "download fastbrowse-rust-{target}.tar.gz from {RELEASES} and put the fastbrowse in it in place \
                     of this one"
                ),
                None => format!("build this platform's binary from the latest release's source at {RELEASES}"),
            },
        ),
    };
    format!("{version} {what}.\nTo upgrade it: {how}\n")
}

/// Run `fastbrowse self-update` with `args`, the arguments after it: the text to print and the exit code.
pub fn command(args: &[String], build: &Build) -> (String, i32) {
    match args {
        [] => {}
        [flag] if flag == "-h" || flag == "--help" => return (USAGE.to_owned(), 0),
        _ => return (USAGE.to_owned(), 2),
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return (format!("fastbrowse self-update: cannot find this executable: {error}\n"), 1),
    };
    // Only a Python install can be uvx's, and uv names itself in `UV` for the processes it starts.
    let uv_cache = build
        .wheel
        .as_ref()
        .and(std::env::var_os("UV"))
        .and_then(|uv| std::process::Command::new(uv).args(["cache", "dir"]).output().ok())
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()));
    (advice(&channel(build.wheel.as_ref(), &exe, uv_cache.as_deref())), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fastbrowse-update-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wheel(prefix: &Path, installer: Option<&str>) -> Wheel {
        Wheel { prefix: prefix.to_path_buf(), installer: installer.map(str::to_owned), checkout: false }
    }

    #[test]
    fn test_update_names_each_wheel_channel_from_its_record() {
        let dir = scratch("wheel");
        let exe = dir.join("fastbrowse");
        let tool = dir.join("tools/fastbrowse");
        std::fs::create_dir_all(&tool).unwrap();
        std::fs::write(tool.join("uv-receipt.toml"), "[tool]\n").unwrap();
        assert_eq!(channel(Some(&wheel(&tool, Some("uv"))), &exe, None), Install::UvTool);
        let cache = dir.join("cache");
        let ephemeral = cache.join("archive-v0/abc");
        std::fs::create_dir_all(&ephemeral).unwrap();
        assert_eq!(channel(Some(&wheel(&ephemeral, Some("uv"))), &exe, Some(&cache)), Install::Uvx);
        let venv = dir.join(".venv");
        assert_eq!(channel(Some(&wheel(&venv, Some("uv"))), &exe, Some(&cache)), Install::Uv(venv.clone()));
        assert_eq!(channel(Some(&wheel(&venv, Some("pip"))), &exe, None), Install::Pip(venv.clone()));
        let none = channel(Some(&wheel(&venv, None)), &exe, None);
        assert_eq!(none, Install::Wheel { installer: None, prefix: venv.clone() });
        assert!(advice(&none).contains(PYPI));
        let checkout = Wheel { checkout: true, ..wheel(&tool, Some("uv")) };
        assert_eq!(channel(Some(&checkout), &exe, None), Install::Checkout);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_update_tells_the_npm_package_binary_from_a_release_binary() {
        let dir = scratch("binary");
        // The npm platform package's layout, as scripts/npm_platform_package.py writes it.
        let exe = dir.join("fastbrowse").join("fastbrowse");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        assert_eq!(channel(None, &exe, None), Install::Release(exe.clone()));
        if let Some(target) = TARGET {
            std::fs::write(dir.join("package.json"), format!(r#"{{"name": "@fastbrowse/{target}"}}"#)).unwrap();
            assert_eq!(channel(None, &exe, None), Install::Npm);
            assert!(advice(&Install::Release(exe.clone())).contains(&format!("fastbrowse-rust-{target}.tar.gz")));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_update_advice_names_each_channels_command() {
        for (install, command) in [
            (Install::Checkout, "git pull"),
            (Install::UvTool, "uv tool upgrade fastbrowse"),
            (Install::Uvx, "uvx fastbrowse@latest"),
            (Install::Uv(PathBuf::from("v")), "uv lock --upgrade-package fastbrowse"),
            (Install::Pip(PathBuf::from("v")), "python -m pip install --upgrade fastbrowse"),
            (Install::Npm, "npm install fastbrowse@latest"),
            (Install::Release(PathBuf::from("fastbrowse")), RELEASES),
        ] {
            assert!(advice(&install).contains(command), "{install:?}");
        }
    }

    #[test]
    fn test_update_refuses_arguments() {
        assert_eq!(command(&["now".into()], &Build::default()).1, 2);
        assert_eq!(command(&["--help".into()], &Build::default()), (USAGE.to_owned(), 0));
    }
}
