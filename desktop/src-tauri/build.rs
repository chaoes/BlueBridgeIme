// Publish the static frontend the same way `tauri dev` / `tauri build` would
// (frontendDist -> the files the Tauri embedder compiles in), so the CI can build
// the GUI binary from a plain `cargo build` without installing the Tauri CLI.
//
// The Tauri v2 CLI resolves `build.frontendDist` relative to this directory,
// copies it into the target directory and passes it through
// TAURI_ENV_FRONTEND_DIST (opaque per-target value), so the eager embedding in
// tauri::generate_context! does not have to re-resolve the path at compile time.
//
// 另外这里负责生成 Windows 安装器（NSIS）的模板：把 tauri-bundler 自带的 installer.nsi
// 与仓库里的补丁片段 gen/nsis/*.patch.nsi 合并成 generated-nsis/installer.nsi，
// 由 tauri.conf.json 的 bundle.windows.nsis.template 引用（见 patch_nsis 的说明）。

use std::{env, fs, path::Path, process};

const FRONTEND_DIST: &str = "../web";
const DIST_DIR: &str = "generated-dist";

const NSIS_PATCHES: [&str; 2] = [
    "gen/nsis/directory-page.patch.nsi",
    "gen/nsis/language-selector.patch.nsi",
];
const NSIS_TEMPLATE: &str = "generated-nsis/installer.nsi";

/// 打包时 tauri 需要读、但归属清单里没有的目录（相对 src-tauri）。
const BUNDLE_INPUT_DIRS: [&str; 2] = ["icons", "gen"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={FRONTEND_DIST}");
    for patch in NSIS_PATCHES {
        println!("cargo:rerun-if-changed={patch}");
    }

    patch_nsis();
    stage_bundle_inputs();

    // An explicit value comes from the Tauri CLI (`tauri build` / `tauri bundle`), which
    // runs its own frontend copy step; leaving it untouched keeps that path working.
    if let Ok(existing) = env::var("TAURI_ENV_FRONTEND_DIST") {
        if !existing.is_empty() {
            tauri_build::build();
            return;
        }
    }

    let dist = Path::new(DIST_DIR);
    if let Err(error) = copy_frontend(Path::new(FRONTEND_DIST), dist) {
        eprintln!("复制前端资源失败（{FRONTEND_DIST} -> {DIST_DIR}）：{error}");
        process::exit(1);
    }

    // SAFETY: build scripts are single threaded; nothing else reads the environment
    // while the build script runs.
    unsafe { env::set_var("TAURI_ENV_FRONTEND_DIST", "../../generated-dist") };

    tauri_build::build();
}

/// 把 tauri.conf.json 引用、但不在归属清单里的资源文件复制到
/// `$CARGO_TARGET_DIR/bundle/resources/` 下，让 `cargo tauri bundle` 能找到它们。
///
/// 为什么需要：`tauri bundle` 会先 `set_current_dir(tauri_dir)`（即 desktop/src-tauri），
/// 再把 `tauri.conf.json` 里的相对路径按 **执行 cargo 时的工作目录** 解析——从 desktop/ 下调用时
/// `icons/icon.ico` 指的是 desktop/icons/icon.ico（不存在，文件在 src-tauri/icons/ 下），
/// 于是只报一句 “No such file or directory (os error 2)”。CI 在 Linux 上交叉打 NSIS 包正是这种情况
/// （Windows 上由 tauri-cli 自己的流程兜住了）。
///
/// tauri 打包时把资源收集到 `<target>/bundle/resources/`，相对路径逐段拼在它后面读取；
/// 这里按同样的方式提前放好，打包一侧就能直接读到。
///
/// 目录以当前仓库布局为准：tauri.conf.json 与它引用的 `icons/`、`gen/` 同在 src-tauri 下，
/// 所以这里直接用清单头目录里的这两个目录。
fn stage_bundle_inputs() {
    let Ok(target_dir) = env::var("CARGO_TARGET_DIR") else {
        return;
    };
    if target_dir.is_empty() {
        return;
    }
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let manifest_dir = Path::new(&manifest_dir);
    let staging = if Path::new(&target_dir).is_absolute() {
        Path::new(&target_dir).join("bundle").join("resources")
    } else {
        // cargo 把相对路径的 CARGO_TARGET_DIR 当作相对于调用时的工作目录。
        env::current_dir()
            .unwrap_or_default()
            .join(&target_dir)
            .join("bundle")
            .join("resources")
    };

    for entry in BUNDLE_INPUT_DIRS {
        let source = manifest_dir.join(entry);
        if !source.is_dir() {
            continue;
        }
        if let Err(error) = copy_frontend(&source, &staging.join(entry)) {
            eprintln!("复制打包输入 {entry} 失败：{error}");
        }
    }
}

