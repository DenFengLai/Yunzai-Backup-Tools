use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use flate2::Compression;
use flate2::write::GzEncoder;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use tar::Builder;
use tempfile::TempDir;
use walkdir::WalkDir;

/// 简单的备份/还原工具
///
/// 功能：
/// - backup: 将工作目录下的 dump.rdb、config/、data/ 以及 plugins/**/(config|data) 打包到一个 tar.gz
///   - 同时扫描 plugins 下是否有 git 仓库，记录每个仓库的 remote URL 和 分支（以及 commit）到 metadata.json
/// - restore: 从指定的 tar.gz 解包，按 metadata.json 的信息 clone 仓库并恢复文件
#[derive(Parser)]
#[command(
    author,
    version,
    about,
    long_about = "示例:\n  your_program backup\n  your_program -w /path backup -o mybackup.tar.gz --example-js-only\n  your_program restore -i mybackup.tar.gz"
)]
struct Cli {
    /// 指定工作目录（默认当前运行目录）
    #[arg(short = 'w', long, global = true, value_name = "PATH")]
    workdir: Option<PathBuf>,

    /// 隐藏的 workdir 短选项 -C（行为等同 -w，但不会出现在 help 中）
    #[arg(short = 'C', global = true, hide = true, value_name = "PATH")]
    workdir_hidden: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 备份到一个 tar.gz 文件
    #[command(alias = "b")]
    Backup {
        /// 输出文件，例如 backup.tar.gz（默认 backup.tar.gz）
        #[arg(short, long, default_value = "backup.tar.gz")]
        out: PathBuf,

        /// 仅备份 plugins/example 下的 .js/.js.bak 与 package.json（跳过 node_modules）
        #[arg(short = 'j', long)]
        example_js_only: bool,
    },

    /// 从备份文件还原
    #[command(alias = "r")]
    Restore {
        /// 输入的备份文件
        #[arg(short, long)]
        input: PathBuf,
    },
}

