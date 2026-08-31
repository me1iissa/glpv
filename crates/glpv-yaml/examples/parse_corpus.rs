fn main() {
    let repo = std::path::Path::new("corpus/gitlab");
    let out = std::process::Command::new("git")
        .args([
            "-C",
            "corpus/gitlab",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ])
        .output()
        .unwrap();
    let _ = repo;
    for f in String::from_utf8_lossy(&out.stdout).lines() {
        if !(f.ends_with(".yml") || f.ends_with(".yaml")) || !f.starts_with(".gitlab") {
            continue;
        }
        let show = std::process::Command::new("git")
            .args(["-C", "corpus/gitlab", "show", &format!("HEAD:{f}")])
            .output()
            .unwrap();
        if !show.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&show.stdout);
        if let Err(e) = glpv_yaml::parse(glpv_yaml::FileId(0), &text) {
            println!("FAIL {f}: {e}");
        }
    }
}