/// 生成自定义的 NSIS 模板。
///
/// 为什么不直接在 tauri.conf.json 里放完整的模板文件：完整模板有 1000 行，复制进仓库后
/// 会随 tauri 升级静默过时（tauri 4 年改了 30 多版），而且模板里的 `{{...}}` 占位符一旦漏进
/// 复制版就会以字面量显示给用户。
///
/// 这里的做法是“先读上游、再打补丁”：
///   1. 找到 tauri-bundler 源码树里的 installer.nsi（版本由 Cargo.lock 决定，不会漂移）；
///   2. 把每个 `gen/nsis/*.patch.nsi` 里的锚点行替换成补丁内容；
///   3. 锚点不存在（tauri 改了模板）时，警告后退回上游原文——安装包照常能出，只是少了定制文案。
///
/// 每个补丁文件头都写明了它期待的上游原文，便于 tauri 升级后核对。
fn patch_nsis() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let manifest_dir = Path::new(&manifest_dir);
    let destination = manifest_dir.join(NSIS_TEMPLATE);
    let Some(upstream) = find_upstream_template() else {
        eprintln!("未找到 tauri-bundler 的 NSIS 模板，安装器将使用 tauri 默认向导（不影响构建）");
        return;
    };

    let template = match fs::read_to_string(&upstream) {
        Ok(template) => template,
        Err(error) => {
            eprintln!("读取 {} 失败：{error}", upstream.display());
            return;
        }
    };
    let mut patched = template.clone();
    for patch in NSIS_PATCHES {
        let text = match fs::read_to_string(manifest_dir.join(patch)) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("读取 {patch} 失败：{error}");
                continue;
            }
        };
        let Some((anchor, replacement)) = patch_parts(&text) else {
            eprintln!("{patch} 缺少 === ANCHOR === / === REPLACE === 标记，已跳过");
            continue;
        };
        if !patched.contains(&anchor) {
            eprintln!(
                "警告：tauri 模板里找不到 {patch} 的锚点，该项定制失效（请对照该文件头注释更新）"
            );
            continue;
        }
        patched = patched.replacen(&anchor, &replacement, 1);
    }

    if let Some(parent) = destination.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            eprintln!("创建 generated-nsis 目录失败：{error}");
            return;
        }
    }
    let header = format!(
        "; 由 build.rs 从 {} + {NSIS_PATCHES:?} 生成，请改 gen/nsis 下的补丁文件\n",
        upstream.display()
    );
    if let Err(error) = fs::write(&destination, header + &patched) {
        eprintln!("写入 {NSIS_TEMPLATE} 失败：{error}");
    }
}

/// 取出补丁文件里的锚点原文与替换内容。
///
/// 补丁文件格式（见 gen/nsis/*.patch.nsi）：
///   === ANCHOR ===   后面若干行 = 上游模板里要被替换掉的原文
///   === REPLACE ===  后面若干行 = 替换成的内容
/// 锚点按 NSIS 原文逐行比对：上游是 LF 换行，所以这里先统一成 LF。
fn patch_parts(text: &str) -> Option<(String, String)> {
    const ANCHOR: &str = "=== ANCHOR ===";
    const REPLACE: &str = "=== REPLACE ===";
    let body = text.split_once(ANCHOR)?.1;
    let (anchor, replacement) = body.split_once(REPLACE)?;
    Some((anchor.trim().to_string(), replacement.trim().to_string()))
}

/// 在 `$CARGO_HOME` 的注册表源码目录里找 tauri-bundler 的 installer.nsi。
///
/// 目录布局是 `<registry>/<镜像名>/tauri-bundler-<版本>/src/bundle/windows/nsis/`，
/// 镜像名会随 Cargo 配置变化，所以逐个镜像目录去找，不写死。
fn find_upstream_template() -> Option<std::path::PathBuf> {
    let cargo_home = env::var("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| env::var("USERPROFILE").map(|p| Path::new(&p).join(".cargo")))
        .ok()?;
    let mut roots = vec![cargo_home.join("registry").join("src")];
    // 允许显式指定 tauri-bundler 源码目录（非标准布局或想锁定某个版本时用）。
    if let Ok(explicit) = env::var("BIB_TAURI_BUNDLER_SRC") {
        roots.insert(0, std::path::PathBuf::from(explicit));
    }
    for root in roots {
        let Ok(mirrors) = fs::read_dir(&root) else {
            continue;
        };
        for mirror in mirrors.flatten() {
            let Ok(entries) = fs::read_dir(mirror.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with("tauri-bundler-") {
                    continue;
                }
                let candidate = entry.path().join("src/bundle/windows/nsis/installer.nsi");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn copy_frontend(source: &Path, target: &Path) -> std::io::Result<()> {
    if target.exists() {
        fs::remove_dir_all(target)?;
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if from.is_dir() {
            copy_frontend(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
