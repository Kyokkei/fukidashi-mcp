use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/fukidashi.ico");
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-env-changed=WindowsSdkDir");
    println!("cargo:rerun-if-env-changed=WindowsSDKVersion");

    if env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("windows")
        || env::var_os("CARGO_FEATURE_EDITOR").is_none()
    {
        return;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let output_dir = PathBuf::from(env::var_os("OUT_DIR").expect("build output dir"));
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let Some(compiler) = resource_compiler(&target_env, &target_arch) else {
        println!(
            "cargo:warning=Windows resource compiler not found; the editor still sets its runtime window icon"
        );
        return;
    };

    let icon = manifest_dir.join("assets").join("fukidashi.ico");
    let resource_script = output_dir.join("fukidashi-icon.rc");
    let resource = output_dir.join("fukidashi-icon.res");
    let icon_path = icon
        .canonicalize()
        .unwrap_or(icon)
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    fs::write(&resource_script, format!("1 ICON \"{icon_path}\"\n"))
        .expect("write editor icon resource script");

    let status = if target_env == "msvc" {
        Command::new(&compiler)
            .arg("/nologo")
            .arg("/fo")
            .arg(&resource)
            .arg(&resource_script)
            .current_dir(&manifest_dir)
            .status()
    } else {
        Command::new(&compiler)
            .arg(&resource_script)
            .arg("-O")
            .arg("coff")
            .arg("-o")
            .arg(&resource)
            .current_dir(&manifest_dir)
            .status()
    }
    .expect("run Windows resource compiler");
    if !status.success() {
        panic!("failed to embed the Fukidashi Editor icon resource");
    }

    println!(
        "cargo:rustc-link-arg-bin=fukidashi-editor={}",
        resource.display()
    );
}

fn resource_compiler(target_env: &str, target_arch: &str) -> Option<PathBuf> {
    let name = if target_env == "msvc" {
        "rc.exe"
    } else {
        "windres.exe"
    };
    if let Some(override_path) = env::var_os("RC").map(PathBuf::from)
        && override_path.is_file()
    {
        return Some(override_path);
    }
    if let Some(from_path) = env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
    {
        return Some(from_path);
    }
    if target_env != "msvc" {
        return None;
    }

    let sdk_arch = match target_arch {
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => "x64",
    };
    let mut roots = Vec::new();
    for variable in ["WindowsSdkDir", "WindowsSDKDir"] {
        if let Some(value) = env::var_os(variable) {
            roots.push(PathBuf::from(value));
        }
    }
    if let Some(program_files) = env::var_os("ProgramFiles(x86)") {
        roots.push(PathBuf::from(program_files).join("Windows Kits").join("10"));
    }
    if let Some(program_files) = env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(program_files).join("Windows Kits").join("10"));
    }

    for root in roots {
        if let Some(version) = env::var_os("WindowsSDKVersion") {
            let versioned = root
                .join("bin")
                .join(
                    version
                        .to_string_lossy()
                        .trim_matches(|ch| ch == '\\' || ch == '/'),
                )
                .join(sdk_arch)
                .join(name);
            if versioned.is_file() {
                return Some(versioned);
            }
        }
        if let Ok(bin) = root.join("bin").read_dir() {
            let mut versions = bin
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>();
            versions.sort();
            for version in versions.into_iter().rev() {
                let candidate = version.join(sdk_arch).join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}
