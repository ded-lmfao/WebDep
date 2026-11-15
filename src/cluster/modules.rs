use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeModulesSnapshot {
    pub package_manager: String,
    pub modules: Vec<NodeModuleStatus>,
}

pub fn module_catalog() -> Vec<ModuleCatalogEntry> {
    vec![
        ModuleCatalogEntry {
            id: "unrar".to_string(),
            name: "UnRAR Extractor".to_string(),
            category: "archive".to_string(),
            required: true,
            description: "Required on every VPS to extract .rar transfer and backup archives (unrar / unar)."
                .to_string(),
            languages: vec![
                "Archives".to_string(),
                "App Shift".to_string(),
                "Backups".to_string(),
            ],
        },
        ModuleCatalogEntry {
            id: "git".to_string(),
            name: "Git CLI".to_string(),
            category: "system".to_string(),
            required: true,
            description: "Required to clone, fetch, and deploy GitHub repositories on the VPS."
                .to_string(),
            languages: vec!["Git Deployments".to_string()],
        },
        ModuleCatalogEntry {
            id: "node".to_string(),
            name: "Node.js & npm".to_string(),
            category: "language".to_string(),
            required: true,
            description: "JavaScript & TypeScript runtime plus npm for web apps, APIs, and bots."
                .to_string(),
            languages: vec![
                "Node.js".to_string(),
                "Next.js".to_string(),
                "React/Vite".to_string(),
                "NestJS".to_string(),
            ],
        },
        ModuleCatalogEntry {
            id: "python".to_string(),
            name: "Python 3 + pip & venv".to_string(),
            category: "language".to_string(),
            required: true,
            description: "Python 3 runtime, pip package manager, and venv virtual environment support."
                .to_string(),
            languages: vec![
                "Python".to_string(),
                "FastAPI".to_string(),
                "Django".to_string(),
                "Flask".to_string(),
            ],
        },
        ModuleCatalogEntry {
            id: "java".to_string(),
            name: "Java JDK / JRE".to_string(),
            category: "language".to_string(),
            required: false,
            description: "OpenJDK runtime and compiler for Java backends, Lavalink, and JVM services."
                .to_string(),
            languages: vec![
                "Java".to_string(),
                "Spring Boot".to_string(),
                "Lavalink".to_string(),
                "Kotlin".to_string(),
            ],
        },
        ModuleCatalogEntry {
            id: "go".to_string(),
            name: "Go Toolchain".to_string(),
            category: "language".to_string(),
            required: false,
            description: "Go compiler and module toolchain for Go web servers and worker binaries."
                .to_string(),
            languages: vec!["Go".to_string(), "Gin".to_string(), "Fiber".to_string()],
        },
        ModuleCatalogEntry {
            id: "rust".to_string(),
            name: "Rust (rustc & cargo)".to_string(),
            category: "language".to_string(),
            required: false,
            description: "Rust compiler and Cargo package manager for building native Rust services."
                .to_string(),
            languages: vec!["Rust".to_string(), "Axum".to_string(), "Actix".to_string()],
        },
        ModuleCatalogEntry {
            id: "php".to_string(),
            name: "PHP & Composer".to_string(),
            category: "language".to_string(),
            required: false,
            description: "PHP CLI/FPM runtime for PHP sites, Laravel apps, and Composer workflows."
                .to_string(),
            languages: vec!["PHP".to_string(), "Laravel".to_string()],
        },
        ModuleCatalogEntry {
            id: "ruby".to_string(),
            name: "Ruby Runtime".to_string(),
            category: "language".to_string(),
            required: false,
            description: "Ruby interpreter and Bundler tooling for Ruby, Sinatra, and Rails apps."
                .to_string(),
            languages: vec!["Ruby".to_string(), "Rails".to_string()],
        },
        ModuleCatalogEntry {
            id: "dotnet".to_string(),
            name: ".NET SDK / Runtime".to_string(),
            category: "language".to_string(),
            required: false,
            description: ".NET CLI for running C#, F#, and ASP.NET Core applications."
                .to_string(),
            languages: vec!["C#".to_string(), ".NET".to_string(), "ASP.NET".to_string()],
        },
    ]
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

fn check_single_module(id: &str) -> NodeModuleStatus {
    match id {
        "unrar" => {
            if let Some(p) = find_command_path("unrar") {
                let ver = run_version_line(&p, &["-idq"])
                    .or_else(|| run_version_line(&p, &[]))
                    .unwrap_or_else(|| "unrar installed".to_string());
                NodeModuleStatus {
                    id: id.to_string(),
                    installed: true,
                    version: if ver.is_empty() {
                        "unrar ready".to_string()
                    } else {
                        ver
                    },
                    binary_path: p.to_string_lossy().to_string(),
                    details: "Using native unrar CLI".to_string(),
                }
            } else if let Some(p) = find_command_path("unar") {
                let ver = run_version_line(&p, &["--version"])
                    .unwrap_or_else(|| "unar installed".to_string());
                NodeModuleStatus {
                    id: id.to_string(),
                    installed: true,
                    version: ver,
                    binary_path: p.to_string_lossy().to_string(),
                    details: "Using unar (.rar compatible)".to_string(),
                }
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
            Some(p) => NodeModuleStatus {
                id: id.to_string(),
                installed: true,
                version: run_version_line(&p, &["--version"]).unwrap_or_default(),
                binary_path: p.to_string_lossy().to_string(),
                details: String::new(),
            },
            None => NodeModuleStatus {
                id: id.to_string(),
                installed: false,
                details: "git binary not found".to_string(),
                ..Default::default()
            },
        },
        "node" => {
            let node_p = find_command_path("node");
            let npm_p = find_command_path("npm");
            match (node_p, npm_p) {
                (Some(np), Some(npm)) => {
                    let nv = run_version_line(&np, &["--version"]).unwrap_or_default();
                    let npmv = run_version_line(&npm, &["--version"]).unwrap_or_default();
                    NodeModuleStatus {
                        id: id.to_string(),
                        installed: true,
                        version: nv,
                        binary_path: np.to_string_lossy().to_string(),
                        details: if npmv.is_empty() {
                            String::new()
                        } else {
                            format!("npm {npmv}")
                        },
                    }
                }
                (Some(np), None) => NodeModuleStatus {
                    id: id.to_string(),
                    installed: false,
                    version: run_version_line(&np, &["--version"]).unwrap_or_default(),
                    binary_path: np.to_string_lossy().to_string(),
                    details: "Node.js found, but npm is missing".to_string(),
                },
                _ => NodeModuleStatus {
                    id: id.to_string(),
                    installed: false,
                    details: "node & npm not found".to_string(),
                    ..Default::default()
                },
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
                NodeModuleStatus {
                    id: id.to_string(),
                    installed: false,
                    details: "python3 / pip / venv not found".to_string(),
                    ..Default::default()
                }
            }
        }
        "java" => {
            if let Some(p) = find_command_path("java") {
                let ver = run_version_line(&p, &["-version"]).unwrap_or_default();
                let has_javac = find_command_path("javac").is_some();
                NodeModuleStatus {
                    id: id.to_string(),
                    installed: true,
                    version: ver,
                    binary_path: p.to_string_lossy().to_string(),
                    details: if has_javac {
                        "JDK (java + javac)".to_string()
                    } else {
                        "JRE only (no javac)".to_string()
                    },
                }
            } else {
                NodeModuleStatus {
                    id: id.to_string(),
                    installed: false,
                    details: "java runtime not found".to_string(),
                    ..Default::default()
                }
            }
        }
        "go" => match find_command_path("go") {
            Some(p) => NodeModuleStatus {
                id: id.to_string(),
                installed: true,
                version: run_version_line(&p, &["version"]).unwrap_or_default(),
                binary_path: p.to_string_lossy().to_string(),
                details: String::new(),
            },
            None => NodeModuleStatus {
                id: id.to_string(),
                installed: false,
                details: "go toolchain not found".to_string(),
                ..Default::default()
            },
        },
        "rust" => {
            let rustc_p = find_command_path("rustc");
            let cargo_p = find_command_path("cargo");
            match (rustc_p, cargo_p) {
                (Some(rp), Some(_)) => NodeModuleStatus {
                    id: id.to_string(),
                    installed: true,
                    version: run_version_line(&rp, &["--version"]).unwrap_or_default(),
                    binary_path: rp.to_string_lossy().to_string(),
                    details: "rustc + cargo ready".to_string(),
                },
                (Some(rp), None) => NodeModuleStatus {
                    id: id.to_string(),
                    installed: false,
                    version: run_version_line(&rp, &["--version"]).unwrap_or_default(),
                    binary_path: rp.to_string_lossy().to_string(),
                    details: "rustc found, cargo missing".to_string(),
                },
                _ => NodeModuleStatus {
                    id: id.to_string(),
                    installed: false,
                    details: "rustc & cargo not found".to_string(),
                    ..Default::default()
                },
            }
        }
        "php" => match find_command_path("php") {
            Some(p) => {
                let ver = run_version_line(&p, &["-v"]).unwrap_or_default();
                let has_composer = find_command_path("composer").is_some();
                NodeModuleStatus {
                    id: id.to_string(),
                    installed: true,
                    version: first_nonempty_line(&ver),
                    binary_path: p.to_string_lossy().to_string(),
                    details: if has_composer {
                        "php + composer ready".to_string()
                    } else {
                        "php ready (composer optional)".to_string()
                    },
                }
            }
            None => NodeModuleStatus {
                id: id.to_string(),
                installed: false,
                details: "php runtime not found".to_string(),
                ..Default::default()
            },
        },
        "ruby" => match find_command_path("ruby") {
            Some(p) => NodeModuleStatus {
                id: id.to_string(),
                installed: true,
                version: run_version_line(&p, &["-v"]).unwrap_or_default(),
                binary_path: p.to_string_lossy().to_string(),
                details: String::new(),
            },
            None => NodeModuleStatus {
                id: id.to_string(),
                installed: false,
                details: "ruby runtime not found".to_string(),
                ..Default::default()
            },
        },
        "dotnet" => match find_command_path("dotnet") {
            Some(p) => NodeModuleStatus {
                id: id.to_string(),
                installed: true,
                version: run_version_line(&p, &["--version"]).unwrap_or_default(),
                binary_path: p.to_string_lossy().to_string(),
                details: ".NET CLI ready".to_string(),
            },
            None => NodeModuleStatus {
                id: id.to_string(),
                installed: false,
                details: "dotnet CLI not found".to_string(),
                ..Default::default()
            },
        },
        _ => NodeModuleStatus {
            id: id.to_string(),
            installed: false,
            details: "Unknown module".to_string(),
            ..Default::default()
        },
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
        ("apt-get", "java") => vec![vec!["default-jdk"]],
        ("apt-get", "go") => vec![vec!["golang-go"]],
        ("apt-get", "rust") => vec![vec!["rustc", "cargo"]],
        ("apt-get", "php") => vec![vec!["php-cli", "php-fpm", "composer"], vec!["php-cli"]],
        ("apt-get", "ruby") => vec![vec!["ruby-full"], vec!["ruby"]],
        ("apt-get", "dotnet") => vec![vec!["dotnet-sdk-8.0"], vec!["dotnet8"]],

        ("dnf" | "yum", "unrar") => vec![vec!["unrar"], vec!["unar"]],
        ("dnf" | "yum", "git") => vec![vec!["git"]],
        ("dnf" | "yum", "node") => vec![vec!["nodejs", "npm"]],
        ("dnf" | "yum", "python") => vec![vec!["python3", "python3-pip"]],
        ("dnf" | "yum", "java") => vec![vec!["java-21-openjdk-devel"], vec!["java-17-openjdk-devel"]],
        ("dnf" | "yum", "go") => vec![vec!["golang"]],
        ("dnf" | "yum", "rust") => vec![vec!["rust", "cargo"]],
        ("dnf" | "yum", "php") => vec![vec!["php-cli", "php-fpm"], vec!["php-cli"]],
        ("dnf" | "yum", "ruby") => vec![vec!["ruby", "rubygems"]],
        ("dnf" | "yum", "dotnet") => vec![vec!["dotnet-sdk-8.0"]],

        ("apk", "unrar") => vec![vec!["unrar"], vec!["unar"]],
        ("apk", "git") => vec![vec!["git"]],
        ("apk", "node") => vec![vec!["nodejs", "npm"]],
        ("apk", "python") => vec![vec!["python3", "py3-pip"]],
        ("apk", "java") => vec![vec!["openjdk21"], vec!["openjdk17"]],
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
                results.push(ModuleInstallResult {
                    module_id: id,
                    success: false,
                    already_installed: false,
                    message: "Unknown module id".to_string(),
                    output: String::new(),
                });
                continue;
            }
            let st = check_single_module(&id);
            if st.installed {
                results.push(ModuleInstallResult {
                    module_id: id,
                    success: true,
                    already_installed: true,
                    message: format!("Already installed ({})", st.version),
                    output: String::new(),
                });
            } else {
                results.push(ModuleInstallResult {
                    module_id: id,
                    success: false,
                    already_installed: false,
                    message: "Automatic package installation is supported on Linux VPS nodes (apt/dnf/yum/apk/pacman); install manually on Windows dev hosts."
                        .to_string(),
                    output: String::new(),
                });
            }
        }
        return (results, detect_node_modules_sync());
    }

    #[cfg(unix)]
    {
        let use_sudo = !is_effective_root();
        let mut index_refreshed = false;

        for raw_id in module_ids {
            let id = raw_id.trim().to_ascii_lowercase();
            if !catalog.iter().any(|c| c.id == id) {
                results.push(ModuleInstallResult {
                    module_id: id,
                    success: false,
                    already_installed: false,
                    message: "Unknown module id".to_string(),
                    output: String::new(),
                });
                continue;
            }

            let current = check_single_module(&id);
            if current.installed {
                results.push(ModuleInstallResult {
                    module_id: id,
                    success: true,
                    already_installed: true,
                    message: format!("Already installed ({})", current.version),
                    output: String::new(),
                });
                continue;
            }

            if pm == "none" {
                results.push(ModuleInstallResult {
                    module_id: id,
                    success: false,
                    already_installed: false,
                    message: "No supported Linux package manager found (apt-get/dnf/yum/apk/pacman)."
                        .to_string(),
                    output: String::new(),
                });
                continue;
            }

            if !index_refreshed {
                index_refreshed = true;
                match pm.as_str() {
                    "apt-get" => {
                        if let Err(e) = run_pkg_cmd(
                            use_sudo,
                            "apt-get",
                            &["update", "-y"],
                            &[("DEBIAN_FRONTEND", "noninteractive")],
                        ) {
                            results.push(ModuleInstallResult {
                                module_id: id,
                                success: false,
                                already_installed: false,
                                message: "Failed running apt-get update (check sudoers/network)".to_string(),
                                output: e,
                            });
                            continue;
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
                results.push(ModuleInstallResult {
                    module_id: id,
                    success: false,
                    already_installed: false,
                    message: format!("No package mapping for '{id}' on '{pm}'"),
                    output: String::new(),
                });
                continue;
            }

            let mut installed_ok = false;
            let mut last_out = String::new();
            for pkgs in candidates {
                let res = match pm.as_str() {
                    "apt-get" => {
                        let mut args = vec!["install", "-y", "--no-install-recommends"];
                        args.extend_from_slice(&pkgs);
                        run_pkg_cmd(
                            use_sudo,
                            "apt-get",
                            &args,
                            &[("DEBIAN_FRONTEND", "noninteractive")],
                        )
                    }
                    "dnf" => {
                        let mut args = vec!["install", "-y"];
                        args.extend_from_slice(&pkgs);
                        run_pkg_cmd(use_sudo, "dnf", &args, &[])
                    }
                    "yum" => {
                        let mut args = vec!["install", "-y"];
                        args.extend_from_slice(&pkgs);
                        run_pkg_cmd(use_sudo, "yum", &args, &[])
                    }
                    "apk" => {
                        let mut args = vec!["add", "--no-cache"];
                        args.extend_from_slice(&pkgs);
                        run_pkg_cmd(use_sudo, "apk", &args, &[])
                    }
                    "pacman" => {
                        let mut args = vec!["-S", "--noconfirm", "--needed"];
                        args.extend_from_slice(&pkgs);
                        run_pkg_cmd(use_sudo, "pacman", &args, &[])
                    }
                    _ => Err("Unsupported package manager".to_string()),
                };
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

            results.push(ModuleInstallResult {
                module_id: id,
                success,
                already_installed: false,
                message: msg,
                output: last_out,
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
}