#[derive(Serialize, Deserialize, Debug)]
struct GitRepoInfo {
    /// 相对于工作目录的路径，比如 plugins/foo
    path: String,
    remote: String,
    branch: String,
    commit: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct MetaData {
    repos: Vec<GitRepoInfo>,
    created_at: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // 处理工作目录优先级：visible -w > hidden -C > current_dir
    let cwd = if let Some(wd) = cli.workdir {
        wd
    } else if let Some(wd2) = cli.workdir_hidden {
        wd2
    } else {
        std::env::current_dir()?
    };

    match cli.cmd {
        Commands::Backup {
            out,
            example_js_only,
        } => backup(&cwd, out, example_js_only).context("backup failed")?,
        Commands::Restore { input } => restore(&cwd, input).context("restore failed")?,
    }
    Ok(())
}

fn backup(cwd: &Path, out: PathBuf, example_js_only: bool) -> Result<()> {
    println!("工作目录: {}", cwd.display());

    // 先扫描并收集要加入归档的文件路径（绝对或相对于 cwd 的 PathBuf），同时收集 repos_info
    let mut repos_info: Vec<GitRepoInfo> = Vec::new();
    let mut files_to_add: Vec<PathBuf> = Vec::new();
    let mut rel_seen: HashSet<String> = HashSet::new(); // 防重复，存相对路径字符串

    // helper: 把单个文件加入集合（以 cwd 相对路径去重）
    fn push_file(
        p: &Path,
        files_to_add: &mut Vec<PathBuf>,
        rel_seen: &mut HashSet<String>,
        cwd: &Path,
    ) -> Result<()> {
        if p.exists() && p.is_file() {
            let rel = p.strip_prefix(cwd)?.to_string_lossy().replace('\\', "/");
            if !rel_seen.contains(&rel) {
                rel_seen.insert(rel);
                files_to_add.push(p.to_path_buf());
            }
        }
        Ok(())
    }

    // helper: 收集目录下所有文件（不跳过 node_modules）
    fn collect_dir_all(
        dir: &Path,
        files_to_add: &mut Vec<PathBuf>,
        rel_seen: &mut HashSet<String>,
        cwd: &Path,
    ) -> Result<()> {
        if dir.exists() && dir.is_dir() {
            for e in WalkDir::new(dir) {
                let e = e?;
                let p = e.path();
                if p.is_file() {
                    push_file(p, files_to_add, rel_seen, cwd)?;
                }
            }
        }
        Ok(())
    }

    // helper: 收集目录下所有文件但跳过 node_modules（用于 plugins/example）
    fn collect_dir_skip_node_modules(
        dir: &Path,
        files_to_add: &mut Vec<PathBuf>,
        rel_seen: &mut HashSet<String>,
        cwd: &Path,
    ) -> Result<()> {
        if dir.exists() && dir.is_dir() {
            for e in WalkDir::new(dir) {
                let e = e?;
                let p = e.path();
                let skip = p
                    .components()
                    .any(|c| c.as_os_str() == OsStr::new("node_modules"));
                if skip {
                    continue;
                }
                if p.is_file() {
                    push_file(p, files_to_add, rel_seen, cwd)?;
                }
            }
        }
        Ok(())
    }

    // 1) dump.rdb
    let dump = cwd.join("dump.rdb");
    push_file(&dump, &mut files_to_add, &mut rel_seen, cwd)?;

    // 2) ./config 和 ./data（完整收集）
    collect_dir_all(&cwd.join("config"), &mut files_to_add, &mut rel_seen, cwd)?;
    collect_dir_all(&cwd.join("data"), &mut files_to_add, &mut rel_seen, cwd)?;

    // 3) plugins/**/(config|data) 和扫描 git 仓库，并且处理 plugins/example 的策略
    let plugins_dir = cwd.join("plugins");
    if plugins_dir.exists() && plugins_dir.is_dir() {
        for entry in fs::read_dir(&plugins_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                // 收集 plugins/foo/config 和 plugins/foo/data（如果存在）
                collect_dir_all(&path.join("config"), &mut files_to_add, &mut rel_seen, cwd)?;
                collect_dir_all(&path.join("data"), &mut files_to_add, &mut rel_seen, cwd)?;

                // detect git repo inside this plugin folder
                let git_path = path.join(".git");
                if git_path.exists() {
                    if let Ok(repo) = Repository::open(&path) {
                        let mut remote_url = String::new();
                        if let Ok(remote) = repo.find_remote("origin") {
                            if let Some(url) = remote.url() {
                                remote_url = url.to_string();
                            }
                        }
                        if remote_url.is_empty() {
                            if let Ok(remotes) = repo.remotes() {
                                if let Some(name) = remotes.get(0) {
                                    if let Ok(r) = repo.find_remote(name) {
                                        if let Some(url) = r.url() {
                                            remote_url = url.to_string();
                                        }
                                    }
                                }
                            }
                        }

                        let mut branch = String::from("HEAD");
                        if let Ok(head) = repo.head() {
                            if let Some(sh) = head.shorthand() {
                                branch = sh.to_string();
                            }
                        }

                        let commit = repo
                            .head()
                            .ok()
                            .and_then(|h| h.target())
                            .map(|oid| oid.to_string());

                        repos_info.push(GitRepoInfo {
                            path: path.strip_prefix(cwd)?.to_string_lossy().replace('\\', "/"),
                            remote: remote_url,
                            branch,
                            commit,
                        });
                    }
                }

                // 专门处理 plugins/example 的收集策略
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if name == "example" {
                        if example_js_only {
                            // 仅收集 .js / .js.bak / package.json（跳过 node_modules）
                            if path.exists() {
                                for e in WalkDir::new(&path) {
                                    let e = e?;
                                    let p = e.path();
                                    let skip = p
                                        .components()
                                        .any(|c| c.as_os_str() == OsStr::new("node_modules"));
                                    if skip {
                                        continue;
                                    }
                                    if p.is_file() {
                                        let fname =
                                            p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                                        if fname.ends_with(".js")
                                            || fname.ends_with(".js.bak")
                                            || fname == "package.json"
                                        {
                                            push_file(p, &mut files_to_add, &mut rel_seen, cwd)?;
                                        }
                                    }
                                }
                            }
                        } else {
                            // 收集 example 下的全部文件（但跳过 node_modules）
                            collect_dir_skip_node_modules(
                                &path,
                                &mut files_to_add,
                                &mut rel_seen,
                                cwd,
                            )?;
                        }
                    }
                }
            }
        }
    }

    // 如果既没有要打包的文件，也没有检测到任何 git 仓库，则认为运行目录不符合预期，异常退出且不生成压缩包
    if files_to_add.is_empty() && repos_info.is_empty() {
        bail!(
            "工作目录 '{}' 下未发现可备份的文件或仓库，已取消备份",
            cwd.display()
        );
    }

    // 创建 tar.gz 并把收集到的文件写入
    let tar_gz =
        fs::File::create(&out).with_context(|| format!("无法创建输出文件 {}", out.display()))?;
    let enc = GzEncoder::new(tar_gz, Compression::default());
    let mut tar = Builder::new(enc);

    for p in &files_to_add {
        let name_in_tar = p.strip_prefix(cwd)?;
        tar.append_path_with_name(p, name_in_tar)?;
    }

    // 写 metadata.json 到 tar（即使 repos_info 为空，也写入）
    let meta = MetaData {
        repos: repos_info,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let meta_json = serde_json::to_vec_pretty(&meta)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(meta_json.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "metadata.json", &meta_json[..])?;

    // finish
    tar.into_inner()?.finish()?;

    println!("已写入备份：{}", out.display());
    Ok(())
}

