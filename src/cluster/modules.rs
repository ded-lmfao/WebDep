use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleCatalogEntry {
    pub id: String,
    pub name: String,
    pub category: String,
    pub required: bool,
    pub description: String,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeModuleStatus {
    pub id: String,
    pub installed: bool,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub binary_path: String,
    #[serde(default)]
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleInstallResult {
    pub module_id: String,
    pub success: bool,
    pub already_installed: bool,
    pub message: String,
    #[serde(default)]
    pub output: String,
}

impl ModuleInstallResult {
    fn new(
        module_id: String,
        success: bool,
        already_installed: bool,
        message: impl Into<String>,
        output: String,
    ) -> Self {
        Self {
            module_id,
            success,
            already_installed,
            message: message.into(),
            output,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeModulesSnapshot {
    pub package_manager: String,
    pub modules: Vec<NodeModuleStatus>,
}

pub fn module_catalog() -> Vec<ModuleCatalogEntry> {
    // (id, name, category, required, description, languages)
    type Row = (&'static str, &'static str, &'static str, bool, &'static str, &'static [&'static str]);
    const ROWS: &[Row] = &[
        (
            "unrar",
            "UnRAR Extractor",
            "archive",
            true,
            "Required on every VPS to extract .rar transfer and backup archives (unrar / unar).",
            &["Archives", "App Shift", "Backups"],
        ),
        (
            "git",
            "Git CLI",
            "system",
            true,
            "Required to clone, fetch, and deploy GitHub repositories on the VPS.",
            &["Git Deployments"],
        ),
        (
            "node",
            "Node.js & npm",
            "language",
            true,
            "JavaScript & TypeScript runtime plus npm for web apps, APIs, and bots.",
            &["Node.js", "Next.js", "React/Vite", "NestJS"],
        ),
        (
            "python",
            "Python 3 + pip & venv",
            "language",
            true,
            "Python 3 runtime, pip package manager, and venv virtual environment support.",
            &["Python", "FastAPI", "Django", "Flask"],
        ),
        (
            "java",
            "Java JDK / JRE",
            "language",
            false,
            "OpenJDK runtime and compiler for Java backends, Lavalink, and JVM services.",
            &["Java", "Spring Boot", "Lavalink", "Kotlin"],
        ),
        (
            "go",
            "Go Toolchain",
            "language",
            false,
            "Go compiler and module toolchain for Go web servers and worker binaries.",
            &["Go", "Gin", "Fiber"],
        ),
        (
            "rust",
            "Rust (rustc & cargo)",
            "language",
            false,
            "Rust compiler and Cargo package manager for building native Rust services.",
            &["Rust", "Axum", "Actix"],
        ),
        (
            "php",
            "PHP & Composer",
            "language",
            false,
            "PHP CLI/FPM runtime for PHP sites, Laravel apps, and Composer workflows.",
            &["PHP", "Laravel"],
        ),
        (
            "ruby",
            "Ruby Runtime",
            "language",
            false,
            "Ruby interpreter and Bundler tooling for Ruby, Sinatra, and Rails apps.",
            &["Ruby", "Rails"],
        ),
        (
            "dotnet",
            ".NET SDK / Runtime",
            "language",
            false,
            ".NET CLI for running C#, F#, and ASP.NET Core applications.",
            &["C#", ".NET", "ASP.NET"],
        ),
    ];
    ROWS.iter()
        .map(|&(id, name, category, required, description, languages)| ModuleCatalogEntry {
            id: id.to_string(),
            name: name.to_string(),
            category: category.to_string(),
            required,
            description: description.to_string(),
            languages: languages.iter().map(|l| l.to_string()).collect(),
        })
        .collect()
}

fn find_command_path(bin: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Ok(out) = Command::new("where")
            .arg(bin)
            .stdin(Stdio::null())
            .output()
        {
            if out.status.success() {
                if let Some(first) = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                {
                    return Some(PathBuf::from(first));
                }
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        if let Ok(out) = Command::new("which")
            .arg(bin)
            .stdin(Stdio::null())
            .output()
        {
            if out.status.success() {
                if let Some(first) = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                {
                    return Some(PathBuf::from(first));
                }
            }
        }
        let mut search_dirs = vec![
            PathBuf::from("/usr/local/sbin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/usr/sbin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/bin"),
            PathBuf::from("/usr/local/go/bin"),
            PathBuf::from("/root/.cargo/bin"),
        ];
        if let Ok(home) = std::env::var("HOME") {
            let hp = PathBuf::from(home);
            search_dirs.push(hp.join(".cargo/bin"));
            search_dirs.push(hp.join(".local/bin"));
            search_dirs.push(hp.join(".dotnet"));
        }
        for dir in search_dirs {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}

fn run_version_line(bin_path: &PathBuf, args: &[&str]) -> Option<String> {
    let out = Command::new(bin_path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    combined
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|s| s.chars().take(72).collect())
}

fn first_nonempty_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(72)
        .collect()
}

pub fn detect_package_manager() -> String {
    #[cfg(windows)]
    {
        if find_command_path("winget").is_some() {
            "winget".to_string()
        } else {
            "manual".to_string()
        }
    }
    #[cfg(not(windows))]
    {
        for pm in ["apt-get", "dnf", "yum", "apk", "pacman"] {
            if find_command_path(pm).is_some() {
                return pm.to_string();
            }
        }
        "none".to_string()
    }
}

impl NodeModuleStatus {
    /// A module whose main binary was found at `path`.
    fn at(id: &str, installed: bool, path: &Path, version: String, details: String) -> Self {
        Self {
            id: id.to_string(),
            installed,
            version,
            binary_path: path.to_string_lossy().to_string(),
            details,
        }
    }

    /// A module that is not installed.
    fn missing(id: &str, details: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            installed: false,
            details: details.into(),
            ..Default::default()
        }
    }
}

fn check_single_module(id: &str) -> NodeModuleStatus {
    match id {
        "unrar" => {
            if let Some(p) = find_command_path("unrar") {
                let ver = run_version_line(&p, &["-idq"])
                    .or_else(|| run_version_line(&p, &[]))
                    .unwrap_or_else(|| "unrar installed".to_string());
                NodeModuleStatus::at(
                    id,
                    true,
                    &p,
                    if ver.is_empty() {
                        "unrar ready".to_string()
                    } else {
                        ver
                    },
                    "Using native unrar CLI".to_string(),
                )
            } else if let Some(p) = find_command_path("unar") {
                let ver = run_version_line(&p, &["--version"])
                    .unwrap_or_else(|| "unar installed".to_string());
                NodeModuleStatus::at(
                    id,
                    true,
                    &p,
                    ver,
                    "Using unar (.rar compatible)".to_string(),
                )
            } else {
                NodeModuleStatus {
                    id: id.to_string(),
                    installed: false,
                    version: String::new(),
                    binary_path: String::new(),
                    details: "Neither unrar nor unar found".to_string(),
                }
            }
        }
        "git" => match find_command_path("git") {
            Some(p) => NodeModuleStatus::at(
                id,
                true,
                &p,
                run_version_line(&p, &["--version"]).unwrap_or_default(),
                String::new(),
            ),
            None => NodeModuleStatus::missing(id, "git binary not found".to_string()),
        },
        "node" => {
            let node_p = find_command_path("node");
            let npm_p = find_command_path("npm");
            match (node_p, npm_p) {
                (Some(np), Some(npm)) => {
                    let nv = run_version_line(&np, &["--version"]).unwrap_or_default();
                    let npmv = run_version_line(&npm, &["--version"]).unwrap_or_default();
                    NodeModuleStatus::at(
                        id,
                        true,
                        &np,
                        nv,
                        if npmv.is_empty() {
                            String::new()
                        } else {
                            format!("npm {npmv}")
                        },
                    )
                }
                (Some(np), None) => NodeModuleStatus::at(
                    id,
                    false,
                    &np,
                    run_version_line(&np, &["--version"]).unwrap_or_default(),
                    "Node.js found, but npm is missing".to_string(),
                ),
                _ => NodeModuleStatus::missing(id, "node & npm not found".to_string()),
            }
        }
        "python" => {
            let py_p = find_command_path("python3").or_else(|| find_command_path("python"));
            if let Some(py) = py_p {
                let ver = run_version_line(&py, &["--version"]).unwrap_or_default();
                let has_pip = find_command_path("pip3").is_some()
                    || find_command_path("pip").is_some()
                    || Command::new(&py)
                        .args(["-m", "pip", "--version"])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                #[cfg(unix)]
                let has_venv = Command::new(&py)
                    .args(["-c", "import venv, ensurepip"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                #[cfg(windows)]
                let has_venv = true;

                let installed = has_pip && has_venv;
                let mut missing = Vec::new();
                if !has_pip {
                    missing.push("pip");
                }
                if !has_venv {
                    missing.push("venv");
                }
                NodeModuleStatus {
                    id: id.to_string(),
                    installed,
                    version: ver,
                    binary_path: py.to_string_lossy().to_string(),
                    details: if installed {
                        "pip & venv ready".to_string()
                    } else {
                        format!("Python installed, missing {}", missing.join(" & "))
                    },
                }
            } else {
                NodeModuleStatus::missing(id, "python3 / pip / venv not found".to_string())
            }
        }
        "java" => {
            if let Some(p) = find_command_path("java") {
                let ver = run_version_line(&p, &["-version"]).unwrap_or_default();
                let has_javac = find_command_path("javac").is_some();
                NodeModuleStatus::at(
                    id,
                    true,
                    &p,
                    ver,
                    if has_javac {
                        "JDK (java + javac)".to_string()
                    } else {
                        "JRE only (no javac)".to_string()
                    },
                )
            } else {
                NodeModuleStatus::missing(id, "java runtime not found".to_string())
            }
        }
        "go" => match find_command_path("go") {
            Some(p) => NodeModuleStatus::at(
                id,
                true,
                &p,
                run_version_line(&p, &["version"]).unwrap_or_default(),
                String::new(),
            ),
            None => NodeModuleStatus::missing(id, "go toolchain not found".to_string()),
        },
        "rust" => {
            let rustc_p = find_command_path("rustc");
            let cargo_p = find_command_path("cargo");
            match (rustc_p, cargo_p) {
                (Some(rp), Some(_)) => NodeModuleStatus::at(
                    id,
                    true,
                    &rp,
                    run_version_line(&rp, &["--version"]).unwrap_or_default(),
                    "rustc + cargo ready".to_string(),
                ),
                (Some(rp), None) => NodeModuleStatus::at(
                    id,
                    false,
                    &rp,
                    run_version_line(&rp, &["--version"]).unwrap_or_default(),
                    "rustc found, cargo missing".to_string(),
                ),
                _ => NodeModuleStatus::missing(id, "rustc & cargo not found".to_string()),
            }
        }
        "php" => match find_command_path("php") {
            Some(p) => {
                let ver = run_version_line(&p, &["-v"]).unwrap_or_default();
                let has_composer = find_command_path("composer").is_some();
                NodeModuleStatus::at(
                    id,
                    true,
                    &p,
                    first_nonempty_line(&ver),
                    if has_composer {
                        "php + composer ready".to_string()
                    } else {
                        "php ready (composer optional)".to_string()
                    },
                )
            }
            None => NodeModuleStatus::missing(id, "php runtime not found".to_string()),
        },
        "ruby" => match find_command_path("ruby") {
            Some(p) => NodeModuleStatus::at(
                id,
                true,
                &p,
                run_version_line(&p, &["-v"]).unwrap_or_default(),
                String::new(),
            ),
            None => NodeModuleStatus::missing(id, "ruby runtime not found".to_string()),
        },
        "dotnet" => match find_command_path("dotnet") {
            Some(p) => NodeModuleStatus::at(
                id,
                true,
                &p,
                run_version_line(&p, &["--version"]).unwrap_or_default(),
                ".NET CLI ready".to_string(),
            ),
            None => NodeModuleStatus::missing(id, "dotnet CLI not found".to_string()),
        },
        _ => NodeModuleStatus::missing(id, "Unknown module".to_string()),
    }
}

pub fn detect_node_modules_sync() -> NodeModulesSnapshot {
    let pm = detect_package_manager();
    let modules = module_catalog()
        .into_iter()
        .map(|entry| check_single_module(&entry.id))
        .collect();
    NodeModulesSnapshot {
        package_manager: pm,
        modules,
    }
}

#[cfg(unix)]
fn is_effective_root() -> bool {
    Command::new("id")
        .arg("-u")
        .stdin(Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

#[cfg(unix)]
fn run_pkg_cmd(
    use_sudo: bool,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, String> {
    let mut cmd = if use_sudo {
        let mut c = Command::new("sudo");
        c.arg("-n").arg(program);
        c
    } else {
        Command::new(program)
    };
    cmd.args(args).stdin(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("Failed executing '{program}': {e}"))?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let tail: Vec<&str> = combined.lines().rev().take(35).collect();
    let snippet: String = tail.into_iter().rev().collect::<Vec<&str>>().join("\n");
    if out.status.success() {
        Ok(snippet)
    } else {
        Err(if snippet.trim().is_empty() {
            format!("{program} exited with status {}", out.status)
        } else {
            snippet
        })
    }
}

#[cfg(unix)]
fn package_candidates(pm: &str, module_id: &str) -> Vec<Vec<&'static str>> {
    match (pm, module_id) {
        ("apt-get", "unrar") => vec![vec!["unrar"], vec!["unar"]],
        ("apt-get", "git") => vec![vec!["git"]],
        ("apt-get", "node") => vec![vec!["nodejs", "npm"]],
        ("apt-get", "python") => vec![vec!["python3", "python3-pip", "python3-venv"]],
        ("apt-get", "java") => vec![
            vec!["default-jdk"],
            vec!["default-jdk-headless"],
            vec!["openjdk-21-jdk-headless"],
            vec!["openjdk-17-jdk-headless"],
            vec!["default-jre-headless"],
        ],
        ("apt-get", "go") => vec![vec!["golang-go"]],
        ("apt-get", "rust") => vec![vec!["rustc", "cargo"]],
        ("apt-get", "php") => vec![vec!["php-cli", "php-fpm", "composer"], vec!["php-cli"]],
        ("apt-get", "ruby") => vec![vec!["ruby-full"], vec!["ruby"]],
        ("apt-get", "dotnet") => vec![vec!["dotnet-sdk-8.0"], vec!["dotnet8"]],

        ("dnf" | "yum", "unrar") => vec![vec!["unrar"], vec!["unar"]],
        ("dnf" | "yum", "git") => vec![vec!["git"]],
        ("dnf" | "yum", "node") => vec![vec!["nodejs", "npm"]],
        ("dnf" | "yum", "python") => vec![vec!["python3", "python3-pip"]],
        ("dnf" | "yum", "java") => vec![
            vec!["java-21-openjdk-devel"],
            vec!["java-17-openjdk-devel"],
            vec!["java-21-openjdk-headless"],
            vec!["java-17-openjdk-headless"],
        ],
        ("dnf" | "yum", "go") => vec![vec!["golang"]],
        ("dnf" | "yum", "rust") => vec![vec!["rust", "cargo"]],
        ("dnf" | "yum", "php") => vec![vec!["php-cli", "php-fpm"], vec!["php-cli"]],
        ("dnf" | "yum", "ruby") => vec![vec!["ruby", "rubygems"]],
        ("dnf" | "yum", "dotnet") => vec![vec!["dotnet-sdk-8.0"]],

        ("apk", "unrar") => vec![vec!["unrar"], vec!["unar"]],
        ("apk", "git") => vec![vec!["git"]],
        ("apk", "node") => vec![vec!["nodejs", "npm"]],
        ("apk", "python") => vec![vec!["python3", "py3-pip"]],
        ("apk", "java") => vec![
            vec!["openjdk21"],
            vec!["openjdk17"],
            vec!["openjdk21-jre"],
            vec!["openjdk17-jre"],
        ],
        ("apk", "go") => vec![vec!["go"]],
        ("apk", "rust") => vec![vec!["rust", "cargo"]],
        ("apk", "php") => vec![vec!["php", "composer"], vec!["php"]],
        ("apk", "ruby") => vec![vec!["ruby", "ruby-bundler"]],
        ("apk", "dotnet") => vec![vec!["dotnet8-sdk"]],

        ("pacman", "unrar") => vec![vec!["unrar"], vec!["unarchiver"]],
        ("pacman", "git") => vec![vec!["git"]],
        ("pacman", "node") => vec![vec!["nodejs", "npm"]],
        ("pacman", "python") => vec![vec!["python", "python-pip"]],
        ("pacman", "java") => vec![vec!["jdk-openjdk"]],
        ("pacman", "go") => vec![vec!["go"]],
        ("pacman", "rust") => vec![vec!["rust"]],
        ("pacman", "php") => vec![vec!["php", "composer"], vec!["php"]],
        ("pacman", "ruby") => vec![vec!["ruby"]],
        ("pacman", "dotnet") => vec![vec!["dotnet-sdk"]],
        _ => Vec::new(),
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum PkgOp {
    Install,
    Remove,
}

/// Installs or removes `pkgs` with the detected package manager (non-interactive flags).
#[cfg(unix)]
fn run_pkg_manager(use_sudo: bool, pm: &str, op: PkgOp, pkgs: &[&str]) -> Result<String, String> {
    const APT_INSTALL: &[&str] = &[
        "-o",
        "DPkg::Lock::Timeout=120",
        "-o",
        "Dpkg::Options::=--force-confdef",
        "-o",
        "Dpkg::Options::=--force-confold",
        "install",
        "-y",
        "--no-install-recommends",
    ];
    const APT_REMOVE: &[&str] = &["-o", "DPkg::Lock::Timeout=120", "remove", "-y"];
    let base: &[&str] = match (pm, op) {
        ("apt-get", PkgOp::Install) => APT_INSTALL,
        ("apt-get", PkgOp::Remove) => APT_REMOVE,
        ("dnf" | "yum", PkgOp::Install) => &["install", "-y"],
        ("dnf" | "yum", PkgOp::Remove) => &["remove", "-y"],
        ("apk", PkgOp::Install) => &["add", "--no-cache"],
        ("apk", PkgOp::Remove) => &["del"],
        ("pacman", PkgOp::Install) => &["-S", "--noconfirm", "--needed"],
        ("pacman", PkgOp::Remove) => &["-R", "--noconfirm"],
        _ => return Err("Unsupported package manager".to_string()),
    };
    let mut args = base.to_vec();
    args.extend_from_slice(pkgs);
    let envs: &[(&str, &str)] = if pm == "apt-get" {
        &[("DEBIAN_FRONTEND", "noninteractive")]
    } else {
        &[]
    };
    run_pkg_cmd(use_sudo, pm, &args, envs)
}

pub fn install_node_modules_sync(module_ids: &[String]) -> (Vec<ModuleInstallResult>, NodeModulesSnapshot) {
    let catalog = module_catalog();
    #[cfg_attr(not(unix), allow(unused_variables))]
    let pm = detect_package_manager();
    let mut results = Vec::new();

    #[cfg(not(unix))]
    {
        for raw_id in module_ids {
            let id = raw_id.trim().to_ascii_lowercase();
            if !catalog.iter().any(|c| c.id == id) {
                results.push(ModuleInstallResult::new(
                    id,
                    false,
                    false,
                    "Unknown module id".to_string(),
                    String::new(),
                ));
                continue;
            }
            let st = check_single_module(&id);
            if st.installed {
                results.push(ModuleInstallResult::new(
                    id,
                    true,
                    true,
                    format!("Already installed ({})", st.version),
                    String::new(),
                ));
            } else {
                results.push(ModuleInstallResult::new(
                    id,
                    false,
                    false,
                    "Automatic package installation is supported on Linux VPS nodes (apt/dnf/yum/apk/pacman); install manually on Windows dev hosts."
                        .to_string(),
                    String::new(),
                ));
            }
        }
        (results, detect_node_modules_sync())
    }

    #[cfg(unix)]
    {
        let use_sudo = !is_effective_root();
        let mut index_refreshed = false;

        for raw_id in module_ids {
            let id = raw_id.trim().to_ascii_lowercase();
            if !catalog.iter().any(|c| c.id == id) {
                results.push(ModuleInstallResult::new(
                    id,
                    false,
                    false,
                    "Unknown module id".to_string(),
                    String::new(),
                ));
                continue;
            }

            let current = check_single_module(&id);
            if current.installed {
                results.push(ModuleInstallResult::new(
                    id,
                    true,
                    true,
                    format!("Already installed ({})", current.version),
                    String::new(),
                ));
                continue;
            }

            if pm == "none" {
                results.push(ModuleInstallResult::new(
                    id,
                    false,
                    false,
                    "No supported Linux package manager found (apt-get/dnf/yum/apk/pacman)."
                        .to_string(),
                    String::new(),
                ));
                continue;
            }

            let mut update_warning = String::new();
            if !index_refreshed {
                index_refreshed = true;
                match pm.as_str() {
                    "apt-get" => {
                        if let Err(e) = run_pkg_cmd(
                            use_sudo,
                            "apt-get",
                            &["-o", "DPkg::Lock::Timeout=120", "update", "-y"],
                            &[("DEBIAN_FRONTEND", "noninteractive")],
                        ) {
                            update_warning = format!("[apt-get update warning]\n{e}\n");
                        }
                    }
                    "apk" => {
                        let _ = run_pkg_cmd(use_sudo, "apk", &["update"], &[]);
                    }
                    _ => {}
                }
            }

            let candidates = package_candidates(&pm, &id);
            if candidates.is_empty() {
                let message = format!("No package mapping for '{id}' on '{pm}'");
                results.push(ModuleInstallResult {
                    module_id: id,
                    success: false,
                    already_installed: false,
                    message,
                    output: update_warning,
                });
                continue;
            }

            let mut installed_ok = false;
            let mut last_out = String::new();
            for pkgs in candidates {
                let res = run_pkg_manager(use_sudo, &pm, PkgOp::Install, &pkgs);
                match res {
                    Ok(out) => {
                        last_out = out;
                        installed_ok = true;
                        break;
                    }
                    Err(err) => {
                        last_out = err;
                    }
                }
            }

            let verified = check_single_module(&id);
            let success = installed_ok && verified.installed;
            let msg = if success {
                format!("Installed successfully ({})", verified.version)
            } else if installed_ok {
                format!(
                    "Package command finished, but module verification reported: {}",
                    verified.details
                )
            } else {
                format!("Installation failed via {pm}")
            };

            let output = if !success && !update_warning.is_empty() {
                format!("{update_warning}\n{last_out}")
            } else {
                last_out
            };

            results.push(ModuleInstallResult {
                module_id: id,
                success,
                already_installed: false,
                message: msg,
                output,
            });
        }

        (results, detect_node_modules_sync())
    }
}

pub fn uninstall_node_modules_sync(module_ids: &[String]) -> (Vec<ModuleInstallResult>, NodeModulesSnapshot) {
    let catalog = module_catalog();
    #[cfg_attr(not(unix), allow(unused_variables))]
    let pm = detect_package_manager();
    let mut results = Vec::new();

    #[cfg(not(unix))]
    {
        for raw_id in module_ids {
            let id = raw_id.trim().to_ascii_lowercase();
            if !catalog.iter().any(|c| c.id == id) {
                results.push(ModuleInstallResult::new(
                    id,
                    false,
                    false,
                    "Unknown module id".to_string(),
                    String::new(),
                ));
                continue;
            }
            let st = check_single_module(&id);
            if !st.installed {
                results.push(ModuleInstallResult::new(
                    id,
                    true,
                    false,
                    "Not installed".to_string(),
                    String::new(),
                ));
            } else {
                results.push(ModuleInstallResult::new(
                    id,
                    false,
                    true,
                    "Automatic package uninstallation is supported on Linux VPS nodes (apt/dnf/yum/apk/pacman); uninstall manually on Windows dev hosts."
                        .to_string(),
                    String::new(),
                ));
            }
        }
        (results, detect_node_modules_sync())
    }

    #[cfg(unix)]
    {
        let use_sudo = !is_effective_root();

        for raw_id in module_ids {
            let id = raw_id.trim().to_ascii_lowercase();
            if !catalog.iter().any(|c| c.id == id) {
                results.push(ModuleInstallResult::new(
                    id,
                    false,
                    false,
                    "Unknown module id".to_string(),
                    String::new(),
                ));
                continue;
            }

            let current = check_single_module(&id);
            if !current.installed {
                results.push(ModuleInstallResult::new(
                    id,
                    true,
                    false,
                    "Already not installed".to_string(),
                    String::new(),
                ));
                continue;
            }

            if pm == "none" {
                results.push(ModuleInstallResult::new(
                    id,
                    false,
                    true,
                    "No supported Linux package manager found (apt-get/dnf/yum/apk/pacman)."
                        .to_string(),
                    String::new(),
                ));
                continue;
            }

            let candidates = package_candidates(&pm, &id);
            if candidates.is_empty() {
                results.push(ModuleInstallResult::new(
                    id.clone(),
                    false,
                    true,
                    format!("No package mapping for '{id}' on '{pm}'"),
                    String::new(),
                ));
                continue;
            }

            let mut outputs = Vec::new();
            let mut cmd_ok = false;
            for pkgs in candidates {
                let res = run_pkg_manager(use_sudo, &pm, PkgOp::Remove, &pkgs);
                match res {
                    Ok(out) => {
                        outputs.push(out);
                        cmd_ok = true;
                        if !check_single_module(&id).installed {
                            break;
                        }
                    }
                    Err(err) => {
                        outputs.push(err);
                    }
                }
            }

            let verified = check_single_module(&id);
            let success = !verified.installed;
            let msg = if success {
                "Uninstalled successfully".to_string()
            } else if cmd_ok {
                format!(
                    "Package removal ran, but module is still detected ({}). It may have been installed outside {pm}.",
                    verified.version
                )
            } else {
                format!("Uninstallation failed via {pm}")
            };

            results.push(ModuleInstallResult {
                module_id: id,
                success,
                already_installed: verified.installed,
                message: msg,
                output: outputs.join("\n"),
            });
        }

        (results, detect_node_modules_sync())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_catalog_marks_unrar_and_core_runners_required() {
        let cat = module_catalog();
        let unrar = cat.iter().find(|m| m.id == "unrar").expect("unrar entry");
        assert!(unrar.required);
        let python = cat.iter().find(|m| m.id == "python").expect("python entry");
        assert!(python.required);
        let node = cat.iter().find(|m| m.id == "node").expect("node entry");
        assert!(node.required);
        let java = cat.iter().find(|m| m.id == "java").expect("java entry");
        assert!(!java.required);
    }

    #[test]
    fn detect_node_modules_covers_every_catalog_entry() {
        let cat = module_catalog();
        let snap = detect_node_modules_sync();
        assert!(!snap.package_manager.is_empty());
        assert_eq!(snap.modules.len(), cat.len());
        for entry in cat {
            assert!(snap.modules.iter().any(|m| m.id == entry.id));
        }
    }

    #[test]
    fn install_rejects_unknown_module_ids() {
        let (res, _) = install_node_modules_sync(&["unknown;rm -rf /".to_string()]);
        assert_eq!(res.len(), 1);
        assert!(!res[0].success);
        assert!(res[0].message.contains("Unknown"));
    }

    #[test]
    fn uninstall_rejects_unknown_module_ids() {
        let (res, _) = uninstall_node_modules_sync(&["unknown;rm -rf /".to_string()]);
        assert_eq!(res.len(), 1);
        assert!(!res[0].success);
        assert!(res[0].message.contains("Unknown"));
    }
}
