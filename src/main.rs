use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use flate2::write::GzEncoder;
use flate2::Compression;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use tar::Builder;
use tempfile::TempDir;
use walkdir::WalkDir;

/// 简单的备份/还原工具
///
/// 功能（尽量遵从你要求的流程）：
/// - backup: 将当前目录下的 dump.rdb、config/、data/ 以及 plugins/**/(config|data) 打包到一个 tar.gz
///   - 同时扫描 plugins 下是否有 git 仓库，记录每个仓库的 remote URL 和 分支（以及 commit）到 metadata.json
/// - restore: 从指定的 tar.gz 解包，先按 metadata.json 中的信息把 plugins 的仓库 clone 回正确位置并切换到记录的分支，
///   然后把 config/data/dump.rdb 覆盖回当前目录
#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 备份到一个 tar.gz 文件
    Backup {
        /// 输出文件，例如 backup.tar.gz
        #[arg(short, long)]
        out: PathBuf,
    },
    /// 从备份文件还原
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
    match cli.cmd {
        Commands::Backup { out } => backup(out).context("backup failed")?,
        Commands::Restore { input } => restore(input).context("restore failed")?,
    }
    Ok(())
}

fn backup(out: PathBuf) -> Result<()> {
    let cwd = std::env::current_dir()?;
    println!("工作目录: {}", cwd.display());

    let mut repos_info: Vec<GitRepoInfo> = Vec::new();

    // 先创建一个 tar builder 写入到 gzip
    let tar_gz = fs::File::create(&out).with_context(|| format!("无法创建输出文件 {}", out.display()))?;
    let enc = GzEncoder::new(tar_gz, Compression::default());
    let mut tar = Builder::new(enc);

    // helper: 尝试把某个路径（文件或目录）添加到 tar
    let mut add_path = |p: &Path| -> Result<()> {
        if p.exists() {
            if p.is_file() {
                tar.append_path_with_name(p, p.strip_prefix(&cwd)?.to_path_buf())?;
            } else if p.is_dir() {
                // walkdir 把目录下所有文件加进去
                for entry in WalkDir::new(p) {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file() {
                        let name = path.strip_prefix(&cwd)?;
                        tar.append_path_with_name(path, name)?;
                    }
                }
            }
        }
        Ok(())
    };

    // 1) dump.rdb
    let dump = cwd.join("dump.rdb");
    add_path(&dump)?;

    // 2) ./config 和 ./data
    add_path(&cwd.join("config"))?;
    add_path(&cwd.join("data"))?;

    // 3) plugins/**/(config|data) 和扫描 git 仓库
    let plugins_dir = cwd.join("plugins");
    if plugins_dir.exists() && plugins_dir.is_dir() {
        for entry in fs::read_dir(&plugins_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                // 加入 plugins/foo/config 和 plugins/foo/data（如果存在）
                add_path(&path.join("config"))?;
                add_path(&path.join("data"))?;

                // detect git repo inside this plugin folder
                // 如果 path/.git 存在，则尝试打开
                let git_path = path.join(".git");
                if git_path.exists() {
                    if let Ok(repo) = Repository::open(&path) {
                        // remote: try origin first then any
                        let mut remote_url = String::new();
                        if let Ok(remote) = repo.find_remote("origin") {
                            if let Some(url) = remote.url() {
                                remote_url = url.to_string();
                            }
                        }
                        if remote_url.is_empty() {
                            // pick first remote
                            if let Ok(remotes) = repo.remotes() {
                                if let Some(name) = remotes.get(0) {
                                    if let Ok(r) = repo.find_remote(name) {
                                        if let Some(url) = r.url() {
                                            // 立刻变成拥有所有权的 String
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

                        let commit = repo.head().ok().and_then(|h| h.target()).map(|oid| oid.to_string());

                        repos_info.push(GitRepoInfo {
                            path: path
                                .strip_prefix(&cwd)?
                                .to_string_lossy()
                                .replace('\\', "/"),
                            remote: remote_url,
                            branch,
                            commit,
                        });
                    }
                }
            }
        }
    }

    // 写 metadata.json 到一个临时文件，再加入到 tar
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

fn restore(input: PathBuf) -> Result<()> {
    let cwd = std::env::current_dir()?;
    println!("工作目录: {}", cwd.display());

    // 解压到临时目录
    let tmp = TempDir::new()?;
    let tmp_path = tmp.path();

    let tar_gz = fs::File::open(&input).with_context(|| format!("无法打开备份文件 {}", input.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tar_gz));
    archive.unpack(&tmp_path)?;

    // 读取 metadata.json
    let meta_file = tmp_path.join("metadata.json");
    let meta: MetaData = if meta_file.exists() {
        let mut s = String::new();
        fs::File::open(&meta_file)?.read_to_string(&mut s)?;
        serde_json::from_str(&s)?
    } else {
        MetaData { repos: vec![], created_at: "".into() }
    };

    // 1) 按 metadata 先 clone 仓库
    for repo in &meta.repos {
        println!("处理 repo: {} -> {} (branch={})", repo.path, repo.remote, repo.branch);
        if repo.remote.is_empty() {
            println!("  warning: repo {} 没有记录 remote，跳过 clone", repo.path);
            continue;
        }
        let target = cwd.join(&repo.path);
        if target.exists() {
            println!("  目标已存在，先删除：{}", target.display());
            fs::remove_dir_all(&target)?;
        }
        // clone with specified branch
        let mut cb = git2::build::RepoBuilder::new();
        // try to set branch; RepoBuilder::branch expects a branch name
        let rb = if !repo.branch.is_empty() { cb.branch(&repo.branch) } else { &mut cb };
        match rb.clone(&repo.remote, &target) {
            Ok(_) => println!("  clone 完成"),
            Err(e) => println!("  clone 失败: {}", e),
        }
    }

    // 2) 把 tmp 中的 config data dump.rdb 覆盖回 cwd
    // helper copy
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

    // plugins/**/config 和 data
    let tmp_plugins = tmp_path.join("plugins");
    if tmp_plugins.exists() {
        for entry in fs::read_dir(&tmp_plugins)? {
            let entry = entry?;
            let plugin_path = entry.path();
            if plugin_path.is_dir() {
                let rel = plugin_path.strip_prefix(&tmp_plugins)?;
                let dst_plugin = cwd.join("plugins").join(rel);
                copy_dir_all(&plugin_path.join("config"), &dst_plugin.join("config"))?;
                copy_dir_all(&plugin_path.join("data"), &dst_plugin.join("data"))?;
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
