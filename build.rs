fn main() {
    // Exact source identity is optional for source archives. Never infer an
    // image digest from a tag, path or environment at collection time.
    let repository_root = std::process::Command::new("git")
        .args(["rev-parse", "--show-prefix"])
        .output()
        .is_ok_and(|v| v.status.success() && v.stdout.iter().all(u8::is_ascii_whitespace));
    if repository_root
        && let Ok(output) = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
        && output.status.success()
        && let Ok(revision) = String::from_utf8(output.stdout)
    {
        let revision = revision.trim();
        if revision.len() == 40 && revision.chars().all(|c| c.is_ascii_hexdigit()) {
            println!("cargo:rustc-env=KEI_BUILD_REVISION={revision}");
            let dirty = std::process::Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=normal"])
                .output()
                .map(|v| !v.status.success() || !v.stdout.is_empty())
                .unwrap_or(true);
            println!("cargo:rustc-env=KEI_BUILD_DIRTY={dirty}");
        }
    }
    if let Ok(output) = std::process::Command::new("git")
        .args(["ls-files"])
        .output()
        && output.status.success()
        && let Ok(paths) = String::from_utf8(output.stdout)
    {
        for path in paths.lines() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-changed=src");
    // Worktree HEAD and the active branch ref can change without source edits.
    let mut watched_refs = vec!["HEAD".to_string(), "index".to_string()];
    if let Ok(output) = std::process::Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .output()
        && output.status.success()
        && let Ok(reference) = String::from_utf8(output.stdout)
    {
        watched_refs.push(reference.trim().to_string());
    }
    for reference in watched_refs {
        let args = ["rev-parse", "--git-path", reference.as_str()];
        if let Ok(output) = std::process::Command::new("git").args(args).output()
            && let Ok(path) = String::from_utf8(output.stdout)
        {
            println!("cargo:rerun-if-changed={}", path.trim());
        }
    }

    // Increase the Windows debug-build stack reserve. Large clap/config
    // construction frames fit within Linux's larger default stack, but can
    // exceed Windows' 1 MiB default when the real binary is exercised by
    // assert_cmd in CI.
    //
    // Keep this in build.rs instead of .cargo/config.toml so CI's global
    // RUSTFLAGS="-Dwarnings" cannot replace the linker setting.
    #[cfg(windows)]
    {
        println!("cargo:rustc-link-arg=/STACK:4194304");
    }
}