fn restore(cwd: &Path, input: PathBuf) -> Result<()> {
    println!("工作目录: {}", cwd.display());

    // 解压到临时目录
    let tmp = TempDir::new()?;
    let tmp_path = tmp.path();

    let tar_gz =
        fs::File::open(&input).with_context(|| format!("无法打开备份文件 {}", input.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tar_gz));
    archive.unpack(&tmp_path)?;

    // 读取 metadata.json（如果有）
    let meta_file = tmp_path.join("metadata.json");
    let meta: MetaData = if meta_file.exists() {
        let mut s = String::new();
        fs::File::open(&meta_file)?.read_to_string(&mut s)?;
        serde_json::from_str(&s)?
    } else {
        MetaData {
            repos: vec![],
            created_at: "".into(),
        }
    };

    // 1) 按 metadata 先 clone 仓库
    for repo in &meta.repos {
        println!(
            "处理 repo: {} -> {} (branch={})",
            repo.path, repo.remote, repo.branch
        );
        if repo.remote.is_empty() {
            println!("  warning: repo {} 没有记录 remote，跳过 clone", repo.path);
            continue;
        }
        let target = cwd.join(&repo.path);
        if target.exists() {
            println!("  目标已存在，先删除：{}", target.display());
            fs::remove_dir_all(&target)?;
        }
        let mut cb = git2::build::RepoBuilder::new();
        let rb = if !repo.branch.is_empty() {
            cb.branch(&repo.branch)
        } else {
            &mut cb
        };
        match rb.clone(&repo.remote, &target) {
            Ok(_) => println!("  clone 完成"),
            Err(e) => println!("  clone 失败: {}", e),
        }
    }

    // helper: 目录复制（递归）
    fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
        if !src.exists() {
            return Ok(());
        }
        fs::create_dir_all(dst)?;
        for entry in WalkDir::new(src) {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(src)?;
            let dest_path = dst.join(rel);
            if path.is_dir() {
                fs::create_dir_all(&dest_path)?;
            } else if path.is_file() {
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(path, &dest_path)?;
            }
        }
        Ok(())
    }

    // config
    let tmp_config = tmp_path.join("config");
    copy_dir_all(&tmp_config, &cwd.join("config"))?;
    // data
    let tmp_data = tmp_path.join("data");
    copy_dir_all(&tmp_data, &cwd.join("data"))?;

    // plugins/**/config 和 data，以及恢复 example（归档里有什么就恢复什么）
    let tmp_plugins = tmp_path.join("plugins");
    if tmp_plugins.exists() {
        for entry in fs::read_dir(&tmp_plugins)? {
            let entry = entry?;
            let plugin_path = entry.path();
            if plugin_path.is_dir() {
                let rel = plugin_path.strip_prefix(&tmp_plugins)?;
                let dst_plugin = cwd.join("plugins").join(rel);
                // 先还原 config 与 data（如果存在）
                copy_dir_all(&plugin_path.join("config"), &dst_plugin.join("config"))?;
                copy_dir_all(&plugin_path.join("data"), &dst_plugin.join("data"))?;

                // 对 example 插件：把归档中实际包含的文件恢复（无论是完整备份还是仅 js）
                if let Some(name) = plugin_path.file_name().and_then(|s| s.to_str()) {
                    if name == "example" {
                        fs::create_dir_all(&dst_plugin)?;
                        for e in WalkDir::new(&plugin_path) {
                            let e = e?;
                            let p = e.path();

                            // 跳过 node_modules（备份时已跳过）
                            let has_node_modules = p
                                .components()
                                .any(|c| c.as_os_str() == OsStr::new("node_modules"));
                            if has_node_modules {
                                continue;
                            }

                            if p.is_file() {
                                let relp = p.strip_prefix(&plugin_path)?;
                                let dest = dst_plugin.join(relp);
                                if let Some(parent) = dest.parent() {
                                    fs::create_dir_all(parent)?;
                                }
                                fs::copy(p, &dest)?;
                            }
                        }
                    }
                }
            }
        }
    }

    // dump.rdb
    let tmp_dump = tmp_path.join("dump.rdb");
    if tmp_dump.exists() {
        fs::copy(&tmp_dump, &cwd.join("dump.rdb"))?;
        println!("dump.rdb 已恢复到 {}", cwd.join("dump.rdb").display());
    }

    println!("还原完成");
    Ok(())
}
